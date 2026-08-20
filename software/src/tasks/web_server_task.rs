//! Wi-Fi station and HTTP API exposing the newest sensor readings.
//!
//! The board joins an existing Wi-Fi network as a client (station mode) and
//! answers JSON requests on TCP port 80. No user interface is served: the
//! device publishes data only, and a separate frontend server reads it. Three
//! tasks cooperate:
//!
//! * `wifi_connection_task` keeps the radio associated with the access point.
//! * `net_task` runs the embassy-net TCP/IP stack.
//! * `web_server_task` accepts connections and answers HTTP requests.
//!
//! Two resources are served, both of them read-only:
//!
//! * `GET /api/status` returns the device uptime and the state of every
//!   history.
//! * `GET /api/readings` returns one page of a single sensor's readings.
//! * `GET /api/thermal` returns the newest thermal image.
//!
//! A client fetches the retained history once, a page at a time, and
//! afterwards asks only for the readings taken since the last one it holds.
//! `README.md` documents both endpoints in full.

use core::fmt::Write as _;

use embassy_net::tcp::TcpSocket;
use embassy_net::{Stack, StackResources};
use embassy_time::{with_timeout, Duration, Instant, Timer};
use embedded_io_async::Write as _;
use esp_println::println;
use esp_wifi::wifi::{
    ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent, WifiStaDevice,
    WifiState,
};
use heapless::String;

use crate::drivers::as7343::{Channel as As7343Channel, SPECTRAL_CHANNELS};
use crate::drivers::opt4048::Channel as Opt4048Channel;
use crate::tasks::mlx90640_task::IMAGE_INTERVAL_MS;
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
/// Longest wait for a finished connection to drain before it is discarded.
///
/// Only the orderly case is worth waiting for. A client that walked away
/// mid-response leaves part of that response in the transmit buffer, and that
/// buffer is never drained: it is emptied when the socket is listened on
/// again, not when the connection ends. Waiting for it unconditionally would
/// therefore be a wait that never ends.
const CLOSE_TIMEOUT_MS: u64 = 2000;
/// Largest number of readings one `/api/readings` request may return.
///
/// The retained history holds a day of readings, which is too much for a
/// single response, so a client fetches it as a handful of pages of this size
/// and then only asks for the readings taken since its last request.
const MAX_PAGE_READINGS: usize = 2000;

/// Size of the buffer the whole `/api/status` body is built in.
///
/// The body carries one entry per sensor, and an entry is about 160 characters
/// once its two sequence numbers have grown to their full width, so the seven
/// entries and the surrounding object need a little over 1 kB. The margin
/// above that is deliberate: `write!` into a `String` fails on overflow and
/// the failure is discarded, which would leave the client parsing truncated
/// JSON. Adding an eighth sensor means checking this again.
const STATUS_BODY_CAPACITY: usize = 2048;

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

/// Serve the measurement API over HTTP.
///
/// Only one connection is handled at a time, which is sufficient for the
/// single frontend server that polls the device and keeps the memory
/// footprint small.
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

        // Hang up, then put the socket back into a state `accept` can reuse.
        //
        // Both waits are bounded, because neither is guaranteed to finish.
        // `flush` returns once the transmit buffer has been sent and
        // acknowledged, and a connection that ended while a response was
        // still being written leaves bytes in that buffer that will never be
        // acknowledged by anyone: the peer is gone, and closing the socket
        // does not discard them. An unbounded `flush` there would park this
        // task forever, and because nothing else ever calls `accept`, the
        // device would keep answering the network yet refuse every connection
        // to port 80 until it was reset. Abandoning the wait costs nothing:
        // `accept` listens on the socket again, which resets it and drops
        // whatever was left queued.
        let close_timeout = Duration::from_millis(CLOSE_TIMEOUT_MS);

        socket.close();
        let _ = with_timeout(close_timeout, socket.flush()).await;
        socket.abort();
        let _ = with_timeout(close_timeout, socket.flush()).await;
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
        "/api/status" => send_status(socket).await,
        "/api/readings" => send_readings(socket, query).await,
        "/api/thermal" => send_thermal(socket).await,
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

/// Write a complete `200 OK` JSON response with `body` as its payload.
async fn send_response(socket: &mut TcpSocket<'_>, body: &str) {
    let mut header: String<192> = String::new();
    let _ = write!(
        header,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );

    if socket.write_all(header.as_bytes()).await.is_ok() {
        let _ = socket.write_all(body.as_bytes()).await;
    }
}

/// Describe the device and the state of every history.
///
/// This is the endpoint a client polls. It is small and of known length, and
/// it tells the client which sequence numbers are still available, so the
/// client can work out exactly which readings it is missing before asking for
/// any of them.
async fn send_status(socket: &mut TcpSocket<'_>) {
    let uptime_ms = Instant::now().as_millis();
    let scd41 = shared_state::scd41_status().await;
    let sps30 = shared_state::sps30_status().await;
    let bme690 = shared_state::bme690_status().await;
    let as7343 = shared_state::as7343_status().await;
    let bmp581 = shared_state::bmp581_status().await;
    let opt4048 = shared_state::opt4048_status().await;
    let sht41 = shared_state::sht41_status().await;

    // One sensor entry runs to a little over a hundred characters and the
    // sequence numbers in it grow for as long as the board stays up, so the
    // buffer is sized well clear of the seven entries it has to hold: `write!`
    // into a `String` fails on overflow and would leave the client parsing
    // truncated JSON.
    let mut body: String<STATUS_BODY_CAPACITY> = String::new();
    let _ = write!(
        body,
        "{{\"uptime_ms\":{},\"window_ms\":{},\"sensors\":{{",
        uptime_ms,
        shared_state::HISTORY_WINDOW_MS
    );
    let _ = write_sensor_status(&mut body, "scd41", &scd41);
    let _ = body.push(',');
    let _ = write_sensor_status(&mut body, "sps30", &sps30);
    let _ = body.push(',');
    let _ = write_sensor_status(&mut body, "bme690", &bme690);
    let _ = body.push(',');
    let _ = write_sensor_status(&mut body, "as7343", &as7343);
    let _ = body.push(',');
    let _ = write_sensor_status(&mut body, "bmp581", &bmp581);
    let _ = body.push(',');
    let _ = write_sensor_status(&mut body, "opt4048", &opt4048);
    let _ = body.push(',');
    let _ = write_sensor_status(&mut body, "sht41", &sht41);
    let _ = body.push_str("}}");

    send_response(socket, &body).await;
}

/// Write one `"name":{...}` member of the `sensors` object.
fn write_sensor_status(
    body: &mut String<STATUS_BODY_CAPACITY>,
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
/// `query` selects the sensor and the page:
/// `sensor=scd41|sps30|bme690|as7343|bmp581|opt4048|sht41`, `from=` the first sequence number
/// wanted, `limit=` how many readings at most. A `from` that names an already
/// overwritten reading is moved up to the oldest reading still retained, and
/// the page reports the `from` it actually used, so a client that has fallen
/// behind can tell that it has lost readings.
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
        "bme690" => shared_state::bme690_status().await,
        "as7343" => shared_state::as7343_status().await,
        "bmp581" => shared_state::bmp581_status().await,
        "opt4048" => shared_state::opt4048_status().await,
        "sht41" => shared_state::sht41_status().await,
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
    // request: a hand-typed URL is easier to work with when a typo still
    // returns data.
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

    // Large enough for the longest single reading, which is the BME690's: it
    // carries every BSEC output, not just the four measured channels. Its keys
    // alone come to about 300 bytes and a typical reading to about 460, but the
    // margin here is deliberate: `write!` into a `String` fails on overflow and
    // the failure is discarded, which would leave the client parsing truncated
    // JSON, and a single `f32` printed at its worst case runs to 48 characters.
    let mut chunk: String<1024> = String::new();
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
        } else if sensor == "bme690" {
            match shared_state::bme690_reading(sequence).await {
                Some(sample) => {
                    let m = sample.value;
                    // `temperature_celsius` and `humidity_percent` are BSEC's
                    // heat-compensated values, with the sensor's own heating
                    // removed; the `raw_` fields are what the sensor reported.
                    let _ = write!(
                        chunk,
                        "{}{{\"taken_at_ms\":{},\"temperature_celsius\":{},\"pressure_pascals\":{},\"humidity_percent\":{},\"gas_resistance_ohms\":{},\"raw_temperature_celsius\":{},\"raw_humidity_percent\":{},\"iaq\":{},\"static_iaq\":{},\"iaq_accuracy\":{},\"co2_equivalent_ppm\":{},\"tvoc_equivalent_ppb\":{},\"gas_percentage\":{},\"stabilized\":{},\"run_in_complete\":{}}}",
                        separator,
                        sample.taken_at.as_millis(),
                        m.temperature_celsius,
                        m.pressure_pascals,
                        m.relative_humidity_percent,
                        m.gas_resistance_ohms,
                        m.raw_temperature_celsius,
                        m.raw_humidity_percent,
                        m.iaq,
                        m.static_iaq,
                        m.iaq_accuracy.as_raw(),
                        m.co2_equivalent_ppm,
                        m.tvoc_equivalent_ppb,
                        m.gas_percentage,
                        m.stabilized,
                        m.run_in_complete
                    );
                }
                None => {
                    let _ = write!(chunk, "{}null", separator);
                }
            }
        } else if sensor == "as7343" {
            match shared_state::as7343_reading(sequence).await {
                Some(sample) => {
                    let m = sample.value;
                    let _ = write!(
                        chunk,
                        "{}{{\"taken_at_ms\":{}",
                        separator,
                        sample.taken_at.as_millis()
                    );
                    // The twelve filtered channels are keyed by the centre
                    // wavelength of their filter, in nanometres. Every channel
                    // in `SPECTRAL_CHANNELS` is a filtered one, so it always
                    // has one.
                    for channel in SPECTRAL_CHANNELS {
                        if let Some(wavelength_nm) = channel.wavelength_nm() {
                            let _ =
                                write!(chunk, ",\"nm_{}\":{}", wavelength_nm, m.channel(channel));
                        }
                    }
                    // The unfiltered and the flicker-detect photodiode are read
                    // once per integration cycle, and a measurement runs three
                    // cycles, so each carries three readings. They are reported
                    // as they were measured rather than averaged here: they are
                    // three separate measurements of the same light, and how
                    // far apart they fall is itself worth seeing.
                    let _ = write!(
                        chunk,
                        ",\"visible\":[{},{},{}],\"flicker\":[{},{},{}]",
                        m.channel(As7343Channel::Visible1),
                        m.channel(As7343Channel::Visible2),
                        m.channel(As7343Channel::Visible3),
                        m.channel(As7343Channel::FlickerDetect1),
                        m.channel(As7343Channel::FlickerDetect2),
                        m.channel(As7343Channel::FlickerDetect3)
                    );
                    // The gain the device reported alongside the readings, as
                    // the factor the counts were taken with. It is absent only
                    // if the device reported a code that is not a defined gain.
                    match m.gain {
                        Some(gain) => {
                            let _ = write!(chunk, ",\"gain\":{}", gain.multiplier());
                        }
                        None => {
                            let _ = chunk.push_str(",\"gain\":null");
                        }
                    }
                    let _ = write!(
                        chunk,
                        ",\"analog_saturation\":{},\"digital_saturation\":{}}}",
                        m.analog_saturation, m.digital_saturation
                    );
                }
                None => {
                    let _ = write!(chunk, "{}null", separator);
                }
            }
        } else if sensor == "bmp581" {
            match shared_state::bmp581_reading(sequence).await {
                Some(sample) => {
                    let m = sample.value;
                    // Pressure is reported in pascals like the BME690's, so
                    // that a chart can draw the two against one axis.
                    let _ = write!(
                        chunk,
                        "{}{{\"taken_at_ms\":{},\"pressure_pascals\":{},\"temperature_celsius\":{}}}",
                        separator,
                        sample.taken_at.as_millis(),
                        m.pressure_pascals(),
                        m.temperature_celsius()
                    );
                }
                None => {
                    let _ = write!(chunk, "{}null", separator);
                }
            }
        } else if sensor == "opt4048" {
            match shared_state::opt4048_reading(sequence).await {
                Some(sample) => {
                    let m = sample.value;
                    // The four linear ADC codes are sent as measured. They are
                    // what everything else here is derived from, so a client
                    // that wants a different colour space, or a corrected set
                    // of coefficients, can work from them directly.
                    let _ = write!(
                        chunk,
                        "{}{{\"taken_at_ms\":{},\"adc_x\":{},\"adc_y\":{},\"adc_z\":{},\"adc_wideband\":{},\"lux\":{}",
                        separator,
                        sample.taken_at.as_millis(),
                        m.adc_code(Opt4048Channel::X),
                        m.adc_code(Opt4048Channel::Y),
                        m.adc_code(Opt4048Channel::Z),
                        m.adc_code(Opt4048Channel::Wideband),
                        m.lux()
                    );
                    // In darkness the tristimulus values sum to zero, and the
                    // colour of light that is not there has no meaning; the
                    // colour fields are `null` for those readings rather than
                    // some arbitrary substitute a chart would then plot.
                    match m.chromaticity() {
                        Some(chromaticity) => {
                            let _ = write!(
                                chunk,
                                ",\"cie_x\":{},\"cie_y\":{}",
                                chromaticity.x, chromaticity.y
                            );
                        }
                        None => {
                            let _ = chunk.push_str(",\"cie_x\":null,\"cie_y\":null");
                        }
                    }
                    // The correlated colour temperature is additionally absent
                    // for light too far off the black-body locus for the
                    // approximation behind it to mean anything.
                    match m.correlated_color_temperature_kelvin() {
                        Some(cct) => {
                            let _ = write!(chunk, ",\"cct_kelvin\":{}", cct);
                        }
                        None => {
                            let _ = chunk.push_str(",\"cct_kelvin\":null");
                        }
                    }
                    // The exponent the device chose for this measurement, which
                    // under automatic ranging is its own report of how bright
                    // the scene was.
                    let _ = write!(
                        chunk,
                        ",\"exponent\":{},\"overload\":{}}}",
                        m.channel(Opt4048Channel::Y).exponent,
                        m.overload
                    );
                }
                None => {
                    let _ = write!(chunk, "{}null", separator);
                }
            }
        } else if sensor == "sht41" {
            match shared_state::sht41_reading(sequence).await {
                Some(sample) => {
                    let m = sample.value;
                    // Temperature and humidity are named as the SCD41's and the
                    // BME690's are, so a chart can draw all three against one
                    // axis.
                    let _ = write!(
                        chunk,
                        "{}{{\"taken_at_ms\":{},\"temperature_celsius\":{},\"humidity_percent\":{}}}",
                        separator,
                        sample.taken_at.as_millis(),
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

/// Send the newest thermal image.
///
/// Unlike the sensors, the camera keeps no history: an image is 768
/// temperatures, so only the last one exists and this endpoint always returns
/// that one. `sequence` counts the images the camera has taken, so a client
/// can tell a new image from the one it already has without comparing 768
/// numbers.
///
/// `pixels` is a flat array of degrees Celsius, `width` per row, rows ordered
/// from the top of the image. It is streamed one row at a time, both because
/// the whole array is far larger than any buffer this device can spare and so
/// that the camera task is never kept waiting for the network: each row is
/// copied out of the shared image on its own.
///
/// A device that has not finished its first image answers with
/// `"available":false` and an empty `pixels` array rather than an error, so a
/// client can treat the camera as simply having nothing yet.
async fn send_thermal(socket: &mut TcpSocket<'_>) {
    let uptime_ms = Instant::now().as_millis();
    let status = shared_state::thermal_status().await;

    let header = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n";
    if socket.write_all(header.as_bytes()).await.is_err() {
        return;
    }

    // One row of temperatures printed to two decimals runs to about 200
    // characters; the buffer is sized well clear of that because `write!`
    // into a `String` fails on overflow and would leave the client parsing
    // truncated JSON.
    let mut chunk: String<512> = String::new();

    let Some(status) = status else {
        let _ = write!(
            chunk,
            "{{\"uptime_ms\":{},\"available\":false,\"width\":{},\"height\":{},\"pixels\":[]}}",
            uptime_ms,
            shared_state::THERMAL_COLUMNS,
            shared_state::THERMAL_ROWS
        );
        let _ = socket.write_all(chunk.as_bytes()).await;
        return;
    };

    let _ = write!(
        chunk,
        "{{\"uptime_ms\":{},\"available\":true,\"taken_at_ms\":{},\"sequence\":{},\"interval_ms\":{},\"width\":{},\"height\":{},\"min_celsius\":{:.2},\"max_celsius\":{:.2},\"mean_celsius\":{:.2},\"ambient_celsius\":{:.2},\"pixels\":[",
        uptime_ms,
        status.taken_at.as_millis(),
        status.sequence,
        IMAGE_INTERVAL_MS,
        shared_state::THERMAL_COLUMNS,
        shared_state::THERMAL_ROWS,
        status.summary.min_celsius,
        status.summary.max_celsius,
        status.summary.mean_celsius,
        status.summary.ambient_celsius
    );
    if socket.write_all(chunk.as_bytes()).await.is_err() {
        return;
    }

    // Two decimals is well past what the camera can resolve and keeps the
    // whole image inside about 5 kB.
    let mut row = [0.0f32; shared_state::THERMAL_COLUMNS];
    for index in 0..shared_state::THERMAL_ROWS {
        if !shared_state::thermal_row(index, &mut row).await {
            break;
        }

        chunk.clear();
        for (column, pixel) in row.iter().enumerate() {
            let separator = if index == 0 && column == 0 { "" } else { "," };
            let _ = write!(chunk, "{}{:.2}", separator, pixel);
        }

        if socket.write_all(chunk.as_bytes()).await.is_err() {
            return;
        }
    }

    let _ = socket.write_all(b"]}").await;
}
