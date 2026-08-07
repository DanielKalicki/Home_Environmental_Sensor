# XIAO ESP32-S3 Rust firmware

This is a bare-metal Rust firmware project for the Seeed Studio XIAO ESP32-S3.
It targets the Xtensa ESP32-S3 processor and blinks the onboard user LED on
GPIO21 every half second.

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

The firmware prints `XIAO ESP32-S3 firmware started; blinking GPIO21` to the
USB JTAG serial monitor at boot, then toggles GPIO21 every half second. The
serial interface is `/dev/ttyACM0` on this system.
