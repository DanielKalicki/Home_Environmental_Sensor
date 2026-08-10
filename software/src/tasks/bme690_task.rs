//! Periodic BME690 chip-id read-out.
//!
//! This task does not yet drive real measurements; it only confirms the
//! sensor is alive on the shared bus by re-reading its fixed chip-id
//! register on a regular schedule.

use embassy_time::{Duration, Ticker, Timer};
use esp_println::println;

use crate::drivers::bme690::{Bme690, Error};
use crate::drivers::i2c_bus::SharedI2cBus;

/// Time between chip-id reads.
pub const MEASUREMENT_INTERVAL_MS: u64 = 5000;
/// Idle time before retrying after a bus or sensor error.
const ERROR_RETRY_DELAY_MS: u64 = 1000;
/// Chip-id value reported by a genuine BME690.
const EXPECTED_CHIP_ID: u8 = 0x61;

/// Verify the sensor responds and report its chip id.
///
/// The shared bus is held for the whole sequence. Returns `false` if the
/// read failed, in which case the caller should retry later. An unexpected
/// chip id is only logged as a warning.
async fn initialize(bus: &SharedI2cBus) -> bool {
    let mut bus = bus.lock().await;
    let mut i2c = bus.acquire();
    let mut sensor = Bme690::new(&mut i2c);

    match sensor.chip_id().await {
        Ok(chip_id) => {
            println!("BME690 ready, chip id: 0x{:02X}", chip_id);
            if chip_id != EXPECTED_CHIP_ID {
                println!(
                    "BME690: warning, attached sensor reports chip id 0x{:02X}",
                    chip_id
                );
            }
            true
        }
        Err(error) => {
            println!("BME690: initialisation failed: {:?}", error);
            false
        }
    }
}

/// Re-read the chip-id register as a liveness check.
async fn read_once(bus: &'static SharedI2cBus) -> Result<u8, Error> {
    let mut bus = bus.lock().await;
    let mut i2c = bus.acquire();
    let mut sensor = Bme690::new(&mut i2c);
    sensor.chip_id().await
}

/// Periodically read the BME690 chip-id register and print the result.
#[embassy_executor::task]
pub async fn measure_task(bus: &'static SharedI2cBus) {
    // Set on every bus error so the next cycle re-runs the sensor's init
    // sequence instead of assuming it is still in the idle state.
    let mut needs_init = true;
    let mut ticker = Ticker::every(Duration::from_millis(MEASUREMENT_INTERVAL_MS));

    loop {
        if needs_init && !initialize(bus).await {
            Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
            continue;
        }
        let reinitialized = needs_init;

        match read_once(bus).await {
            Ok(chip_id) => {
                needs_init = false;
                println!("BME690: chip id 0x{:02X}", chip_id);

                // Begin a fresh fixed schedule after recovery; this read-out
                // becomes the first deadline of the new schedule.
                if reinitialized {
                    ticker.reset();
                }
                // `Ticker` advances from its previous deadline instead of from
                // this call, equivalent to FreeRTOS `vTaskDelayUntil()`.
                ticker.next().await;
            }
            Err(error) => {
                needs_init = true;
                println!("BME690: read failed: {:?}, recovering bus", error);
                Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
            }
        }
    }
}
