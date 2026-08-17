//! Periodic AS7343 spectral read-out.
//!
//! Every cycle runs one 18-channel measurement, prints all of its raw ADC
//! counts to the serial console, and appends the measurement to the retained
//! history the web server serves. The console output is kept because it is
//! what the wiring, the gain and the integration time are judged from during
//! bring-up.

use embassy_time::{Duration, Ticker, Timer};
use esp_println::{print, println};

use crate::drivers::as7343::{
    As7343, Channel, Configuration, Error, Gain, Identification, Measurement, SPECTRAL_CHANNELS,
};
use crate::drivers::i2c_bus::SharedI2cBus;
use crate::utils::shared_state;

/// Gain and integration time every measurement is taken with.
///
/// ex:
/// `ATIME` = 29 and `ASTEP` = 599 give `(29 + 1) * (599 + 1) * 2.78 us` =
/// 50.04 ms per integration cycle. The 18-channel auto-SMUX sequence runs three
/// of those, so one measurement occupies the sensor for about 150 ms. 256x gain
/// is the device's own power-on gain and is a workable starting point for
/// indoor lighting; lower it if the readings saturate.
const MEASUREMENT_CONFIGURATION: Configuration = Configuration {
    gain: Gain::Gain1024x,
    atime: 254,
    astep: 256,
    // gain: Gain::Gain256x,
    // atime: 29,
    // astep: 599,
};

/// How long the device needs for one 18-channel measurement.
///
/// Derived from the configuration above rather than written down separately, so
/// changing the integration time cannot leave the wait out of step with it.
const MEASUREMENT_TIME_US: u64 = MEASUREMENT_CONFIGURATION.measurement_time_us() as u64;

/// Time between the starts of two measurements.
///
/// Public because the retained history is sized from it: a full day of
/// readings at this interval is what the ring buffer has to hold.
pub const MEASUREMENT_INTERVAL_MS: u64 = 5000;

/// Idle time before retrying after a bus or sensor error.
const ERROR_RETRY_DELAY_MS: u64 = 1000;

/// How many readings are dropped after the sensor is configured.
///
/// The first measurement after initialisation reports a gain that the sensor
/// was not set to: over 28 consecutive readings from this board, `ASTATUS`
/// decoded to 0.5x on the first and to the configured 256x on all 27 that
/// followed, while the counts of the first two differed by about a percent, so
/// a real 512-fold difference in gain is impossible. The first read-out is
/// therefore taken as not yet describing itself correctly and is kept out of
/// the history. Discarded readings are still read and printed, so the settling
/// is visible on the console. The count restarts after every
/// re-initialisation, including the one following a bus error. Set to 0 to
/// publish every reading.
const DISCARDED_WARMUP_READINGS: u32 = 1;

/// Reset the sensor, confirm its identity and apply the configuration.
///
/// The shared bus is held for the whole sequence. Returns `false` if any step
/// failed, in which case the caller should retry later.
async fn initialize(bus: &SharedI2cBus) -> bool {
    let mut bus = bus.lock().await;
    let mut i2c = bus.acquire();
    let mut sensor = As7343::new(&mut i2c);

    let result = async {
        let identification = sensor.identification().await?;
        sensor.init(&MEASUREMENT_CONFIGURATION).await?;
        Ok::<Identification, Error>(identification)
    }
    .await;

    match result {
        Ok(identification) => {
            print_identification(&identification);
            true
        }
        Err(error) => {
            println!("AS7343: initialisation failed: {:?}", error);
            false
        }
    }
}

/// Print what the device reports about itself and how it was configured.
fn print_identification(identification: &Identification) {
    println!(
        "AS7343 ready, part number: {:#04X}, revision: {:#04X}, auxiliary id: {:#04X}",
        identification.id, identification.revid, identification.auxid
    );
    println!(
        "AS7343 configuration: gain: {:?}, ATIME: {}, ASTEP: {}, {} us per cycle, {} us per measurement",
        MEASUREMENT_CONFIGURATION.gain,
        MEASUREMENT_CONFIGURATION.atime,
        MEASUREMENT_CONFIGURATION.astep,
        MEASUREMENT_CONFIGURATION.integration_time_us(),
        MEASUREMENT_CONFIGURATION.measurement_time_us()
    );
}

/// Run one 18-channel measurement.
///
/// The shared bus is released while the sensor converts, so the other sensor
/// tasks can use it during those ~150 ms; only the start and the read-out hold
/// it. Waiting the conversion time first also means `AVALID` is already set
/// when the data are fetched, so the driver's poll loop runs exactly once.
async fn read_once(bus: &SharedI2cBus) -> Result<Measurement, Error> {
    {
        let mut bus = bus.lock().await;
        let mut i2c = bus.acquire();
        let mut sensor = As7343::new(&mut i2c);
        sensor.start_measurement().await?;
    }

    Timer::after_micros(MEASUREMENT_TIME_US).await;

    let mut bus = bus.lock().await;
    let mut i2c = bus.acquire();
    let mut sensor = As7343::new(&mut i2c);
    sensor.read_measurement().await
}

/// Print every value of one measurement.
///
/// The twelve filtered channels are listed by centre wavelength, followed by
/// the three unfiltered visible readings and the three flicker-detect readings,
/// which are what the three integration cycles produce alongside them.
fn print_measurement(measurement: &Measurement, settling: bool) {
    print!("AS7343 spectrum:");
    for channel in SPECTRAL_CHANNELS {
        // Every channel in `SPECTRAL_CHANNELS` is a filtered one, so it always
        // has a centre wavelength.
        if let Some(wavelength_nm) = channel.wavelength_nm() {
            print!(" {} nm: {},", wavelength_nm, measurement.channel(channel));
        }
    }
    println!(
        " visible: {}/{}/{}, flicker: {}/{}/{}{}",
        measurement.channel(Channel::Visible1),
        measurement.channel(Channel::Visible2),
        measurement.channel(Channel::Visible3),
        measurement.channel(Channel::FlickerDetect1),
        measurement.channel(Channel::FlickerDetect2),
        measurement.channel(Channel::FlickerDetect3),
        if settling {
            " (warm-up, discarded)"
        } else {
            ""
        }
    );

    if measurement.saturated() {
        println!(
            "AS7343: saturated (analog: {}, digital: {}), lower the gain or the integration time",
            measurement.analog_saturation, measurement.digital_saturation
        );
    }
}

/// Periodically read the AS7343, print every channel of the result and retain
/// it.
///
/// Measurement starts follow a fixed [`MEASUREMENT_INTERVAL_MS`] schedule.
#[embassy_executor::task]
pub async fn measure_task(bus: &'static SharedI2cBus) {
    // Set on every bus error so the next cycle re-runs the sensor's init
    // sequence instead of assuming its configuration survived.
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
                    shared_state::publish_as7343(measurement).await;
                }

                print_measurement(&measurement, settling);

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
                println!("AS7343: measurement failed: {:?}, recovering bus", error);
                Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
            }
        }
    }
}
