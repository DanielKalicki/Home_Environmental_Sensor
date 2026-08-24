//! Periodic BMM350 magnetic-field read-out.
//!
//! Every cycle runs one forced measurement, prints it to the serial console and
//! appends it to the retained history the web server serves.
//!
//! Unlike the other sensors the BMM350 compensates nothing on-chip. Its
//! per-chip coefficients are read out of the sensor's OTP block once, during
//! initialisation, and this task holds them for as long as that initialisation
//! stands: every raw reading has to be put through them before it means
//! anything. They are re-read whenever the sensor is re-initialised after an
//! error, because a bus error may well have been a sensor reset that cleared
//! the configuration those coefficients were read under.

use embassy_time::{Duration, Ticker, Timer};
use esp_println::println;

use crate::drivers::bmm350::{
    Averaging, Axes, Bmm350, Compensation, Configuration, DataRate, Error, Measurement,
};
use crate::drivers::i2c_bus::SharedI2cBus;
use crate::utils::shared_state;

/// Data rate and averaging every measurement is taken with.
///
/// The data rate only governs normal mode, which this task does not use, but it
/// still caps the averaging: the device silently lowers an averaging setting
/// that would not fit into the data-rate period. 25 Hz is slow enough to leave
/// eight-fold averaging intact, and eight-fold averaging is the quietest the
/// device offers, which is what a sensor sitting still in a room wants. All
/// three axes are measured, because a single-axis magnetic reading has no
/// meaning without the other two to give it a direction.
const MEASUREMENT_CONFIGURATION: Configuration = Configuration {
    data_rate: DataRate::Hz25,
    averaging: Averaging::X8,
    axes: Axes::ALL,
    data_ready_status_enabled: true,
};

/// Time between the starts of two measurements.
///
/// Public because the retained history is sized from it: a full day of
/// readings at this interval is what the ring buffer has to hold.
pub const MEASUREMENT_INTERVAL_MS: u64 = 10_000;

/// Idle time before retrying after a bus or sensor error.
const ERROR_RETRY_DELAY_MS: u64 = 1000;

/// How many readings are dropped after the sensor is configured.
///
/// Initialisation ends with a magnetic reset, which drives current through the
/// on-chip coil and leaves the die warmer than it will settle at. The die
/// temperature is not just reported, it also feeds the temperature-coefficient
/// terms of the magnetic compensation, so the first reading after a reset is
/// skewed on every channel. Discarded readings are still read and printed, they
/// are only kept out of the published history. The count restarts after every
/// re-initialisation, including the one following a bus error. Set to 0 to
/// publish every reading.
const DISCARDED_WARMUP_READINGS: u32 = 1;

/// Reset the sensor, confirm its identity, load its compensation data and apply
/// the configuration.
///
/// The shared bus is held for the whole sequence, which includes reading all 32
/// OTP words and a magnetic reset, so it is slower than the other sensors'
/// initialisation. Returns the compensation coefficients, or `None` if any step
/// failed, in which case the caller should retry later.
async fn initialize(bus: &SharedI2cBus) -> Option<Compensation> {
    let mut bus = bus.lock().await;
    let mut i2c = bus.acquire();
    let mut sensor = Bmm350::new(&mut i2c);

    match sensor.init(&MEASUREMENT_CONFIGURATION).await {
        Ok(compensation) => {
            println!(
                "BMM350 ready, variant id: {:#04X}, {}x averaging, all three axes",
                compensation.variant_id(),
                1u8 << MEASUREMENT_CONFIGURATION.averaging.raw()
            );
            Some(compensation)
        }
        Err(error) => {
            println!("BMM350: initialisation failed: {:?}", error);
            None
        }
    }
}

/// Run one forced measurement and compensate it.
///
/// Like the BMP581 task this keeps the shared bus for the whole conversion
/// instead of releasing it in between: at the configured averaging the
/// conversion finishes in under 30 ms, which is shorter than the bus recovery a
/// second lock would have to repeat.
async fn read_once(bus: &SharedI2cBus, compensation: &Compensation) -> Result<Measurement, Error> {
    let mut bus = bus.lock().await;
    let mut i2c = bus.acquire();
    let mut sensor = Bmm350::new(&mut i2c);
    sensor.measure_forced(compensation).await
}

/// Periodically read the BMM350, print the result and retain it.
///
/// Measurement starts follow a fixed [`MEASUREMENT_INTERVAL_MS`] schedule.
#[embassy_executor::task]
pub async fn measure_task(bus: &'static SharedI2cBus) {
    // The per-chip coefficients, held for as long as the initialisation they
    // were read under stands. `None` forces the next cycle to re-run that
    // initialisation instead of assuming the configuration survived.
    let mut compensation: Option<Compensation> = None;
    // Counts down the readings still to be dropped, restarted every time the
    // sensor is initialised.
    let mut warmup_remaining = DISCARDED_WARMUP_READINGS;
    let mut ticker = Ticker::every(Duration::from_millis(MEASUREMENT_INTERVAL_MS));

    loop {
        let reinitialized = compensation.is_none();
        if reinitialized {
            match initialize(bus).await {
                Some(loaded) => {
                    compensation = Some(loaded);
                    warmup_remaining = DISCARDED_WARMUP_READINGS;
                }
                None => {
                    Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
                    continue;
                }
            }
        }

        // Set just above, so this cannot fail.
        let Some(coefficients) = compensation else {
            continue;
        };

        match read_once(bus, &coefficients).await {
            Ok(measurement) => {
                // Reported either way, but withheld from the history until the
                // sensor has settled.
                let settling = warmup_remaining > 0;
                if settling {
                    warmup_remaining -= 1;
                } else {
                    shared_state::publish_bmm350(measurement).await;
                }

                println!(
                    "BMM350 field: x {} uT, y {} uT, z {} uT, die {} C{}",
                    measurement.x_microtesla,
                    measurement.y_microtesla,
                    measurement.z_microtesla,
                    measurement.temperature_celsius,
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
                compensation = None;
                println!("BMM350: measurement failed: {:?}, recovering bus", error);
                Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
            }
        }
    }
}
