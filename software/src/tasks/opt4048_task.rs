//! Periodic OPT4048 colour / illuminance read-out.
//!
//! Every cycle runs one four-channel measurement, prints the derived
//! photometric and colorimetric values to the serial console, and appends the
//! measurement to the retained history the web server serves.
//!
//! What is stored is the raw per-channel ADC result, not the derived lux or
//! chromaticity. Those are cheap to recompute from the stored channels, and
//! keeping the raw values means a later correction to the conversion
//! coefficients does not invalidate a day of history.

use embassy_time::{Duration, Ticker, Timer};
use esp_println::println;

use crate::drivers::i2c_bus::SharedI2cBus;
use crate::drivers::opt4048::{
    Channel, Configuration, ConversionTime, Error, Measurement, Opt4048, Range,
};
use crate::utils::shared_state;

/// Range and conversion time every measurement is taken with.
///
/// The range is left on automatic so the device picks its own exponent per
/// measurement: this sensor is pointed at a room whose light spans several
/// decades between daylight and a lamp at night, and a fixed range would
/// either clip at the top or quantise the bottom away. 100 ms per channel is
/// the device's own power-on conversion time and integrates long enough to
/// average out mains flicker at both 50 and 60 Hz.
const MEASUREMENT_CONFIGURATION: Configuration = Configuration {
    range: Range::Auto,
    conversion_time: ConversionTime::Ms100,
    latch_flags: true,
};

/// How long the device needs for one four-channel measurement.
///
/// Derived from the configuration above rather than written down separately,
/// so changing the conversion time cannot leave the wait out of step with it.
/// The device converts the four channels one after another, so this is four
/// times the per-channel conversion time.
const MEASUREMENT_TIME_US: u64 = MEASUREMENT_CONFIGURATION.measurement_time_us() as u64;

/// Time between the starts of two measurements.
///
/// Public because the retained history is sized from it: a full day of
/// readings at this interval is what the ring buffer has to hold.
pub const MEASUREMENT_INTERVAL_MS: u64 = 10_000;

/// Idle time before retrying after a bus or sensor error.
const ERROR_RETRY_DELAY_MS: u64 = 1000;

/// Confirm the device's identity and apply the configuration.
///
/// The shared bus is held for the whole sequence. Returns `false` if any step
/// failed, in which case the caller should retry later.
async fn initialize(bus: &SharedI2cBus) -> bool {
    let mut bus = bus.lock().await;
    let mut i2c = bus.acquire();
    let mut sensor = Opt4048::new(&mut i2c);

    let result = async {
        let device_id = sensor.device_id().await?;
        sensor.init(&MEASUREMENT_CONFIGURATION).await?;
        Ok::<u16, Error>(device_id)
    }
    .await;

    match result {
        Ok(device_id) => {
            println!("OPT4048 ready, device id: {:#06X}", device_id);
            println!(
                "OPT4048 configuration: range: {:?}, conversion time: {} us per channel, {} us per measurement",
                MEASUREMENT_CONFIGURATION.range,
                MEASUREMENT_CONFIGURATION.conversion_time.microseconds(),
                MEASUREMENT_CONFIGURATION.measurement_time_us()
            );
            true
        }
        Err(error) => {
            println!("OPT4048: initialisation failed: {:?}", error);
            false
        }
    }
}

/// Run one four-channel measurement.
///
/// The shared bus is released while the sensor converts, so the other sensor
/// tasks can use it during those ~400 ms; only the trigger and the read-out
/// hold it. Waiting the conversion time first also means `CONVERSION_READY` is
/// already set when the data are fetched, so the driver's poll loop runs
/// exactly once.
async fn read_once(bus: &SharedI2cBus) -> Result<Measurement, Error> {
    {
        let mut bus = bus.lock().await;
        let mut i2c = bus.acquire();
        let mut sensor = Opt4048::new(&mut i2c);
        sensor.start_measurement().await?;
    }

    Timer::after_micros(MEASUREMENT_TIME_US).await;

    let mut bus = bus.lock().await;
    let mut i2c = bus.acquire();
    let mut sensor = Opt4048::new(&mut i2c);
    sensor.read_measurement().await
}

/// Print the derived values of one measurement, and the raw counts behind them.
fn print_measurement(measurement: &Measurement) {
    match measurement.chromaticity() {
        Some(chromaticity) => match measurement.correlated_color_temperature_kelvin() {
            Some(cct) => println!(
                "OPT4048 light: {} lux, CIE x: {}, y: {}, CCT: {} K",
                measurement.lux(),
                chromaticity.x,
                chromaticity.y,
                cct
            ),
            None => println!(
                "OPT4048 light: {} lux, CIE x: {}, y: {}, CCT: undefined",
                measurement.lux(),
                chromaticity.x,
                chromaticity.y
            ),
        },
        // In darkness the tristimulus values sum to zero and the colour
        // coordinates have no meaning, but the illuminance still does.
        None => println!(
            "OPT4048 light: {} lux, colour undefined (too dark)",
            measurement.lux()
        ),
    }

    // The linear ADC codes the values above were derived from. The exponent is
    // printed alongside because with automatic ranging it is the sensor's own
    // report of how bright the scene was.
    println!(
        "OPT4048 channels: X: {}, Y: {}, Z: {}, wideband: {}, exponent: {}",
        measurement.adc_code(Channel::X),
        measurement.adc_code(Channel::Y),
        measurement.adc_code(Channel::Z),
        measurement.adc_code(Channel::Wideband),
        measurement.channel(Channel::Y).exponent
    );

    if measurement.overload {
        println!("OPT4048: overload, the input exceeded the full-scale range");
    }
}

/// Periodically read the OPT4048, print the result and retain it.
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

                shared_state::publish_opt4048(measurement).await;

                print_measurement(&measurement);

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
                println!("OPT4048: measurement failed: {:?}, recovering bus", error);
                Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
            }
        }
    }
}
