//! Wi-Fi station and HTTP server exposing the newest sensor readings.
//!
//! The board joins an existing Wi-Fi network as a client (station mode) and
//! serves a single HTML page on TCP port 80. Three tasks cooperate:
//!
//! * `wifi_connection_task` keeps the radio associated with the access point.
//! * `net_task` runs the embassy-net TCP/IP stack.
//! * `web_server_task` accepts connections and answers HTTP requests.

use core::fmt::Write as _;

use embassy_net::tcp::TcpSocket;
use embassy_net::{Stack, StackResources};
use embassy_time::{Duration, Timer};
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
/// Number of sockets embassy-net may keep open at once.
pub const SOCKET_COUNT: usize = 3;
/// Idle time before retrying after a failed association attempt.
const RECONNECT_DELAY_MS: u64 = 5000;
/// Close an idle client connection after this long without traffic.
const SOCKET_TIMEOUT_S: u64 = 10;

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
    let path = parts.next().unwrap_or("");

    if method != "GET" {
        let _ = socket
            .write_all(
                b"HTTP/1.1 405 Method Not Allowed\r\nAllow: GET\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            )
            .await;
        return;
    }

    match path {
        "/" | "/index.html" => send_page(socket).await,
        _ => {
            let _ = socket
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                )
                .await;
        }
    }
}

/// Render and send the measurement page.
async fn send_page(socket: &mut TcpSocket<'_>) {
    let mut body: String<1024> = String::new();
    render_page(&mut body).await;

    let mut header: String<128> = String::new();
    let _ = write!(
        header,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );

    if socket.write_all(header.as_bytes()).await.is_ok() {
        let _ = socket.write_all(body.as_bytes()).await;
    }
}

/// Write the HTML document for the newest readings into `out`.
///
/// Writes are made with `write!`, whose errors only signal that the
/// fixed-capacity buffer is full; in that case the page is simply truncated.
async fn render_page(out: &mut String<1024>) {
    let readings = shared_state::snapshot().await;

    let _ = out.push_str(concat!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">",
        "<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">",
        "<meta http-equiv=\"refresh\" content=\"10\">",
        "<title>Home Environmental Sensor</title></head><body>",
        "<h1>Home Environmental Sensor</h1>"
    ));

    match readings.scd41 {
        Some(m) => {
            let _ = write!(
                out,
                "<h2>SCD41</h2><ul><li>CO2: {} ppm</li><li>Temperature: {} C</li><li>Humidity: {} %</li></ul>",
                m.co2_ppm,
                m.temperature_celsius(),
                m.humidity_percent()
            );
        }
        None => {
            let _ = out.push_str("<h2>SCD41</h2><p>No reading yet.</p>");
        }
    }

    match readings.sps30 {
        Some(m) => {
            let _ = write!(
                out,
                "<h2>SPS30</h2><ul><li>PM1.0: {} ug/m3</li><li>PM2.5: {} ug/m3</li><li>PM4.0: {} ug/m3</li><li>PM10: {} ug/m3</li><li>Typical particle size: {} um</li></ul>",
                m.pm1_0, m.pm2_5, m.pm4_0, m.pm10, m.typical_particle_size
            );
        }
        None => {
            let _ = out.push_str("<h2>SPS30</h2><p>No reading yet.</p>");
        }
    }

    let _ = out.push_str("<p>This page reloads every 10 seconds.</p></body></html>");
}
