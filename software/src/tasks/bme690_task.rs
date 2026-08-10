//! Periodic BME690 forced-mode read-out.
//!
//! Each cycle triggers a single measurement, waits out the conversion and
//! heater time, converts the ADC counts into physical units with the sensor's
//! factory calibration, and publishes the result to the shared history. The
//! sensor keeps its configuration in registers until it is reset, so the whole
//! setup sequence only has to be re-run after an error.

use embassy_time::{Duration, Ticker, Timer};
use esp_println::println;

use crate::drivers::bme690::{
    Bme690, Calibration, CompensatedMeasurement, Configuration, Error, GasConfig, HeaterProfile,
    IirFilterCoefficient, Measurement, Oversampling,
};
use crate::drivers::i2c_bus::SharedI2cBus;
use crate::utils::shared_state;

/// Time between measurements.
pub const MEASUREMENT_INTERVAL_MS: u64 = 5000;
/// Idle time before retrying after a bus or sensor error.
const ERROR_RETRY_DELAY_MS: u64 = 1000;
/// How many readings are dropped after the sensor is configured.
///
/// The heater plate has not reached its target on the first measurements, so
/// the gas resistance they report is far off: the first reading after start-up
/// was measured at 386 kOhm against a steady 11-16 kOhm, and it still set the
/// heater-stable flag, so that flag alone does not identify it. Discarded
/// readings are still taken and printed, they are only kept out of the
/// published history, so a single outlier cannot stretch the chart axis. The
/// count restarts after every re-initialisation, including the one following a
/// bus error. Set to 0 to publish every reading.
const DISCARDED_WARMUP_READINGS: u32 = 2;

/// Oversampling and filtering applied to every measurement.
const CONFIGURATION: Configuration = Configuration {
    humidity_oversampling: Oversampling::X1,
    temperature_oversampling: Oversampling::X2,
    pressure_oversampling: Oversampling::X16,
    // Smooth out short-term perturbations in the temperature/pressure output.
    iir_filter: IirFilterCoefficient::Coefficient7,
};

/// Heater profile slot used for the gas measurement.
const HEATER_PROFILE_INDEX: u8 = 0;
/// Heater target temperature in °C.
const HEATER_TARGET_CELSIUS: u16 = 300;
/// How long the heater stays on before the gas ADC is sampled.
const HEATER_DURATION_MS: u16 = 100;
/// Ambient temperature assumed when converting the heater target into a
/// register value. The heater is driven relative to the temperature the sensor
/// is sitting in, so this being a fixed guess biases the actual plate
/// temperature whenever the room is far from it.
const AMBIENT_TEMPERATURE_CELSIUS: i8 = 25;

/// Reset the sensor, apply the measurement configuration, and arm the heater.
///
/// The shared bus is held for the whole sequence. Returns the factory
/// calibration on success, or `None` if any step failed, in which case the
/// caller should retry later.
async fn initialize(bus: &SharedI2cBus) -> Option<Calibration> {
    let mut bus = bus.lock().await;
    let mut i2c = bus.acquire();
    let mut sensor = Bme690::new(&mut i2c);

    let result = async {
        sensor.init(&CONFIGURATION).await?;

        // Read once and hand back to the caller: the driver is rebuilt on
        // every bus lock, so it cannot hold the coefficients itself.
        let calibration = sensor.read_calibration().await?;
        sensor
            .set_heater_profile(
                &HeaterProfile {
                    index: HEATER_PROFILE_INDEX,
                    target_temperature_celsius: HEATER_TARGET_CELSIUS,
                    duration_ms: HEATER_DURATION_MS,
                },
                &calibration,
                AMBIENT_TEMPERATURE_CELSIUS,
            )
            .await?;

        sensor
            .set_gas_config(&GasConfig {
                heater_enabled: true,
                gas_measurement_enabled: true,
                heater_profile: HEATER_PROFILE_INDEX,
            })
            .await?;

        Ok::<Calibration, Error>(calibration)
    }
    .await;

    match result {
        Ok(calibration) => {
            println!("BME690 ready");
            Some(calibration)
        }
        Err(error) => {
            println!("BME690: initialisation failed: {:?}", error);
            None
        }
    }
}

/// Run one forced-mode measurement and compensate it.
///
/// The raw result is returned alongside the compensated one because the
/// validity and heater-stability flags only exist on the raw measurement.
async fn read_once(
    bus: &'static SharedI2cBus,
    calibration: &Calibration,
) -> Result<(Measurement, CompensatedMeasurement), Error> {
    let mut bus = bus.lock().await;
    let mut i2c = bus.acquire();
    let mut sensor = Bme690::new(&mut i2c);
    let measurement = sensor
        .measure_forced(&CONFIGURATION, HEATER_DURATION_MS)
        .await?;

    Ok((measurement, calibration.compensate(&measurement)))
}

/// Periodically run a forced-mode measurement, publish it, and print it.
#[embassy_executor::task]
pub async fn measure_task(bus: &'static SharedI2cBus) {
    // Cleared on every bus error so the next cycle re-runs the sensor's init
    // sequence instead of assuming it is still in the idle state. Also holds
    // the coefficients, which the driver cannot keep between bus locks.
    let mut calibration: Option<Calibration> = None;
    // Counts down the readings still to be dropped, restarted every time the
    // sensor is initialised.
    let mut warmup_remaining = DISCARDED_WARMUP_READINGS;
    let mut ticker = Ticker::every(Duration::from_millis(MEASUREMENT_INTERVAL_MS));

    loop {
        // `None` means the sensor still has to be set up, either on the first
        // pass or after a bus error cleared it.
        let (coefficients, reinitialized) = match calibration {
            Some(coefficients) => (coefficients, false),
            None => match initialize(bus).await {
                Some(coefficients) => {
                    calibration = Some(coefficients);
                    warmup_remaining = DISCARDED_WARMUP_READINGS;
                    (coefficients, true)
                }
                None => {
                    Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
                    continue;
                }
            },
        };

        match read_once(bus, &coefficients).await {
            Ok((measurement, compensated)) => {
                // Reported either way, but withheld from the history until the
                // heater has settled.
                let settling = warmup_remaining > 0;
                if settling {
                    warmup_remaining -= 1;
                } else {
                    shared_state::publish_bme690(compensated).await;
                }

                // `core` has no floating-point formatting, so the values are
                // scaled to hundredths and printed as integers.
                let temperature_centi = (compensated.temperature_celsius * 100.0) as i32;
                let humidity_centi = (compensated.relative_humidity_percent * 100.0) as i32;

                println!(
                    "BME690: {}{}.{:02} C, {} Pa, {}.{:02} %RH, {} ohm (valid {}, stable {}){}",
                    if temperature_centi < 0 { "-" } else { "" },
                    (temperature_centi / 100).abs(),
                    (temperature_centi % 100).unsigned_abs(),
                    compensated.pressure_pascals as u32,
                    humidity_centi / 100,
                    (humidity_centi % 100).unsigned_abs(),
                    compensated.gas_resistance_ohms as u32,
                    measurement.gas_measurement_valid,
                    measurement.heater_stable,
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
                calibration = None;
                println!("BME690: read failed: {:?}, recovering bus", error);
                Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
            }
        }
    }
}
