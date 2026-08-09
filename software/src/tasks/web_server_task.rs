//! Wi-Fi station and HTTP server exposing the newest sensor readings.
//!
//! The board joins an existing Wi-Fi network as a client (station mode) and
//! serves a small web application on TCP port 80. Three tasks cooperate:
//!
//! * `wifi_connection_task` keeps the radio associated with the access point.
//! * `net_task` runs the embassy-net TCP/IP stack.
//! * `web_server_task` accepts connections and answers HTTP requests.
//!
//! Three resources are served, all of them read-only:
//!
//! * `GET /` returns the page (HTML, CSS and JavaScript in one file, stored in
//!   flash).
//! * `GET /api/status` returns the device uptime and the state of both
//!   histories.
//! * `GET /api/readings` returns one page of a single sensor's readings.
//!
//! The page fetches the retained history once, a page at a time, keeps it in
//! the browser, and afterwards asks only for the readings taken since the last
//! one it holds. `README.md` documents both endpoints in full.

use core::fmt::Write as _;

use embassy_net::tcp::TcpSocket;
use embassy_net::{Stack, StackResources};
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::Write as _;
use esp_println::println;
use esp_wifi::wifi::{
    ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent, WifiStaDevice,
    WifiState,
};
use heapless::String;

use crate::utils::shared_state;

/// Network credentials, supplied at build time via environment variables.
///
/// Example: `WIFI_SSID="my-net" WIFI_PASSWORD="secret" cargo build --release`.
const SSID: &str = match option_env!("WIFI_SSID") {
    Some(value) => value,
    None => "",
};
const PASSWORD: &str = match option_env!("WIFI_PASSWORD") {
    Some(value) => value,
    None => "",
};

/// TCP port the HTTP server listens on.
const HTTP_PORT: u16 = 80;
/// The single page application, compiled into the firmware image.
const INDEX_HTML: &str = include_str!("web/index.html");
/// Number of sockets embassy-net may keep open at once.
pub const SOCKET_COUNT: usize = 3;
/// Idle time before retrying after a failed association attempt.
const RECONNECT_DELAY_MS: u64 = 5000;
/// Close an idle client connection after this long without traffic.
const SOCKET_TIMEOUT_S: u64 = 10;
/// Largest number of readings one `/api/readings` request may return.
///
/// The retained history holds a day of readings, which is too much for a
/// single response, so a client fetches it as a handful of pages of this size
/// and then only asks for the readings taken since its last page.
const MAX_PAGE_READINGS: usize = 2000;

/// Receive buffer for one client connection.
static mut RX_BUFFER: [u8; 1536] = [0; 1536];
/// Transmit buffer for one client connection.
static mut TX_BUFFER: [u8; 1536] = [0; 1536];

/// Type alias for the resources embassy-net needs for the whole program run.
pub type WifiStackResources = StackResources<SOCKET_COUNT>;
/// Type alias for the station-mode network stack.
pub type WifiStack = Stack<WifiDevice<'static, WifiStaDevice>>;

/// Keep the station associated with the configured access point.
///
/// On disconnection the task waits briefly and retries forever, so a router
/// reboot does not require a device reset.
#[embassy_executor::task]
pub async fn wifi_connection_task(mut controller: WifiController<'static>) {
    if SSID.is_empty() {
        println!("Wi-Fi: WIFI_SSID not set at build time, web server disabled");
        return;
    }

    loop {
        if esp_wifi::wifi::get_wifi_state() == WifiState::StaConnected {
            // Block here until the connection is lost, then fall through and
            // reconnect below.
            controller.wait_for_event(WifiEvent::StaDisconnected).await;
            Timer::after_millis(RECONNECT_DELAY_MS).await;
        }

        if !matches!(controller.is_started(), Ok(true)) {
            let configuration = Configuration::Client(ClientConfiguration {
                ssid: SSID.try_into().unwrap(),
                password: PASSWORD.try_into().unwrap(),
                ..Default::default()
            });

            if let Err(error) = controller.set_configuration(&configuration) {
                println!("Wi-Fi: configuration rejected: {:?}", error);
                Timer::after_millis(RECONNECT_DELAY_MS).await;
                continue;
            }

            if let Err(error) = controller.start().await {
                println!("Wi-Fi: could not start radio: {:?}", error);
                Timer::after_millis(RECONNECT_DELAY_MS).await;
                continue;
            }

            println!("Wi-Fi: radio started, connecting to \"{}\"", SSID);
        }

        match controller.connect().await {
            Ok(()) => println!("Wi-Fi: associated with \"{}\"", SSID),
            Err(error) => {
                println!("Wi-Fi: association failed: {:?}", error);
                Timer::after_millis(RECONNECT_DELAY_MS).await;
            }
        }
    }
}

/// Drive the embassy-net stack; it does no work unless this task runs.
#[embassy_executor::task]
pub async fn net_task(stack: &'static WifiStack) {
    stack.run().await
}

/// Serve the measurement page over HTTP.
///
/// Only one connection is handled at a time, which is sufficient for a page
/// that a person loads manually and keeps the memory footprint small.
#[embassy_executor::task]
pub async fn web_server_task(stack: &'static WifiStack) {
    // Wait for the link and then for the DHCP lease before binding.
    while !stack.is_link_up() {
        Timer::after_millis(500).await;
    }

    loop {
        if let Some(config) = stack.config_v4() {
            println!("Web server: http://{}/", config.address.address());
            break;
        }
        Timer::after_millis(500).await;
    }

    // SAFETY: this task is spawned once, so it is the only user of these
    // buffers, and the references never outlive the task.
    let (rx_buffer, tx_buffer) = unsafe {
        (
            &mut *core::ptr::addr_of_mut!(RX_BUFFER),
            &mut *core::ptr::addr_of_mut!(TX_BUFFER),
        )
    };
    let mut socket = TcpSocket::new(stack, rx_buffer, tx_buffer);
    socket.set_timeout(Some(embassy_time::Duration::from_secs(SOCKET_TIMEOUT_S)));

    loop {
        if let Err(error) = socket.accept(HTTP_PORT).await {
            println!("Web server: accept failed: {:?}", error);
            socket.abort();
            let _ = socket.flush().await;
            Timer::after(Duration::from_millis(500)).await;
            continue;
        }

        serve_connection(&mut socket).await;

        socket.close();
        let _ = socket.flush().await;
        socket.abort();
        let _ = socket.flush().await;
    }
}

/// Read one HTTP request from `socket` and write the response.
async fn serve_connection(socket: &mut TcpSocket<'_>) {
    let mut request = [0u8; 512];
    let mut filled = 0;

    // Read until the end of the header block; the request has no body.
    loop {
        match socket.read(&mut request[filled..]).await {
            Ok(0) => return,
            Ok(count) => {
                filled += count;
                if request[..filled].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if filled == request.len() {
                    // Header block too large; treat as a bad request.
                    let _ = socket
                        .write_all(b"HTTP/1.1 431 Request Header Fields Too Large\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                }
            }
            Err(error) => {
                println!("Web server: read failed: {:?}", error);
                return;
            }
        }
    }

    let head = core::str::from_utf8(&request[..filled]).unwrap_or("");
    let mut parts = head.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");

    if method != "GET" {
        let _ = socket
            .write_all(
                b"HTTP/1.1 405 Method Not Allowed\r\nAllow: GET\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            )
            .await;
        return;
    }

    // Split `/api/readings?sensor=scd41&from=0` into path and query string.
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    };

    match path {
        "/" | "/index.html" => send_page(socket).await,
        "/api/status" => send_status(socket).await,
        "/api/readings" => send_readings(socket, query).await,
        _ => {
            let _ = socket
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                )
                .await;
        }
    }
}

/// Value of `name` in a `key=value&key=value` query string.
///
/// Percent-escapes are not decoded: every parameter this server accepts is a
/// bare number or one of a fixed set of sensor names, none of which can
/// contain a character that needs escaping.
fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })
}

/// Send the page itself, straight from flash.
async fn send_page(socket: &mut TcpSocket<'_>) {
    send_response(socket, "text/html; charset=utf-8", INDEX_HTML).await;
}

/// Write a complete `200 OK` response with `body` as its payload.
async fn send_response(socket: &mut TcpSocket<'_>, content_type: &str, body: &str) {
    let mut header: String<192> = String::new();
    let _ = write!(
        header,
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        content_type,
        body.len()
    );

    if socket.write_all(header.as_bytes()).await.is_ok() {
        let _ = socket.write_all(body.as_bytes()).await;
    }
}

/// Describe the device and the state of both histories.
///
/// This is the endpoint a client polls. It is small and of known length, and
/// it tells the client which sequence numbers are still available, so the
/// client can work out exactly which readings it is missing before asking for
/// any of them.
async fn send_status(socket: &mut TcpSocket<'_>) {
    let uptime_ms = Instant::now().as_millis();
    let scd41 = shared_state::scd41_status().await;
    let sps30 = shared_state::sps30_status().await;

    let mut body: String<512> = String::new();
    let _ = write!(
        body,
        "{{\"uptime_ms\":{},\"window_ms\":{},\"sensors\":{{",
        uptime_ms,
        shared_state::HISTORY_WINDOW_MS
    );
    let _ = write_sensor_status(&mut body, "scd41", &scd41);
    let _ = body.push(',');
    let _ = write_sensor_status(&mut body, "sps30", &sps30);
    let _ = body.push_str("}}");

    send_response(socket, "application/json", &body).await;
}

/// Write one `"name":{...}` member of the `sensors` object.
fn write_sensor_status(
    body: &mut String<512>,
    name: &str,
    status: &shared_state::HistoryStatus,
) -> core::fmt::Result {
    write!(
        body,
        "\"{}\":{{\"interval_ms\":{},\"capacity\":{},\"len\":{},\"first_sequence\":{},\"next_sequence\":{}}}",
        name, status.interval_ms, status.capacity, status.len, status.first_sequence, status.next_sequence
    )
}

/// Send one page of readings for a single sensor.
///
/// `query` selects the sensor and the page: `sensor=scd41|sps30`, `from=` the
/// first sequence number wanted, `limit=` how many readings at most. A `from`
/// that names an already overwritten reading is moved up to the oldest reading
/// still retained, and the page reports the `from` it actually used, so a
/// client that has fallen behind can tell that it has lost readings.
///
/// The body is streamed one reading at a time rather than rendered into a
/// single buffer, which would be a wasteful permanent allocation on the
/// device. Its length is therefore not known in advance, and the end of the
/// body is signalled by closing the connection, as HTTP/1.1 allows for
/// `Connection: close` responses.
///
/// Readings are dated by the device uptime at which they were taken. The board
/// has no real-time clock, so the client subtracts the `uptime_ms` reported
/// here from its own clock to place every reading on a wall-clock timeline.
async fn send_readings(socket: &mut TcpSocket<'_>, query: &str) {
    let sensor = query_param(query, "sensor").unwrap_or("");
    let status = match sensor {
        "scd41" => shared_state::scd41_status().await,
        "sps30" => shared_state::sps30_status().await,
        _ => {
            let _ = socket
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                )
                .await;
            return;
        }
    };

    // Unparsable values fall back to the defaults rather than failing the
    // request: the page never sends any, and a hand-typed URL is easier to
    // work with when a typo still returns data.
    let requested_from = query_param(query, "from")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let limit = query_param(query, "limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(MAX_PAGE_READINGS)
        .min(MAX_PAGE_READINGS);

    let uptime_ms = Instant::now().as_millis();
    let from = requested_from.max(status.first_sequence);
    let available = status.next_sequence.saturating_sub(from);
    let count = core::cmp::min(available, limit as u64) as usize;

    let header = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n";
    if socket.write_all(header.as_bytes()).await.is_err() {
        return;
    }

    let mut chunk: String<384> = String::new();
    let _ = write!(
        chunk,
        "{{\"sensor\":\"{}\",\"uptime_ms\":{},\"interval_ms\":{},\"capacity\":{},\"first_sequence\":{},\"next_sequence\":{},\"from\":{},\"count\":{},\"readings\":[",
        sensor,
        uptime_ms,
        status.interval_ms,
        status.capacity,
        status.first_sequence,
        status.next_sequence,
        from,
        count
    );
    if socket.write_all(chunk.as_bytes()).await.is_err() {
        return;
    }

    // Oldest reading first, so the client can append the page to what it
    // already holds. The array always has exactly `count` elements: a reading
    // that the sensor tasks overwrite while the page is being sent becomes a
    // `null` rather than disappearing, so the client can still map each
    // element to its sequence number.
    for offset in 0..count {
        let sequence = from + offset as u64;
        chunk.clear();
        let separator = if offset == 0 { "" } else { "," };

        if sensor == "scd41" {
            match shared_state::scd41_reading(sequence).await {
                Some(sample) => {
                    let m = sample.value;
                    let _ = write!(
                        chunk,
                        "{}{{\"taken_at_ms\":{},\"co2_ppm\":{},\"temperature_celsius\":{},\"humidity_percent\":{}}}",
                        separator,
                        sample.taken_at.as_millis(),
                        m.co2_ppm,
                        m.temperature_celsius(),
                        m.humidity_percent()
                    );
                }
                None => {
                    let _ = write!(chunk, "{}null", separator);
                }
            }
        } else {
            match shared_state::sps30_reading(sequence).await {
                Some(sample) => {
                    let m = sample.value;
                    let _ = write!(
                        chunk,
                        "{}{{\"taken_at_ms\":{},\"pm1_0\":{},\"pm2_5\":{},\"pm4_0\":{},\"pm10\":{},\"typical_particle_size\":{}}}",
                        separator,
                        sample.taken_at.as_millis(),
                        m.pm1_0,
                        m.pm2_5,
                        m.pm4_0,
                        m.pm10,
                        m.typical_particle_size
                    );
                }
                None => {
                    let _ = write!(chunk, "{}null", separator);
                }
            }
        }

        if socket.write_all(chunk.as_bytes()).await.is_err() {
            return;
        }
    }

    let _ = socket.write_all(b"]}").await;
}
