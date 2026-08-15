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
`espflash flash --chip esp32s3 --partition-table partitions.csv --port /dev/ttyACM0 target/xtensa-esp32s3-none-elf/release/xiao-esp32s3`.

`--partition-table` is not optional. The firmware saves BSEC's learned gas
baseline into a partition named `bsec_state`, and without the project's own
[partitions.csv](partitions.csv) espflash generates its default table, whose
application partition covers the same address. `cargo run` and
`tools/flash.sh` both pass the flag already.

Flashing does not erase the saved state. espflash writes the bootloader, the
partition table and the application, and `bsec_state` sits past the end of all
three, so a new build starts with the calibration the previous one learned. To
deliberately discard it, use `espflash erase-parts bsec_state`.

The firmware prints `XIAO ESP32-S3 Embassy firmware started; blinking GPIO21` to
the USB JTAG serial monitor at boot, then toggles GPIO21 every half second. The
serial interface is `/dev/ttyACM0` on this system.

## Measurement history and PSRAM

The board keeps a full day of readings per sensor: 8640 SCD41 readings at one
every 10 seconds, 17280 SPS30 readings at one every 5 seconds, 14400 BME690
readings at one every 6 seconds, and 17280 AS7343 readings at one every 5
seconds. That is far more than the internal RAM can spare next to the Wi-Fi
driver and the network stack, so all four ring buffers live in the external
PSRAM.

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
| AS7343 | `src/tasks/as7343_task.rs` | 1 | The first measurement after the gain is applied, whose `ASTATUS` reports a gain the sensor was not set to. |

Setting a count to 0 publishes every reading. The countdown restarts on every
re-initialisation, not only at boot, so the readings taken right after a bus
error is recovered from are discarded as well.

The BME690 does not discard anything, because BSEC reports its own readiness
instead. See the section below.

## The BME690 and BSEC

The BME690 measures temperature, pressure, humidity and the electrical
resistance of a heated gas-sensing film. Only the first three mean anything on
their own. The gas resistance falls as volatile compounds increase, but it also
moves with humidity and drifts as the film ages, so a single reading cannot be
turned into an air-quality figure without a baseline learned from the
environment the sensor sits in.

Bosch supplies that model as **BSEC**, a closed-source library shipped as a
prebuilt archive. This firmware links the ESP32-S3 build of it from
`src/drivers/bsec/bsec_v3-3-0-0/release_bin/IAQ/bin/esp/esp32_s3/libalgobsec.a`.
Only the Rust side is written here:

| File | Contents |
| --- | --- |
| `src/drivers/bsec/ffi.rs` | The library's C interface, declared by hand. The struct layouts are checked against the sizes the library was built with at compile time, so a mismatch is a build error rather than a runtime fault. |
| `src/drivers/bsec/mod.rs` | A safe wrapper: instance memory, the output subscription, and typed inputs and outputs. |
| `src/drivers/bsec/config.rs` | The tuning blob, transcribed from Bosch's C array. |
| `src/drivers/bsec/math_shims.rs` | The C maths functions the archive calls, forwarded to the `libm` crate. Rust's `core` does not provide them in a `no_std` binary. |
| `src/utils/flash_store.rs` | Keeps the learned baseline in the flash across reboots. |

BSEC decides how the sensor is operated, so `src/tasks/bme690_task.rs` is a loop
around the library rather than a fixed schedule: it asks BSEC what oversampling,
heater temperature and heater duration to use and when to come back, applies
them, runs one forced-mode measurement, feeds the compensated result back, and
sleeps until the moment BSEC named.

The library runs at its low-power rate, one measurement every 3 seconds, which
is the only rate that produces a TVOC estimate. Every cycle has to run, because
the estimates depend on a steady stream of measurements, but only every second
one is stored: air quality indoors does not change fast enough to justify the
PSRAM that 3-second history would cost. That is why the retained interval is
6 seconds while the sensor is read every 3.

The tuning blob is chosen to match. `IAQ_33V_3S_4D` means a 3.3 V sensor supply,
the 3-second rate, and a baseline horizon of four days. The blob encodes the
rate it was tuned for, so changing the rate means transcribing the matching blob
from `release_bin/IAQ/config/bme690/` as well.

BSEC needs time before its output is trustworthy, and it says so itself through
three fields carried with every reading:

| Field | Meaning |
| --- | --- |
| `stabilized` | False while the gas film is still burning off after power-up. |
| `run_in_complete` | False until enough measurements have been collected to place the baseline. |
| `iaq_accuracy` | 0 unreliable, 1 calibrating, 2 calibrated, 3 high accuracy. |

The learned baseline is written to the flash so that a reboot does not throw it
away. See the next section.

## Saving the learned baseline

BSEC needs hours of measurements before its air-quality output is trustworthy,
so losing that work to a power cut would make the sensor close to useless in a
place where the mains is not perfectly reliable. The library can serialise
what it has learned into a blob of at most 255 bytes, and the firmware keeps
that blob in the flash.

[src/utils/flash_store.rs](src/utils/flash_store.rs) is the storage
side. It is one named slot holding one opaque byte string:

- The address comes from the partition table, looked up at runtime by the label
  `bsec_state`. Compiling the address in instead would mean keeping two files
  in step by hand, and a stale constant would not fail cleanly — it would erase
  a sector of whatever else happened to be there, quite possibly the running
  firmware.
- Every record carries a CRC-32. The flash can only be erased a whole 4096-byte
  sector at a time, so saving 255 bytes briefly blanks a sector, and losing
  power in that window leaves a half-written record behind. A record that does
  not check out is reported as if the slot were empty, so the firmware simply
  starts learning again.
- Writing to the flash means turning off the instruction cache, so the routines
  run from RAM inside a critical section. A save blocks every task and every
  interrupt for the few milliseconds an erase takes. That is why it is done in
  the gap BSEC leaves between measurements, and never in the middle of one.

A save happens when either of two things is true:

- The IAQ accuracy has risen since the last save. Accuracy only climbs through
  four values, so this accounts for at most three writes over the life of a
  boot, and it means a hard-won calibration is on the flash within seconds of
  being reached rather than hours later.
- Six hours have passed since the last save. This bounds how much learning a
  power cut can cost.

That comes to a handful of erases a day against a sector rated for around
100000, so the flash will not wear out. A restored state is deliberately not
saved straight back: without that check, every boot would erase a sector to
store what it had just read out of it.

At boot the firmware prints one of `BSEC: restored N bytes of learned state`,
`BSEC: no saved state; learning from scratch`, or a line explaining why neither
happened. If the device was flashed without the project's partition table it
says so explicitly and keeps running without saving anything.

A state blob is tied to the library version and the tuning blob it was written
with. Changing either makes BSEC reject the old state, which is reported and
then ignored.

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

It shows the latest reading from each sensor as a card, then a day of history as
charts:

| Chart | Series |
| --- | --- |
| Carbon dioxide | The SCD41's measurement and the BME690's estimate, on one axis so the gap between them is visible. |
| Temperature | SCD41 and BME690. |
| Humidity | SCD41 and BME690. |
| Indoor air quality | `iaq` and `static_iaq`. The two separating shows the baseline moving underneath. |
| Volatile organic compounds | `tvoc_equivalent_ppb`. |
| Gas | `gas_percentage`, on a fixed 0-100 axis. |
| Fine particulates | PM1.0, PM2.5 and PM4.0. |
| Coarse particulates | PM10 and the typical particle size. |
| Barometric pressure | BME690. |
| Gas resistance | The BME690's raw film resistance, in kilohms. |
| Light level | The AS7343's unfiltered channel, averaged over the three readings one measurement produces. |
| Spectral channels | The AS7343's 450, 550, 640 and 855 nm channels on a shared zero-based axis, so the light changing colour is visible. |

The AS7343's own section carries a bar per filtered channel for the newest
reading, drawn in the colour that channel's wavelength is seen as. It is bars
rather than a curve because the twelve channels are twelve photodiodes behind
twelve filters, not a curve sampled at twelve points, and they are spaced
evenly rather than to scale because placing them by wavelength crowds the four
between 515 and 555 nm together. The heights are raw converter counts, which
are not corrected for the differing response of the channels and are not
divided by the gain, so one bar standing above its neighbour does not by itself
mean there is more light at that wavelength. What the chart shows reliably is
how one channel changes.

The four gas-derived cards are left uncoloured while `iaq_accuracy` is 0,
because BSEC emits fixed placeholders until it has a baseline and colouring one
green would claim the air is fine on the strength of a number it has not
measured. A line under those cards says which stage the sensor has reached and
disappears once calibration is done.

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
      "interval_ms": 6000,
      "capacity": 14400,
      "len": 600,
      "first_sequence": 0,
      "next_sequence": 600
    },
    "as7343": {
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

The sensor names are `scd41`, `sps30`, `bme690` and `as7343`.

A client detects a device reboot by `uptime_ms` being lower than the value it
saw previously, and must then discard everything it holds, because the sequence
numbers have restarted.

### `GET /api/readings`

One page of a single sensor's readings, oldest first.

| Parameter | Required | Default | Meaning |
| --- | --- | --- | --- |
| `sensor` | yes | — | `scd41`, `sps30`, `bme690` or `as7343`. Any other value is answered `400 Bad Request`. |
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
metre) and `typical_particle_size` (micrometres).

The BME690 reading fields are everything BSEC produces:

| Field | Meaning |
| --- | --- |
| `taken_at_ms` | Device uptime when the reading was taken. |
| `temperature_celsius` | Temperature with the sensor's own heating removed. |
| `humidity_percent` | Relative humidity, corrected against the same. |
| `pressure_pascals` | Pressure in pascals, not hectopascals. |
| `gas_resistance_ohms` | Resistance of the heated gas-sensing film. |
| `raw_temperature_celsius` | Temperature as the sensor reported it, uncorrected. |
| `raw_humidity_percent` | Relative humidity as the sensor reported it, uncorrected. |
| `iaq` | Indoor air quality index, 0-500, relative to the learned baseline. |
| `static_iaq` | The same index without the baseline tracking, so it does not drift back towards its own average. |
| `iaq_accuracy` | 0 unreliable, 1 calibrating, 2 calibrated, 3 high accuracy. |
| `co2_equivalent_ppm` | CO2 estimated from the gas signal. It is not a measurement; the SCD41 measures CO2 directly. |
| `tvoc_equivalent_ppb` | Total volatile organic compounds, estimated the same way. |
| `gas_percentage` | Where the current gas signal sits between the cleanest and dirtiest air seen so far. |
| `stabilized` | False while the gas film is still burning off after power-up. |
| `run_in_complete` | False until BSEC has collected enough measurements to place the baseline. |

Everything derived from the gas signal is an estimate against a learned
baseline, so treat `iaq`, `co2_equivalent_ppm` and `tvoc_equivalent_ppb` as
meaningful only once `iaq_accuracy` has reached at least 1 and
`run_in_complete` is true. The baseline is kept in flash across reboots, so
this normally only has to be waited out once; see the section on saving it
above. The BME690 measures temperature and
humidity independently of the SCD41, so the two sensors will not report
identical values.

The AS7343 reading fields are:

| Field | Meaning |
| --- | --- |
| `taken_at_ms` | Device uptime when the reading was taken. |
| `nm_405` … `nm_855` | One field per filtered channel, named after the centre wavelength of its filter in nanometres: 405, 425, 450, 475, 515, 550, 555, 600, 640, 690, 745 and 855. |
| `visible` | Three readings of the unfiltered photodiode, one per integration cycle. |
| `flicker` | Three readings of the flicker-detect photodiode, the same way. |
| `gain` | The factor the counts were taken with, 0.5 to 2048. `null` if the device reported a code that is not a defined gain. |
| `analog_saturation` | True when the photodiode current exceeded what the converter can take. |
| `digital_saturation` | True when a channel reached the top of its count range. |

Every channel value is the raw count the sensor's converter returned for one
integration, not an irradiance. The counts are not corrected for how differently
the channels respond, so they cannot be compared with each other as if they
were, and they scale with `gain`, which is not divided out. A measurement runs
three integration cycles, which is why `visible` and `flicker` carry three
values: they are three separate measurements of the same light, reported as
they were taken rather than averaged here. Either saturation flag being true
means the counts are cut off rather than merely high, and the reading should be
discarded.

### How the page uses these

On its first load the page fetches `/api/status`, then walks each sensor's
history with `/api/readings` in pages of 2000, which is 5 requests for the
SCD41, 9 for the SPS30 and 8 for the BME690 when a full day is retained. It
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
