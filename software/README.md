# XIAO ESP32-S3 Rust firmware

This is a bare-metal Rust firmware project for the Seeed Studio XIAO ESP32-S3.
It targets the Xtensa ESP32-S3 processor and blinks the onboard user LED on
GPIO21 every half second.

## Framework

The firmware runs on the [Embassy](https://embassy.dev) async framework:

- `esp-hal-embassy` provides the `#[esp_hal_embassy::main]` entry point and the
  single-core executor.
- One `TIMG0` hardware timer is handed to `esp_hal_embassy::init` to drive
  `embassy_time`.
- All datasheet-mandated sensor delays are `embassy_time::Timer::after_millis`
  awaits, so they yield to the executor instead of busy-waiting. Only the
  microsecond-scale bit-banged I2C bus recovery still uses a blocking
  `esp_hal::delay::Delay`.
- The I2C transfers themselves remain blocking `esp-hal` calls.

## Requirements

- Espressif Rust toolchain `1.79.0.0`, managed by `espup`.
- `espflash` on `PATH`.
- USB data cable connected to the board.

Install the compatible Xtensa compiler once:

`espup install --targets esp32s3 --toolchain-version 1.79.0.0`

Before building or flashing in a new shell, load Espressif's environment:

`. "$HOME/export-esp.sh"`

## Build

Run `cargo build --release`. The repository's toolchain and target settings
select the ESP compiler and `xtensa-esp32s3-none-elf` target automatically.

## Flash and monitor

1. Put the XIAO into download mode: hold **BOOT**, press and release **RESET**,
   then release **BOOT**.
2. Confirm that Linux creates a serial device such as `/dev/ttyACM0`.
3. Run `cargo run --release` to flash the firmware and open a monitor.

If automatic port selection cannot identify the board, use
`espflash flash --chip esp32s3 --port /dev/ttyACM0 target/xtensa-esp32s3-none-elf/release/xiao-esp32s3`.

The firmware prints `XIAO ESP32-S3 Embassy firmware started; blinking GPIO21` to
the USB JTAG serial monitor at boot, then toggles GPIO21 every half second. The
serial interface is `/dev/ttyACM0` on this system.

## Measurement history and PSRAM

The board keeps a full day of readings per sensor: 8640 SCD41 readings at one
every 10 seconds, 17280 SPS30 readings at one every 5 seconds, and 17280 BME690
readings at one every 5 seconds. That is far more than the internal RAM can
spare next to the Wi-Fi driver and the network stack, so all three ring buffers
live in the external PSRAM.

- The XIAO carries an ESP32-S3R8 module, so `esp-hal` is built with its
  `opsram-8m` feature (8 MB of octal-SPI PSRAM). A module with a different
  memory variant needs the matching `psram-*` or `opsram-*` feature instead.
- `utils::psram::Psram` maps the PSRAM at boot and hands out permanent buffers
  from it; nothing is ever freed, so no allocator is needed.
- At boot the firmware prints how many readings it reserved room for and how
  much PSRAM is left, as a line starting with `History:`. If the reservation
  ever fails it says so and keeps running without retaining readings.

Every stored reading is given a **sequence number**. The first reading a sensor
takes after boot is number zero and each following one is one higher, whether
or not the reading it overwrote is still retained. Sequence numbers are never
reused while the device runs, so a client that remembers the last one it
received can ask for exactly the readings taken since then. They restart at
zero when the device reboots.

Readings are dated by the **device uptime** in milliseconds at which they were
taken. The board has no real-time clock, so a client subtracts the `uptime_ms`
reported with each response from its own clock to learn when the device booted,
and adds each reading's `taken_at_ms` to that to place it on a wall-clock
timeline.

## Discarded warm-up readings

Every sensor needs a moment to settle after it is started, and the readings it
gives before then are not representative. Each task therefore drops a number of
readings after every initialisation. Those readings are still taken and printed
to the serial log, marked `(warm-up, discarded)`, but they are not published to
the history, so a single unsettled value cannot stretch a chart axis and hide
the real data.

The count is a constant named `DISCARDED_WARMUP_READINGS` at the top of each
task module, and it is the only thing to change to adjust the behaviour:

| Sensor | File | Default | Covers |
| --- | --- | --- | --- |
| SCD41 | `src/tasks/scd41_task.rs` | 1 | The first single-shot conversion, taken before the sensor's own temperature has settled. |
| SPS30 | `src/tasks/sps30_task.rs` | 6 | Roughly the first 30 seconds, while the fan and laser spin up. |
| BME690 | `src/tasks/bme690_task.rs` | 2 | The first measurements, taken before the gas heater plate reaches its target. |

Setting a count to 0 publishes every reading. The countdown restarts on every
re-initialisation, not only at boot, so the readings taken right after a bus
error is recovered from are discarded as well.

The BME690 default is not arbitrary: the first reading after start-up measured
386 kΩ of gas resistance against a steady 11-16 kΩ afterwards, and it reported
its heater as stable, so the sensor's own stability flag does not identify it.

## Web API

The HTTP server listens on port 80. Every endpoint is read-only and answers
`GET` only. A request with any other method is answered `405 Method Not
Allowed` with an `Allow: GET` header, an unknown path is answered `404 Not
Found`, and a request whose header block exceeds 512 bytes is answered `431
Request Header Fields Too Large`. Error responses have an empty body.

Successful responses carry `Cache-Control: no-store`. Every response, success
or error, carries `Connection: close`: the server handles one connection at a
time and closes it when it is done, so a client must not issue requests in
parallel.

`/` and `/api/status` are sent with a `Content-Length`. `/api/readings` is
written out as it is generated rather than buffered first, so its length is not
known in advance and it has no `Content-Length`; the end of the body is the
close of the connection, which HTTP/1.1 permits for `Connection: close`
responses.

### `GET /`

The single-page application, served from flash as `text/html`. `GET
/index.html` returns the same thing.

### `GET /api/status`

The device's uptime and the state of every history. This is the endpoint to
poll: it is small, of known length, and tells a client which sequence numbers
are available so it can work out exactly what it is missing before asking for
anything.

```json
{
  "uptime_ms": 3600000,
  "window_ms": 86400000,
  "sensors": {
    "scd41": {
      "interval_ms": 10000,
      "capacity": 8640,
      "len": 360,
      "first_sequence": 0,
      "next_sequence": 360
    },
    "sps30": {
      "interval_ms": 5000,
      "capacity": 17280,
      "len": 720,
      "first_sequence": 0,
      "next_sequence": 720
    },
    "bme690": {
      "interval_ms": 5000,
      "capacity": 17280,
      "len": 720,
      "first_sequence": 0,
      "next_sequence": 720
    }
  }
}
```

| Field | Meaning |
| --- | --- |
| `uptime_ms` | Milliseconds since the device booted, at the moment the response was generated. |
| `window_ms` | How far back readings are retained, in milliseconds. Currently 86400000, one day. |
| `sensors.<name>.interval_ms` | Scheduled milliseconds between two readings of that sensor. |
| `sensors.<name>.capacity` | Readings the ring buffer can hold. Zero if the PSRAM reservation failed, in which case nothing is retained. |
| `sensors.<name>.len` | Readings currently retained. |
| `sensors.<name>.first_sequence` | Sequence number of the oldest retained reading. |
| `sensors.<name>.next_sequence` | Sequence number the next reading will be given. Equals `first_sequence + len`. |

The sensor names are `scd41`, `sps30` and `bme690`.

A client detects a device reboot by `uptime_ms` being lower than the value it
saw previously, and must then discard everything it holds, because the sequence
numbers have restarted.

### `GET /api/readings`

One page of a single sensor's readings, oldest first.

| Parameter | Required | Default | Meaning |
| --- | --- | --- | --- |
| `sensor` | yes | — | `scd41`, `sps30` or `bme690`. Any other value is answered `400 Bad Request`. |
| `from` | no | `0` | Lowest sequence number wanted. Raised to `first_sequence` if it names a reading that has already been overwritten. |
| `limit` | no | `2000` | Largest number of readings to return. Values above 2000 are capped at 2000. |

Values that are not valid numbers fall back to the defaults rather than failing
the request. Percent-escapes are not decoded, which no accepted value needs.

`GET /api/readings?sensor=scd41&from=358&limit=2`:

```json
{
  "sensor": "scd41",
  "uptime_ms": 3600000,
  "interval_ms": 10000,
  "capacity": 8640,
  "first_sequence": 0,
  "next_sequence": 360,
  "from": 358,
  "count": 2,
  "readings": [
    { "taken_at_ms": 3580000, "co2_ppm": 812, "temperature_celsius": 21.5, "humidity_percent": 45.2 },
    { "taken_at_ms": 3590000, "co2_ppm": 818, "temperature_celsius": 21.5, "humidity_percent": 45.1 }
  ]
}
```

| Field | Meaning |
| --- | --- |
| `sensor` | Echo of the requested sensor. |
| `uptime_ms` | Device uptime when the response was generated. |
| `interval_ms`, `capacity`, `first_sequence`, `next_sequence` | As in `/api/status`, for this sensor. |
| `from` | Sequence number of the first element of `readings`, after clamping. A value higher than the `from` that was requested means readings were lost to overwriting. |
| `count` | Number of elements in `readings`. Always equals `readings.length`. |
| `readings` | The readings themselves, oldest first. Element *i* has sequence number `from + i`. |

An element is `null` if that reading was overwritten by the sensor tasks while
the response was being sent. It still occupies its place, so the sequence
number of every following element stays correct.

The SCD41 reading fields are `taken_at_ms`, `co2_ppm` (integer),
`temperature_celsius` and `humidity_percent`. The SPS30 reading fields are
`taken_at_ms`, `pm1_0`, `pm2_5`, `pm4_0`, `pm10` (all micrograms per cubic
metre) and `typical_particle_size` (micrometres). The BME690 reading fields are
`taken_at_ms`, `temperature_celsius`, `pressure_pascals` (pascals, not
hectopascals), `humidity_percent` and `gas_resistance_ohms`.

`gas_resistance_ohms` is the raw resistance of the sensor's heated gas-sensing
film, not an air-quality index. It falls as volatile compounds increase, but it
also moves with humidity and drifts as the film ages, so only its change over
time is meaningful. The BME690 measures temperature and humidity independently
of the SCD41, so the two sensors will not report identical values.

### How the page uses these

On its first load the page fetches `/api/status`, then walks each sensor's
history with `/api/readings` in pages of 2000, which is 5 requests for the
SCD41 and 9 each for the SPS30 and the BME690 when a full day is retained. It
keeps every reading in the browser and redraws after each page arrives.

Afterwards it polls `/api/status` every 5 seconds and requests only the
readings whose sequence numbers it does not yet have, which is normally none or
one per sensor. Passes never overlap, and the page drops readings the device
itself has discarded so its own copy cannot outgrow the device's.

### Calling it from a shell

The device prints the address it got from DHCP at boot as a line starting with
`Web server:`.

```sh
curl http://192.168.1.50/api/status
curl 'http://192.168.1.50/api/readings?sensor=scd41&from=0&limit=100'
```

Quote the URL: an unquoted `&` would put `curl` in the background. To follow a
sensor from a script, read `next_sequence` from one response and pass it as
`from` to the next.
