//! Periodic SCD41 CO2 / temperature / humidity read-out.

use embassy_time::{Duration, Ticker, Timer};
use esp_println::println;

use crate::drivers::i2c_bus::SharedI2cBus;
use crate::drivers::scd41::{Measurement, Scd41, SINGLE_SHOT_MEASUREMENT_DELAY_MS};
use crate::utils::history::MeasurementHistory;
use crate::utils::shared_state;

/// Time between the starts of two single-shot conversions.
const MEASUREMENT_INTERVAL_MS: u64 = 10_000;
/// Idle time before retrying after a bus or sensor error.
const ERROR_RETRY_DELAY_MS: u64 = 1000;
/// Number of successful readings retained in memory.
const HISTORY_CAPACITY: usize = 60;

/// Periodically read the SCD41 and print the result.
///
/// Conversion starts follow a fixed 10 second schedule. The shared bus is
/// released during the sensor's 5 second conversion, allowing the SPS30 task
/// to use it meanwhile.
#[embassy_executor::task]
pub async fn measure_task(bus: &'static SharedI2cBus) {
    // Set on every bus error so the next cycle re-runs the sensor's init
    // sequence instead of assuming it is still in the idle state.
    let mut needs_init = true;
    let mut history = MeasurementHistory::<Measurement, HISTORY_CAPACITY>::new();
    let mut ticker = Ticker::every(Duration::from_millis(MEASUREMENT_INTERVAL_MS));

    loop {
        if needs_init {
            let initialized = {
                let mut bus = bus.lock().await;
                let mut i2c = bus.acquire();
                let mut sensor = Scd41::new(&mut i2c);

                match sensor.stop_periodic_measurement().await {
                    Ok(()) => match sensor.serial_number().await {
                        Ok(serial) => {
                            println!("SCD41 ready, serial number: 0x{:012X}", serial);
                            true
                        }
                        Err(error) => {
                            println!("SCD41: initialisation failed: {:?}", error);
                            false
                        }
                    },
                    Err(error) => {
                        println!("SCD41: initialisation failed: {:?}", error);
                        false
                    }
                }
            };

            if !initialized {
                Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
                continue;
            }

            // Begin a fresh fixed schedule after recovery; the first
            // conversion below starts immediately.
            ticker.reset();
        }

        let started = {
            let mut bus = bus.lock().await;
            let mut i2c = bus.acquire();
            let mut sensor = Scd41::new(&mut i2c);
            sensor.start_single_shot()
        };

        if let Err(error) = started {
            println!("SCD41: could not start measurement: {:?}", error);
            needs_init = true;
            Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
            continue;
        }

        // The I2C mutex is intentionally not held while the SCD41 converts.
        Timer::after_millis(SINGLE_SHOT_MEASUREMENT_DELAY_MS).await;

        let result = {
            let mut bus = bus.lock().await;
            let mut i2c = bus.acquire();
            let mut sensor = Scd41::new(&mut i2c);
            sensor.read_measurement().await
        };

        match result {
            Ok(measurement) => {
                needs_init = false;
                history.push(measurement);
                shared_state::publish_scd41(measurement).await;
                println!(
                    "CO2: {} ppm, temperature: {} C, humidity: {} %",
                    measurement.co2_ppm,
                    measurement.temperature_celsius(),
                    measurement.humidity_percent()
                );
            }
            Err(error) => {
                needs_init = true;
                println!("SCD41: measurement failed: {:?}, recovering bus", error);
            }
        }

        if needs_init {
            Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
        } else {
            // `Ticker` advances from its previous deadline instead of from
            // this call, equivalent to FreeRTOS `vTaskDelayUntil()`.
            ticker.next().await;
        }
    }
}
