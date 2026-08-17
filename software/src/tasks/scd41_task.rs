//! Periodic SCD41 CO2 / temperature / humidity read-out.

use embassy_time::{Duration, Ticker, Timer};
use esp_hal::peripherals::I2C0;
use esp_println::println;

use crate::drivers::i2c_bus::SharedI2cBus;
use crate::drivers::scd41::{
    Error, Measurement, Scd41, SensorVariant, SINGLE_SHOT_MEASUREMENT_DELAY_MS,
};
use crate::utils::shared_state;

/// Time between the starts of two single-shot conversions.
pub const MEASUREMENT_INTERVAL_MS: u64 = 10_000;
/// Idle time before retrying after a bus or sensor error.
const ERROR_RETRY_DELAY_MS: u64 = 1000;
/// How many readings are dropped after the sensor is started.
///
/// The first single-shot conversion runs before the sensor's own temperature
/// has settled, so its result is not representative. Discarded readings are
/// still read and printed, they are only kept out of the published history, so
/// one unsettled value cannot distort the charts. The count restarts after
/// every re-initialisation, including the one following a bus error. Set to 0
/// to publish every reading.
const DISCARDED_WARMUP_READINGS: u32 = 1;

/// Checks run against an idle sensor before measurements are started.
///
/// New start-up checks belong here; the first failing one aborts the sequence.
async fn run_init_checks(sensor: &mut Scd41<'_, '_, I2C0>) -> Result<(), Error> {
    sensor.stop_periodic_measurement().await?;

    let serial = sensor.serial_number().await?;
    let variant = sensor.sensor_variant().await?;
    let temperature_offset = sensor.temperature_offset_celsius().await?;
    let altitude = sensor.sensor_altitude_meters().await?;
    let ambient_pressure = sensor.ambient_pressure_pascals().await?;
    let asc_enabled = sensor.automatic_self_calibration_enabled().await?;
    let asc_target = sensor.automatic_self_calibration_target_ppm().await?;
    let asc_initial_period = sensor
        .automatic_self_calibration_initial_period_hours()
        .await?;
    let asc_standard_period = sensor
        .automatic_self_calibration_standard_period_hours()
        .await?;

    println!(
        "SCD41 ready, serial number: 0x{:012X}, variant: {:?}",
        serial, variant
    );
    println!(
        "SCD41 configuration: temperature offset: {} C, altitude: {} m, ambient pressure: {} Pa",
        temperature_offset, altitude, ambient_pressure
    );
    println!(
        "SCD41 self-calibration: {}, target: {} ppm",
        if asc_enabled { "enabled" } else { "disabled" },
        asc_target
    );
    println!(
        "SCD41 self-calibration periods: initial: {} h, standard: {} h",
        asc_initial_period, asc_standard_period
    );
    if variant != SensorVariant::Scd41 {
        println!(
            "SCD41: warning, attached sensor reports variant {:?}",
            variant
        );
    }

    Ok(())
}

/// Bring the sensor into a known state and verify it responds.
///
/// The shared bus is held for the whole sequence. Returns `false` if any
/// check failed, in which case the caller should retry later.
async fn initialize(bus: &SharedI2cBus) -> bool {
    let mut bus = bus.lock().await;
    let mut i2c = bus.acquire();
    let mut sensor = Scd41::new(&mut i2c);

    match run_init_checks(&mut sensor).await {
        Ok(()) => true,
        Err(error) => {
            println!("SCD41: initialisation failed: {:?}", error);
            false
        }
    }
}

/// Run one single-shot conversion cycle and return the result.
///
/// The shared bus is released during the sensor's 5 second conversion,
/// allowing the SPS30 task to use it meanwhile.
async fn read_once(bus: &'static SharedI2cBus) -> Result<Measurement, Error> {
    {
        let mut bus = bus.lock().await;
        let mut i2c = bus.acquire();
        let mut sensor = Scd41::new(&mut i2c);
        sensor.start_single_shot()?;
    }

    Timer::after_millis(SINGLE_SHOT_MEASUREMENT_DELAY_MS).await;

    let mut bus = bus.lock().await;
    let mut i2c = bus.acquire();
    let mut sensor = Scd41::new(&mut i2c);
    sensor.read_measurement().await
}

/// Periodically read the SCD41 and print the result.
///
/// Conversion starts follow a fixed 10 second schedule.
#[embassy_executor::task]
pub async fn measure_task(bus: &'static SharedI2cBus) {
    // Set on every bus error so the next cycle re-runs the sensor's init
    // sequence instead of assuming it is still in the idle state.
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
                    shared_state::publish_scd41(measurement).await;
                }

                println!(
                    "CO2: {} ppm, temperature: {} C, humidity: {} %{}",
                    measurement.co2_ppm,
                    measurement.temperature_celsius(),
                    measurement.humidity_percent(),
                    if settling {
                        " (warm-up, discarded)"
                    } else {
                        ""
                    }
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
                println!("SCD41: measurement failed: {:?}, recovering bus", error);
                Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
            }
        }
    }
}
