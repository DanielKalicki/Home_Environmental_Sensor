//! Periodic SPS30 particulate-matter read-out.

use embassy_time::{Duration, Ticker, Timer};
use esp_println::println;

use crate::drivers::i2c_bus::SharedI2cBus;
use crate::drivers::sps30::{Error, Measurement, Sps30};
use crate::utils::shared_state;

/// Time between scheduled read-out deadlines.
pub const MEASUREMENT_INTERVAL_MS: u64 = 5000;
/// Idle time before retrying after a bus or sensor error.
const ERROR_RETRY_DELAY_MS: u64 = 1000;
/// How long to wait for the sensor to flag a fresh result.
const DATA_READY_POLL_ATTEMPTS: usize = 30;
const DATA_READY_POLL_DELAY_MS: u64 = 100;

/// Bring the sensor into a known state and verify it responds.
///
/// Returns `false` if any check failed, in which case the caller should
/// retry later.
async fn initialize(bus: &SharedI2cBus) -> bool {
    let mut bus = bus.lock().await;
    let mut i2c = bus.acquire();
    let mut sensor = Sps30::new(&mut i2c);

    let (serial, len) = match sensor.serial_number().await {
        Ok(serial) => serial,
        Err(error) => {
            println!("SPS30: initialisation failed: {:?}", error);
            return false;
        }
    };
    let serial = core::str::from_utf8(&serial[..len]).unwrap_or("<non-ascii>");
    println!("SPS30 found, serial number: {}", serial);

    // The sensor rejects start_measurement while it is already measuring,
    // so return it to idle first.
    let _ = sensor.stop_measurement().await;

    match sensor.start_measurement().await {
        Ok(()) => {
            println!("SPS30: measurement started");
            true
        }
        Err(error) => {
            println!("SPS30: initialisation failed: {:?}", error);
            false
        }
    }
}

/// Wait for the sensor to flag a fresh result, then read it.
///
/// If no flag appears within the poll window the last values are read anyway;
/// a genuine bus problem still surfaces as an error from the read itself.
async fn read_once(bus: &'static SharedI2cBus) -> Result<Measurement, Error> {
    for _ in 0..DATA_READY_POLL_ATTEMPTS {
        let ready = {
            let mut bus = bus.lock().await;
            let mut i2c = bus.acquire();
            let mut sensor = Sps30::new(&mut i2c);
            sensor.is_data_ready().await?
        };

        if ready {
            break;
        }
        Timer::after_millis(DATA_READY_POLL_DELAY_MS).await;
    }

    let mut bus = bus.lock().await;
    let mut i2c = bus.acquire();
    let mut sensor = Sps30::new(&mut i2c);
    sensor.read_measured_values().await
}

/// Periodically read the SPS30 and print the result.
///
/// The shared bus is held only for individual transfers. In particular, it is
/// released between readiness polls and while waiting for the next deadline.
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
            Ok(measurement) => {
                needs_init = false;
                shared_state::publish_sps30(measurement).await;
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
                println!("SPS30: measurement failed: {:?}, recovering bus", error);
                Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
            }
        }
    }
}
