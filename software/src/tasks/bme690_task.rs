//! BME690 read-out, scheduled and interpreted by the BSEC fusion library.
//!
//! The BME690 measures temperature, pressure, humidity and the resistance of a
//! heated gas-sensing film. Only the first three are directly meaningful; the
//! gas resistance has to be interpreted against a baseline learned from the
//! environment, which is what [`crate::drivers::bsec`] does. That library also
//! decides how the sensor is operated, so this task is a loop around it rather
//! than a fixed measurement schedule:
//!
//! 1. Ask BSEC how to configure the sensor now, and when to come back.
//! 2. Apply those settings and run one forced-mode measurement.
//! 3. Compensate the ADC counts with the sensor's factory calibration.
//! 4. Feed the result back into BSEC and publish what it returns.
//! 5. Sleep until the time BSEC asked for.
//!
//! The sensor keeps its configuration in registers until it is reset, but the
//! heater target changes from cycle to cycle, so the relevant registers are
//! rewritten every time round.

use embassy_time::{Instant, Timer};
use esp_println::println;
use static_cell::StaticCell;

use crate::drivers::bme690::{
    Bme690, Calibration, CompensatedMeasurement, Configuration, Error, GasConfig, HeaterProfile,
    IirFilterCoefficient, Mode, Oversampling,
};
use crate::drivers::bsec::{config, ffi, BmeSettings, Bsec, InputSet, Instance, Outputs, SampleRate};
use crate::drivers::i2c_bus::SharedI2cBus;
use crate::utils::flash_store::FlashStore;
use crate::utils::shared_state;

/// Rate BSEC is run at.
///
/// [`SampleRate::LowPower`] measures every three seconds and is the only rate
/// that produces a TVOC estimate. It has to agree with the tuning blob below,
/// because the blob encodes the rate it was tuned for.
const SAMPLE_RATE: SampleRate = SampleRate::LowPower;

/// Tuning blob matching [`SAMPLE_RATE`], a 3.3 V sensor supply and a four-day
/// baseline horizon.
const CONFIGURATION_BLOB: &[u8] = &config::IAQ_33V_3S_4D;

/// How long one BSEC cycle lasts at [`SAMPLE_RATE`], in milliseconds.
const CYCLE_INTERVAL_MS: u64 = 3000;

/// How many BSEC cycles pass between two published readings.
///
/// Every cycle has to run, because BSEC's estimates depend on being fed a
/// steady stream of measurements, but retaining all of them would fill the
/// history with far more detail than indoor air quality ever changes at, and
/// cost the PSRAM to match.
const CYCLES_PER_PUBLISHED_READING: u64 = 2;

/// Time between two published readings.
///
/// This is what the retained history is sized against and what the web
/// interface reports as the sensor's interval. It is not how often the sensor
/// is measured, which is every [`CYCLE_INTERVAL_MS`].
pub const MEASUREMENT_INTERVAL_MS: u64 = CYCLE_INTERVAL_MS * CYCLES_PER_PUBLISHED_READING;

/// Idle time before retrying after a bus or sensor error.
const ERROR_RETRY_DELAY_MS: u64 = 1000;

/// Flash partition BSEC's learned state is kept in.
///
/// Declared in `partitions.csv` and found by this name at runtime, so the
/// address is never written down twice.
const STATE_PARTITION_LABEL: &str = "bsec_state";

/// How long between two routine writes of the learned state.
///
/// A compromise between how much learning a power cut may cost and how often
/// the flash is erased. Six hours is under 1500 erase cycles over ten years of
/// continuous running, against a sector rated for around 100000.
const STATE_SAVE_INTERVAL_MS: u64 = 6 * 60 * 60 * 1000;

/// Heater profile slot BSEC's forced-mode settings are written to.
///
/// Forced mode runs exactly one profile, so which slot it is does not matter as
/// long as the same one is selected in `nb_conv`.
const HEATER_PROFILE_INDEX: u8 = 0;

/// IIR filter applied to the temperature and pressure channels.
///
/// Left off deliberately. BSEC does its own filtering and expects to see what
/// the sensor actually measured; smoothing the signal first would hide the
/// short-term detail its models are built around. Bosch's reference integration
/// likewise leaves the filter at its post-reset default, which is off.
const IIR_FILTER: IirFilterCoefficient = IirFilterCoefficient::Off;

/// Oversampling used for the reset-time configuration only.
///
/// Every cycle overwrites these with what BSEC asks for, so the values here
/// only decide the very first register write.
const INITIAL_CONFIGURATION: Configuration = Configuration {
    humidity_oversampling: Oversampling::X1,
    temperature_oversampling: Oversampling::X1,
    pressure_oversampling: Oversampling::X1,
    iir_filter: IIR_FILTER,
};

/// Ambient temperature assumed before the first measurement has been taken.
///
/// The heater is driven to its target *relative* to the temperature the sensor
/// sits in, so this value biases the plate temperature until a real reading
/// replaces it.
const INITIAL_AMBIENT_CELSIUS: f32 = 25.0;

/// Reset the sensor, confirm its identity, and read its factory calibration.
///
/// The oversampling written here is immediately superseded by the first cycle's
/// settings; what matters is that the sensor is back in a known state. Returns
/// `None` if any step failed, in which case the caller should retry later.
async fn initialize_sensor(bus: &SharedI2cBus) -> Option<Calibration> {
    let mut bus = bus.lock().await;
    let mut i2c = bus.acquire();
    let mut sensor = Bme690::new(&mut i2c);

    let result = async {
        sensor.init(&INITIAL_CONFIGURATION).await?;
        // Read once and hand back to the caller: the driver is rebuilt on
        // every bus lock, so it cannot hold the coefficients itself.
        sensor.read_calibration().await
    }
    .await;

    match result {
        Ok(calibration) => Some(calibration),
        Err(error) => {
            println!("BME690: initialisation failed: {:?}", error);
            None
        }
    }
}

/// Apply one cycle's BSEC settings and take the measurement they ask for.
///
/// The gas-validity flag is returned alongside the reading because it decides
/// on its own whether the gas resistance may be fed back: the temperature,
/// pressure and humidity of the same measurement stay usable either way.
///
/// Returns `None` when BSEC asked for a cycle that takes no measurement, which
/// is normal.
async fn run_cycle(
    bus: &SharedI2cBus,
    settings: &BmeSettings,
    calibration: &Calibration,
    ambient_celsius: f32,
) -> Result<Option<(CompensatedMeasurement, bool)>, Error> {
    let mut bus = bus.lock().await;
    let mut i2c = bus.acquire();
    let mut sensor = Bme690::new(&mut i2c);

    if settings.op_mode == ffi::OP_MODE_SLEEP {
        sensor.set_mode(Mode::Sleep).await?;
        return Ok(None);
    }

    // Anything other than sleep or forced mode is parallel mode, which only the
    // gas-scanning BSEC builds ask for and which this driver does not implement.
    if settings.op_mode != ffi::OP_MODE_FORCED {
        return Err(Error::UnsupportedOperatingMode(settings.op_mode));
    }

    let configuration = Configuration {
        humidity_oversampling: Oversampling::from_raw(settings.humidity_oversampling)
            .ok_or(Error::InvalidOversampling(settings.humidity_oversampling))?,
        temperature_oversampling: Oversampling::from_raw(settings.temperature_oversampling)
            .ok_or(Error::InvalidOversampling(settings.temperature_oversampling))?,
        pressure_oversampling: Oversampling::from_raw(settings.pressure_oversampling)
            .ok_or(Error::InvalidOversampling(settings.pressure_oversampling))?,
        iir_filter: IIR_FILTER,
    };

    sensor
        .set_oversampling(
            configuration.humidity_oversampling,
            configuration.temperature_oversampling,
            configuration.pressure_oversampling,
        )
        .await?;

    let run_gas = settings.run_gas != 0;

    // The heater target moves from cycle to cycle, and so does the ambient
    // temperature it is measured against, so both registers are rewritten every
    // time rather than programmed once at start-up.
    sensor
        .set_heater_profile(
            &HeaterProfile {
                index: HEATER_PROFILE_INDEX,
                target_temperature_celsius: settings.heater_temperature,
                duration_ms: settings.heater_duration,
            },
            calibration,
            ambient_celsius as i8,
        )
        .await?;

    sensor
        .set_gas_config(&GasConfig {
            heater_enabled: run_gas,
            gas_measurement_enabled: run_gas,
            heater_profile: HEATER_PROFILE_INDEX,
        })
        .await?;

    if settings.trigger_measurement == 0 {
        return Ok(None);
    }

    let heater_duration_ms = if run_gas { settings.heater_duration } else { 0 };
    let measurement = sensor
        .measure_forced(&configuration, heater_duration_ms)
        .await?;

    Ok(Some((
        calibration.compensate(&measurement),
        measurement.gas_measurement_valid,
    )))
}

/// Collect the measurement into the inputs BSEC asked for this cycle.
fn build_inputs(
    settings: &BmeSettings,
    measurement: &CompensatedMeasurement,
    gas_valid: bool,
    timestamp_ns: i64,
) -> InputSet {
    let mut inputs = InputSet::new(timestamp_ns);

    inputs.push_if_requested(
        settings,
        ffi::INPUT_TEMPERATURE,
        measurement.temperature_celsius,
    );
    inputs.push_if_requested(
        settings,
        ffi::INPUT_HUMIDITY,
        measurement.relative_humidity_percent,
    );
    inputs.push_if_requested(settings, ffi::INPUT_PRESSURE, measurement.pressure_pascals);

    // A gas resistance the sensor itself flagged as invalid would be taken as a
    // real reading and pull the learned baseline with it.
    if gas_valid {
        inputs.push_if_requested(
            settings,
            ffi::INPUT_GASRESISTOR,
            measurement.gas_resistance_ohms,
        );
        // Which step of the heater profile the reading came from. Forced mode
        // runs a single step, so it is always the first one.
        inputs.push_if_requested(settings, ffi::INPUT_PROFILE_PART, 0.0);
    }

    // How much the board heats the sensor above the true ambient temperature.
    // BSEC subtracts it to produce the heat-compensated temperature.
    inputs.push_if_requested(
        settings,
        ffi::INPUT_HEATSOURCE,
        SAMPLE_RATE.self_heating_celsius(),
    );

    inputs
}

/// Print one reading.
///
/// `core` has no floating-point formatting, so every value with a fractional
/// part is scaled to an integer pair first.
fn report(outputs: &Outputs) {
    let temperature_centi = (outputs.temperature_celsius * 100.0) as i32;
    let humidity_centi = (outputs.relative_humidity_percent * 100.0) as i32;
    let iaq_deci = (outputs.iaq * 10.0) as i32;

    println!(
        "BME690: {}{}.{:02} C, {} Pa, {}.{:02} %RH, {} ohm | IAQ {}.{} (accuracy {}), \
         CO2eq {} ppm, TVOC {} ppb, gas {} %{}{}",
        if temperature_centi < 0 { "-" } else { "" },
        (temperature_centi / 100).abs(),
        (temperature_centi % 100).unsigned_abs(),
        outputs.pressure_pascals as u32,
        humidity_centi / 100,
        (humidity_centi % 100).unsigned_abs(),
        outputs.gas_resistance_ohms as u32,
        iaq_deci / 10,
        (iaq_deci % 10).unsigned_abs(),
        outputs.iaq_accuracy.as_raw(),
        outputs.co2_equivalent_ppm as u32,
        outputs.tvoc_equivalent_ppb as u32,
        outputs.gas_percentage as i32,
        if outputs.run_in_complete {
            ""
        } else {
            ", running in"
        },
        if outputs.stabilized {
            ""
        } else {
            ", stabilising"
        },
    );
}

/// Open the flash partition the learned state lives in.
///
/// Returns `None` if it is not there, which means the device was flashed
/// without the project's partition table. That is worth saying out loud,
/// because everything else still works and the only symptom is that the
/// calibration is silently relearned after every reboot.
fn open_state_store() -> Option<FlashStore> {
    match FlashStore::open(STATE_PARTITION_LABEL) {
        Ok(store) => Some(store),
        Err(error) => {
            println!(
                "BSEC: no '{}' flash partition ({:?}); the baseline will not survive a reboot",
                STATE_PARTITION_LABEL, error
            );
            None
        }
    }
}

/// Hand BSEC the state saved before the last reboot, if there is one.
///
/// Must run after the tuning blob is applied and before the subscription is
/// made, which is the order Bosch's reference integration uses.
fn restore_state(bsec: &mut Bsec, store: &mut FlashStore) {
    let mut buffer = [0u8; ffi::MAX_STATE_BLOB_SIZE];

    match store.load(&mut buffer) {
        Ok(Some(state)) => match bsec.restore_state(state) {
            Ok(()) => println!("BSEC: restored {} bytes of learned state", state.len()),
            // The blob was readable but BSEC would not take it, which happens
            // when the library version or the tuning blob changed since it was
            // written. Starting from nothing is the correct response.
            Err(error) => println!("BSEC: saved state rejected: {:?}", error),
        },
        Ok(None) => println!("BSEC: no saved state; learning from scratch"),
        Err(error) => println!("BSEC: could not read saved state: {:?}", error),
    }
}

/// Serialise BSEC's learned state and write it to the flash.
///
/// Reported rather than propagated: a failed save costs the calibration on the
/// next reboot and nothing else, so it must not stop the sensor being read.
fn save_state(bsec: &mut Bsec, store: &mut FlashStore) {
    let mut buffer = [0u8; ffi::MAX_STATE_BLOB_SIZE];

    let state = match bsec.save_state(&mut buffer) {
        Ok(length) => &buffer[..length],
        Err(error) => {
            println!("BSEC: could not serialise state: {:?}", error);
            return;
        }
    };

    match store.save(state) {
        Ok(()) => println!("BSEC: saved {} bytes of learned state", state.len()),
        Err(error) => println!("BSEC: could not write state to flash: {:?}", error),
    }
}

/// Set up BSEC, or report why it could not be set up.
///
/// A failure here is not recoverable by retrying: it means the library was
/// linked or configured wrongly, not that a device misbehaved.
fn initialize_bsec(store: Option<&mut FlashStore>) -> Option<Bsec> {
    // Around 7 KiB of instance and scratch memory. It has to outlive every call
    // into BSEC and is far too large for the task's own stack frame.
    static INSTANCE: StaticCell<Instance> = StaticCell::new();

    let mut bsec = match Bsec::new(INSTANCE.init(Instance::new())) {
        Ok(bsec) => bsec,
        Err(error) => {
            println!("BSEC: initialisation failed: {:?}", error);
            return None;
        }
    };

    // Reading the version proves the prebuilt archive was linked in and that
    // its calling convention matches; a mismatch would otherwise only show up
    // as nonsensical sensor settings much later.
    let version = bsec.version();
    println!(
        "BSEC {}.{}.{}.{} linked",
        version.major, version.minor, version.major_bugfix, version.minor_bugfix
    );

    if let Err(error) = bsec.set_configuration(CONFIGURATION_BLOB) {
        println!("BSEC: configuration rejected: {:?}", error);
        return None;
    }

    // Between the configuration and the subscription: the state describes an
    // algorithm already tuned by the blob, and it has to be in place before
    // BSEC works out what to compute.
    if let Some(store) = store {
        restore_state(&mut bsec, store);
    }

    if let Err(error) = bsec.subscribe(SAMPLE_RATE) {
        println!("BSEC: subscription rejected: {:?}", error);
        return None;
    }

    Some(bsec)
}

/// Run the BME690 through BSEC, publishing what it reports.
#[embassy_executor::task]
pub async fn measure_task(bus: &'static SharedI2cBus) {
    let mut store = open_state_store();

    let Some(mut bsec) = initialize_bsec(store.as_mut()) else {
        // Without BSEC there is nothing this task can do, and retrying would
        // fail the same way every time.
        return;
    };

    // Cleared on every bus error so the next pass re-runs the sensor's reset
    // sequence instead of assuming it is still in a known state. Also holds the
    // coefficients, which the driver cannot keep between bus locks.
    let mut calibration: Option<Calibration> = None;
    // Carried between cycles: the heater target is relative to the temperature
    // the sensor sits in, and the best estimate of that is the last reading.
    let mut ambient_celsius = INITIAL_AMBIENT_CELSIUS;
    // BSEC only returns an output when it recomputes it, so results accumulate
    // here rather than being rebuilt from scratch each cycle.
    let mut outputs = Outputs::default();
    // Counts the cycles still to be skipped before the next reading is stored.
    let mut cycles_until_publish = 0u64;
    // When the learned state was last written to the flash, and the accuracy it
    // stood at then. Both are needed: the timer bounds how much learning a
    // power cut can cost, and the accuracy captures a hard-won improvement
    // immediately rather than leaving it unsaved for hours.
    //
    // `None` until the first output arrives. Restoring a state from the flash
    // brings its accuracy back with it, and treating that as an improvement
    // would erase a flash sector on every boot to store what was just read out
    // of it. The first output is adopted rather than saved for that reason.
    let mut state_saved_at = Instant::now();
    let mut saved_accuracy: Option<u8> = None;

    loop {
        // Whether this pass got anything new out of BSEC. Only then is there a
        // point in reconsidering the saved state.
        let mut fresh_outputs = false;

        // `None` means the sensor still has to be set up, either on the first
        // pass or after a bus error cleared it.
        let coefficients = match calibration {
            Some(coefficients) => coefficients,
            None => match initialize_sensor(bus).await {
                Some(coefficients) => {
                    calibration = Some(coefficients);
                    coefficients
                }
                None => {
                    Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
                    continue;
                }
            },
        };

        // BSEC dates its samples in nanoseconds from an arbitrary origin and
        // requires them to increase strictly. The device's uptime satisfies
        // both, and is the same clock the sleep at the end is scheduled on.
        let timestamp_ns = Instant::now().as_micros() as i64 * 1000;

        let settings = match bsec.sensor_control(timestamp_ns) {
            Ok(settings) => settings,
            Err(error) => {
                println!("BSEC: sensor control failed: {:?}", error);
                Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
                continue;
            }
        };

        match run_cycle(bus, &settings, &coefficients, ambient_celsius).await {
            Ok(Some((measurement, gas_valid))) => {
                ambient_celsius = measurement.temperature_celsius;

                let inputs = build_inputs(&settings, &measurement, gas_valid, timestamp_ns);
                match bsec.do_steps(inputs.as_slice(), &mut outputs) {
                    // A cycle can legitimately produce nothing: BSEC returns an
                    // output only once it has recomputed it.
                    Ok(0) => {}
                    Ok(_) => {
                        if cycles_until_publish == 0 {
                            shared_state::publish_bme690(outputs).await;
                            cycles_until_publish = CYCLES_PER_PUBLISHED_READING - 1;
                        } else {
                            cycles_until_publish -= 1;
                        }
                        report(&outputs);
                        fresh_outputs = true;
                    }
                    Err(error) => println!("BSEC: processing failed: {:?}", error),
                }
            }
            // BSEC asked for a cycle that takes no measurement.
            Ok(None) => {}
            Err(error) => {
                calibration = None;
                println!("BME690: read failed: {:?}, recovering bus", error);
                Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
                continue;
            }
        }

        // BSEC schedules itself: it expects the next call at the moment it
        // named, on the same clock the time stamps came from. Drifting late
        // blurs its notion of how much time has passed between readings.
        let next_call = Instant::from_micros((settings.next_call / 1000) as u64);

        // Saving stops the world for the length of a flash erase, so it is done
        // here, in the gap BSEC left before the next measurement, rather than
        // anywhere in the middle of one. Only cycles that produced an output
        // are considered, because `outputs` is otherwise whatever the last one
        // left behind.
        if let (Some(store), true) = (store.as_mut(), fresh_outputs) {
            let accuracy = outputs.iaq_accuracy.as_raw();

            match saved_accuracy {
                // Nothing has been learned since the state that is already in
                // the flash, so there is nothing to write. Without this, a
                // restored state would look like an improvement over the
                // starting value and erase a sector on every boot to store
                // what had just been read out of it.
                None => saved_accuracy = Some(accuracy),
                Some(saved) => {
                    let overdue = Instant::now()
                        .saturating_duration_since(state_saved_at)
                        .as_millis()
                        >= STATE_SAVE_INTERVAL_MS;

                    if accuracy > saved || overdue {
                        save_state(&mut bsec, store);
                        state_saved_at = Instant::now();
                        // Raised, never lowered. Accuracy can fall as well as
                        // rise, and following it down would let an oscillation
                        // between two values erase a sector every time it came
                        // back up. Kept as it is, the accuracy can account for
                        // at most three writes over the life of a boot.
                        saved_accuracy = Some(saved.max(accuracy));
                    }
                }
            }
        }

        Timer::at(next_call).await;
    }
}
