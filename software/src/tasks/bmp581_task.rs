//! Periodic BMP581 pressure / temperature read-out.
//!
//! Every cycle runs one forced measurement, prints it to the serial console and
//! appends it to the retained history the web server serves.

use embassy_time::{Duration, Ticker, Timer};
use esp_println::println;

use crate::drivers::bmp581::{
    Bmp581, Configuration, Error, Identification, IirFilter, Measurement, Oversampling,
};
use crate::drivers::i2c_bus::SharedI2cBus;
use crate::utils::shared_state;

/// Oversampling and filtering every measurement is taken with.
///
/// Pressure carries the noise that matters here, so it is oversampled 16-fold
/// while temperature, which is only read for its own sake, is oversampled
/// twice. Both IIR filters are bypassed: the filter smooths consecutive
/// measurements together, and with one forced measurement every
/// [`MEASUREMENT_INTERVAL_MS`] its state would be seconds old and would only
/// blur real changes. `flush_filter_on_forced` is set all the same so that the
/// bypass cannot be undone by leftover filter state.
const MEASUREMENT_CONFIGURATION: Configuration = Configuration {
    temperature_oversampling: Oversampling::X2,
    pressure_oversampling: Oversampling::X16,
    pressure_enabled: true,
    temperature_filter: IirFilter::Bypass,
    pressure_filter: IirFilter::Bypass,
    flush_filter_on_forced: true,
};

/// Time between the starts of two measurements.
///
/// Public because the retained history is sized from it: a full day of
/// readings at this interval is what the ring buffer has to hold.
pub const MEASUREMENT_INTERVAL_MS: u64 = 10_000;

/// Idle time before retrying after a bus or sensor error.
const ERROR_RETRY_DELAY_MS: u64 = 1000;

/// Reset the sensor, confirm its identity and apply the configuration.
///
/// The shared bus is held for the whole sequence. Returns `false` if any step
/// failed, in which case the caller should retry later.
async fn initialize(bus: &SharedI2cBus) -> bool {
    let mut bus = bus.lock().await;
    let mut i2c = bus.acquire();
    let mut sensor = Bmp581::new(&mut i2c);

    let result = async {
        let identification = sensor.identification().await?;
        sensor.init(&MEASUREMENT_CONFIGURATION).await?;
        Ok::<Identification, Error>(identification)
    }
    .await;

    match result {
        Ok(identification) => {
            println!(
                "BMP581 ready, chip id: {:#04X}, revision: {:#04X}",
                identification.chip_id, identification.rev_id
            );
            println!(
                "BMP581 configuration: pressure oversampling: {}x, temperature oversampling: {}x",
                MEASUREMENT_CONFIGURATION.pressure_oversampling.factor(),
                MEASUREMENT_CONFIGURATION.temperature_oversampling.factor()
            );
            true
        }
        Err(error) => {
            println!("BMP581: initialisation failed: {:?}", error);
            false
        }
    }
}

/// Run one forced measurement.
///
/// Unlike the other sensor tasks this one keeps the shared bus for the whole
/// conversion instead of releasing it in between: a forced measurement at the
/// configured oversampling finishes in a few milliseconds, which is shorter
/// than the bus recovery a second lock would have to repeat.
async fn read_once(bus: &SharedI2cBus) -> Result<Measurement, Error> {
    let mut bus = bus.lock().await;
    let mut i2c = bus.acquire();
    let mut sensor = Bmp581::new(&mut i2c);
    sensor.measure().await
}

/// Periodically read the BMP581, print the result and retain it.
///
/// Measurement starts follow a fixed [`MEASUREMENT_INTERVAL_MS`] schedule.
#[embassy_executor::task]
pub async fn measure_task(bus: &'static SharedI2cBus) {
    // Set on every bus error so the next cycle re-runs the sensor's init
    // sequence instead of assuming its configuration survived.
    let mut needs_init = true;
    let mut ticker = Ticker::every(Duration::from_millis(MEASUREMENT_INTERVAL_MS));

    loop {
        if needs_init {
            if !initialize(bus).await {
                Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
                continue;
            }
        }
        let reinitialized = needs_init;

        match read_once(bus).await {
            Ok(measurement) => {
                needs_init = false;

                shared_state::publish_bmp581(measurement).await;

                println!(
                    "BMP581 pressure: {} hPa, temperature: {} C",
                    measurement.pressure_hectopascals(),
                    measurement.temperature_celsius()
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
                println!("BMP581: measurement failed: {:?}, recovering bus", error);
                Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
            }
        }
    }
}
