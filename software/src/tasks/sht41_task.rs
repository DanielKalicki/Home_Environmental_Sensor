//! Periodic SHT41 temperature / humidity read-out.
//!
//! Every cycle runs one high-precision measurement, prints it to the serial
//! console and appends it to the retained history the web server serves.

use embassy_time::{Duration, Ticker, Timer};
use esp_hal::peripherals::I2C0;
use esp_println::println;

use crate::drivers::i2c_bus::SharedI2cBus;
use crate::drivers::sht41::{Error, Measurement, Sht41};
use crate::utils::shared_state;

/// Time between the starts of two measurements.
///
/// Public because the retained history is sized from it: a full day of
/// readings at this interval is what the ring buffer has to hold.
pub const MEASUREMENT_INTERVAL_MS: u64 = 10_000;

/// Idle time before retrying after a bus or sensor error.
const ERROR_RETRY_DELAY_MS: u64 = 1000;

/// How many readings are dropped after the sensor is reset.
///
/// The readings taken right after the soft reset are still carrying the
/// sensor's own start-up self-heating, so they read warm and correspondingly
/// dry. Discarded readings are still read and printed, they are only kept out
/// of the published history, so the settling cannot distort the charts. The
/// count restarts after every re-initialisation, including the one following a
/// bus error. Set to 0 to publish every reading.
const DISCARDED_WARMUP_READINGS: u32 = 2;

/// Checks run against the sensor before measurements are started.
///
/// New start-up checks belong here; the first failing one aborts the sequence.
async fn run_init_checks(sensor: &mut Sht41<'_, '_, I2C0>) -> Result<(), Error> {
    sensor.soft_reset().await?;

    let serial = sensor.serial_number().await?;
    println!("SHT41 ready, serial number: 0x{:08X}", serial);

    Ok(())
}

/// Bring the sensor into a known state and verify it responds.
///
/// The shared bus is held for the whole sequence. Returns `false` if any
/// check failed, in which case the caller should retry later.
async fn initialize(bus: &SharedI2cBus) -> bool {
    let mut bus = bus.lock().await;
    let mut i2c = bus.acquire();
    let mut sensor = Sht41::new(&mut i2c);

    match run_init_checks(&mut sensor).await {
        Ok(()) => true,
        Err(error) => {
            println!("SHT41: initialisation failed: {:?}", error);
            false
        }
    }
}

/// Run one high-precision measurement.
///
/// Like the BMP581 task this one keeps the shared bus for the whole
/// conversion instead of releasing it in between: a high-precision conversion
/// finishes in under 10 ms, which is shorter than the bus recovery a second
/// lock would have to repeat.
async fn read_once(bus: &SharedI2cBus) -> Result<Measurement, Error> {
    let mut bus = bus.lock().await;
    let mut i2c = bus.acquire();
    let mut sensor = Sht41::new(&mut i2c);
    sensor.measure_high_precision().await
}

/// Periodically read the SHT41, print the result and retain it.
///
/// Measurement starts follow a fixed [`MEASUREMENT_INTERVAL_MS`] schedule.
#[embassy_executor::task]
pub async fn measure_task(bus: &'static SharedI2cBus) {
    // Set on every bus error so the next cycle re-runs the sensor's init
    // sequence instead of assuming it is still in a known state.
    let mut needs_init = true;
    // Counts down the readings still to be dropped, restarted every time the
    // sensor is initialised.
    let mut warmup_remaining = DISCARDED_WARMUP_READINGS;
    let mut ticker = Ticker::every(Duration::from_millis(MEASUREMENT_INTERVAL_MS));

    loop {
        if needs_init {
            if !initialize(bus).await {
                Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
                continue;
            }
            warmup_remaining = DISCARDED_WARMUP_READINGS;
        }
        let reinitialized = needs_init;

        match read_once(bus).await {
            Ok(measurement) => {
                needs_init = false;

                // Reported either way, but withheld from the history until the
                // sensor has settled.
                let settling = warmup_remaining > 0;
                if settling {
                    warmup_remaining -= 1;
                } else {
                    shared_state::publish_sht41(measurement).await;
                }

                println!(
                    "SHT41 temperature: {} C, humidity: {} %{}",
                    measurement.temperature_celsius(),
                    measurement.humidity_percent(),
                    if settling { " (warm-up, discarded)" } else { "" }
                );

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
                println!("SHT41: measurement failed: {:?}, recovering bus", error);
                Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
            }
        }
    }
}
