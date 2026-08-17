//! Periodic MLX90640 thermal image capture.
//!
//! The camera measures on its own and refreshes only half its pixels at a
//! time, so one image is assembled from two read-outs, one per subpage. The
//! task discards whatever the camera happens to be holding, waits for both
//! halves to arrive fresh, converts them to degrees Celsius and publishes the
//! result as *the* image: only the newest one is kept.
//!
//! The calibration constants, the raw frame and the finished image are far too
//! large to live in the task's own future, so they are `static` buffers. This
//! task is spawned exactly once, which is what makes that safe.

use core::ptr::addr_of_mut;

use embassy_time::{Duration, Instant, Ticker, Timer};
use esp_println::println;

use crate::drivers::i2c_bus::SharedI2cBus;
use crate::drivers::mlx90640::{
    Configuration, Eeprom, Error, Frame, Mlx90640, Parameters, Pattern, RefreshRate, Resolution,
    COLUMNS, EEPROM_WORDS, PIXEL_COUNT, ROWS,
};
use crate::utils::shared_state::{self, ThermalSummary};

/// Settings every image is taken with.
///
/// 2 Hz per subpage is the camera's own default and about as fast as this
/// board can read it: one frame is 1664 bytes, which takes roughly 150 ms on
/// the 100 kHz bus, and the two subpages of an image have to be read within
/// 500 ms of each other. 18-bit resolution and the chess pattern are also the
/// device's defaults, and the chess pattern is the one it is calibrated for,
/// so no pattern correction is needed.
const CAMERA_CONFIGURATION: Configuration = Configuration {
    refresh_rate: RefreshRate::Hz2,
    resolution: Resolution::Bits18,
    pattern: Pattern::Chess,
};

/// Time between the starts of two image captures.
pub const IMAGE_INTERVAL_MS: u64 = 10_000;

/// Emissivity assumed for whatever the camera is pointed at.
///
/// 0.95 is the usual figure for painted walls, wood, fabric and skin. Bare
/// metal is far lower and will read far too cold; that is a property of the
/// surface, not a fault in the camera.
const EMISSIVITY: f32 = 0.95;

/// How far below the camera's own die temperature the surroundings are taken
/// to be, in degrees Celsius.
///
/// The model needs the temperature of whatever radiation the observed surface
/// reflects. Without a second sensor pointed at the room there is nothing to
/// measure it with, so the datasheet's suggestion of eight degrees under the
/// die temperature is used.
const REFLECTED_OFFSET_C: f32 = 8.0;

/// Time between two polls of the camera's data-ready flag.
const POLL_INTERVAL_MS: u64 = 50;

/// Longest wait for one subpage before the capture is abandoned.
///
/// Four times the 500 ms a subpage should take, so an image is only given up
/// on when the camera has really stopped measuring.
const SUBPAGE_TIMEOUT_MS: u64 = 2000;

/// Idle time before retrying after a bus or camera error.
const ERROR_RETRY_DELAY_MS: u64 = 1000;

/// The camera's calibration constants, about 9.5 kB of them.
static mut PARAMETERS: Parameters = Parameters::zeroed();
/// One raw read-out of the camera's measurement RAM.
static mut FRAME: Frame = Frame::new();
/// The newest image, in degrees Celsius, while it is being assembled.
static mut IMAGE: [f32; PIXEL_COUNT] = [0.0; PIXEL_COUNT];
/// Scratch space for the calibration EEPROM, used once during initialisation.
static mut EEPROM: Eeprom = [0; EEPROM_WORDS];

/// Why a capture did not produce an image.
#[derive(Debug, Clone, Copy, PartialEq)]
enum CaptureError {
    /// The bus or the camera reported a fault.
    Camera(Error),
    /// A subpage did not arrive within [`SUBPAGE_TIMEOUT_MS`].
    Timeout,
}

impl From<Error> for CaptureError {
    fn from(error: Error) -> Self {
        CaptureError::Camera(error)
    }
}

/// Load the camera's calibration and apply the configuration.
///
/// The shared bus is held for the whole sequence, which is dominated by the
/// 1664-byte EEPROM read. Returns `false` if any step failed, in which case
/// the caller should retry later.
async fn initialize(bus: &SharedI2cBus, parameters: &mut Parameters, eeprom: &mut Eeprom) -> bool {
    println!("MLX90640: reading calibration EEPROM");

    // Hold the bus only for the I2C work. Unpacking the EEPROM is pure CPU and
    // must not keep the other sensors off the bus.
    let read_result = {
        let mut bus = bus.lock().await;
        let mut i2c = bus.acquire();
        let mut camera = Mlx90640::new(&mut i2c);
        camera.read_eeprom(eeprom).await
    };

    if let Err(error) = read_result {
        println!("MLX90640: initialisation failed: {:?}", error);
        return false;
    }

    println!("MLX90640: unpacking calibration");
    if let Err(error) = parameters.extract(eeprom) {
        println!("MLX90640: initialisation failed: {:?}", error);
        return false;
    }

    let configure_result = {
        let mut bus = bus.lock().await;
        let mut i2c = bus.acquire();
        let mut camera = Mlx90640::new(&mut i2c);
        camera.set_configuration(&CAMERA_CONFIGURATION).await
    };

    match configure_result {
        Ok(()) => {
            println!(
                "MLX90640 ready, {}x{} pixels, {} ms per subpage, chess pattern",
                COLUMNS,
                ROWS,
                CAMERA_CONFIGURATION.subpage_period_us() / 1000
            );
            let broken = parameters.broken_pixels().len();
            let outliers = parameters.outlier_pixels().len();
            if broken > 0 || outliers > 0 {
                println!(
                    "MLX90640: EEPROM flags {} broken and {} out-of-tolerance pixels",
                    broken, outliers
                );
            }
            true
        }
        Err(error) => {
            println!("MLX90640: initialisation failed: {:?}", error);
            false
        }
    }
}

/// Wait until the camera has a subpage ready, then read it.
///
/// The bus is locked only for the poll itself and for the read, so the other
/// sensor tasks keep their turns during the up to 500 ms the camera spends
/// measuring.
async fn read_next_frame(bus: &SharedI2cBus, frame: &mut Frame) -> Result<(), CaptureError> {
    let deadline = Instant::now() + Duration::from_millis(SUBPAGE_TIMEOUT_MS);

    loop {
        {
            let mut bus = bus.lock().await;
            let mut i2c = bus.acquire();
            let mut camera = Mlx90640::new(&mut i2c);

            if camera.data_ready().await? {
                return Ok(camera.read_frame(frame).await?);
            }
        }

        if Instant::now() >= deadline {
            return Err(CaptureError::Timeout);
        }
        Timer::after_millis(POLL_INTERVAL_MS).await;
    }
}

/// Take one complete image into `image`.
///
/// Anything the camera is already holding is thrown away first, so both halves
/// of the image are measured after this call started and within one refresh of
/// each other. The two subpages are then read in whatever order they arrive
/// and each is converted straight into `image`, which is why the pixels of the
/// other half are left alone rather than zeroed.
async fn capture(
    bus: &SharedI2cBus,
    parameters: &Parameters,
    frame: &mut Frame,
    image: &mut [f32; PIXEL_COUNT],
) -> Result<ThermalSummary, CaptureError> {
    {
        let mut bus = bus.lock().await;
        let mut i2c = bus.acquire();
        Mlx90640::new(&mut i2c).clear_data_ready().await?;
    }

    let mut seen = [false; 2];
    let mut ambient = 0.0;

    while !(seen[0] && seen[1]) {
        read_next_frame(bus, frame).await?;

        ambient = frame.ambient_temperature(parameters);
        frame.object_temperatures(parameters, EMISSIVITY, ambient - REFLECTED_OFFSET_C, image);
        seen[frame.subpage() as usize] = true;
    }

    Ok(summarize(image, ambient))
}

/// Reduce a finished image to the few numbers that describe it.
fn summarize(image: &[f32; PIXEL_COUNT], ambient_celsius: f32) -> ThermalSummary {
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let mut total = 0.0;

    for &pixel in image.iter() {
        if pixel < min {
            min = pixel;
        }
        if pixel > max {
            max = pixel;
        }
        total += pixel;
    }

    ThermalSummary {
        min_celsius: min,
        max_celsius: max,
        mean_celsius: total / PIXEL_COUNT as f32,
        ambient_celsius,
    }
}

/// Periodically capture a thermal image and publish it.
///
/// Captures follow a fixed schedule; each one occupies the camera for a little
/// over a second, most of which is spent waiting for the two subpages rather
/// than holding the bus.
#[embassy_executor::task]
pub async fn capture_task(bus: &'static SharedI2cBus) {
    // SAFETY: this task is spawned once, so it is the only user of these
    // buffers, and the references never outlive the task.
    let (parameters, frame, image, eeprom) = unsafe {
        (
            &mut *addr_of_mut!(PARAMETERS),
            &mut *addr_of_mut!(FRAME),
            &mut *addr_of_mut!(IMAGE),
            &mut *addr_of_mut!(EEPROM),
        )
    };

    // Set on every error so the next cycle reloads the calibration instead of
    // assuming the camera survived whatever went wrong.
    let mut needs_init = true;
    let mut ticker = Ticker::every(Duration::from_millis(IMAGE_INTERVAL_MS));

    loop {
        if needs_init {
            if !initialize(bus, parameters, eeprom).await {
                Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
                continue;
            }
        }
        let reinitialized = needs_init;

        match capture(bus, parameters, frame, image).await {
            Ok(summary) => {
                needs_init = false;
                shared_state::publish_thermal(image, summary).await;

                println!(
                    "Thermal image: {} to {} C, mean {} C, sensor die {} C",
                    summary.min_celsius,
                    summary.max_celsius,
                    summary.mean_celsius,
                    summary.ambient_celsius
                );

                // Begin a fresh fixed schedule after recovery; this capture
                // becomes the first deadline of the new schedule.
                if reinitialized {
                    ticker.reset();
                }
                ticker.next().await;
            }
            Err(error) => {
                needs_init = true;
                println!("MLX90640: capture failed: {:?}, reinitialising", error);
                Timer::after_millis(ERROR_RETRY_DELAY_MS).await;
            }
        }
    }
}
