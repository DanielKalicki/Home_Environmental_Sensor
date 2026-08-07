//! Periodic SPS30 particulate-matter read-out.

use embassy_time::Timer;
use esp_hal::i2c::Instance;
use esp_println::println;

use crate::drivers::i2c_bus::SharedI2cBus;
use crate::drivers::sps30::{Error, Measurement, Sps30};

/// Idle time between two read-outs.
const MEASUREMENT_INTERVAL_MS: u64 = 5000;
/// Idle time before retrying after a bus or sensor error.
const ERROR_RETRY_DELAY_MS: u64 = 1000;
/// How long to wait for the sensor to flag a fresh result.
const DATA_READY_POLL_ATTEMPTS: usize = 30;
const DATA_READY_POLL_DELAY_MS: u64 = 100;

/// Wait for the sensor to flag a fresh result, then read it.
///
/// If no flag appears within the poll window the last values are read anyway;
/// a genuine bus problem still surfaces as an error from the read itself.
async fn read_when_ready<T: Instance>(sensor: &mut Sps30<'_, '_, T>) -> Result<Measurement, Error> {
    for _ in 0..DATA_READY_POLL_ATTEMPTS {
        if sensor.is_data_ready().await? {
            break;
        }
        Timer::after_millis(DATA_READY_POLL_DELAY_MS).await;
    }

    sensor.read_measured_values().await
}

/// Periodically read the SPS30 and print the result.
///
/// The shared bus is locked for the whole transaction and released while the
/// task waits for the next interval, so the SCD41 task can use it meanwhile.
#[embassy_executor::task]
pub async fn measure_task(bus: &'static SharedI2cBus) {
    // Set on every bus error so the next cycle re-runs the sensor's init
    // sequence instead of assuming it is still in the idle state.
    let mut needs_init = true;

    loop {
        let result = {
            let mut bus = bus.lock().await;
            let mut i2c = bus.acquire();
            let mut sensor = Sps30::new(&mut i2c);

            let initialized = if needs_init {
                match sensor.serial_number().await {
                    Ok((serial, len)) => {
                        let serial = core::str::from_utf8(&serial[..len]).unwrap_or("<non-ascii>");
                        println!("SPS30 found, serial number: {}", serial);

                        // The sensor rejects start_measurement while it is
                        // already measuring, so return it to idle first.
                        let _ = sensor.stop_measurement().await;

                        match sensor.start_measurement().await {
                            Ok(()) => {
                                println!("SPS30: measurement started");
                                true
                            }
                            Err(error) => {
                                println!("SPS30: start_measurement failed: {:?}", error);
                                false
                            }
                        }
                    }
                    Err(error) => {
                        println!("SPS30: initialisation failed: {:?}", error);
                        false
                    }
                }
            } else {
                true
            };

            if initialized {
                Some(read_when_ready(&mut sensor).await)
            } else {
                None
            }
        };

        match result {
            Some(Ok(measurement)) => {
                needs_init = false;
                println!(
                    "PM1.0: {} ug/m3, PM2.5: {} ug/m3, PM4.0: {} ug/m3, PM10: {} ug/m3",
                    measurement.pm1_0, measurement.pm2_5, measurement.pm4_0, measurement.pm10
                );
                println!(
                    "NC0.5: {} #/cm3, NC1.0: {} #/cm3, NC2.5: {} #/cm3, NC4.0: {} #/cm3, NC10: {} #/cm3",
                    measurement.nc0_5,
                    measurement.nc1_0,
                    measurement.nc2_5,
                    measurement.nc4_0,
                    measurement.nc10
                );
                println!(
                    "Typical particle size: {} um",
                    measurement.typical_particle_size
                );
            }
            Some(Err(error)) => {
                needs_init = true;
                println!("SPS30: measurement failed: {:?}, recovering bus", error);
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
