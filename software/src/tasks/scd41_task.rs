//! Periodic SCD41 CO2 / temperature / humidity read-out.

use embassy_time::Timer;
use esp_println::println;

use crate::drivers::i2c_bus::SharedI2cBus;
use crate::drivers::scd41::Scd41;

/// Idle time between two read-outs.
const MEASUREMENT_INTERVAL_MS: u64 = 5000;
/// Idle time before retrying after a bus or sensor error.
const ERROR_RETRY_DELAY_MS: u64 = 1000;

/// Periodically read the SCD41 and print the result.
///
/// The shared bus is locked for the whole transaction, including the 5 second
/// conversion, and released while the task waits for the next interval.
#[embassy_executor::task]
pub async fn measure_task(bus: &'static SharedI2cBus) {
    // Set on every bus error so the next cycle re-runs the sensor's init
    // sequence instead of assuming it is still in the idle state.
    let mut needs_init = true;

    loop {
        let result = {
            let mut bus = bus.lock().await;
            let mut i2c = bus.acquire();
            let mut sensor = Scd41::new(&mut i2c);

            let initialized = if needs_init {
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
            } else {
                true
            };

            if initialized {
                Some(sensor.measure_single_shot().await)
            } else {
                None
            }
        };

        match result {
            Some(Ok(measurement)) => {
                needs_init = false;
                println!(
                    "CO2: {} ppm, temperature: {} C, humidity: {} %",
                    measurement.co2_ppm,
                    measurement.temperature_celsius(),
                    measurement.humidity_percent()
                );
            }
            Some(Err(error)) => {
                needs_init = true;
                println!("SCD41: measurement failed: {:?}, recovering bus", error);
            }
            None => needs_init = true,
        }

        if needs_init {
            Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
        } else {
            Timer::after_millis(MEASUREMENT_INTERVAL_MS).await;
        }
    }
}
