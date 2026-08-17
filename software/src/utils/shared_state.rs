//! Sensor history shared between the sensor tasks and the web server.
//!
//! The sensor tasks are the only writers; the web server task only reads. The
//! data is protected by a critical-section mutex so it can be published from
//! any executor without assuming a particular task priority.
//!
//! Each stored reading carries the `Instant` at which it was taken. The board
//! has no real-time clock, so readings are dated by the device's uptime and a
//! reader turns those uptimes into wall-clock times of its own.
//!
//! A full day of readings is far too much for the internal RAM the Wi-Fi
//! driver and the network stack also need, so all five ring buffers are
//! placed in the external PSRAM by [`init`]. Until that call succeeds the
//! histories are absent: readings are then dropped rather than stored, and the
//! web server reports an empty history.
//!
//! The thermal camera is the exception to all of that. One of its images is
//! 768 temperatures, so a retained day of them would dwarf every other
//! history put together; only the newest image is kept, in a buffer of its own
//! that each new image overwrites.

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::Instant;

use crate::drivers::as7343::Measurement as As7343Measurement;
use crate::drivers::bmp581::Measurement as Bmp581Measurement;
use crate::drivers::bsec::Outputs as Bme690Measurement;
use crate::drivers::mlx90640::{COLUMNS, PIXEL_COUNT, ROWS};
use crate::drivers::scd41::Measurement as Scd41Measurement;
use crate::drivers::sps30::Measurement as Sps30Measurement;
use crate::tasks::as7343_task::MEASUREMENT_INTERVAL_MS as AS7343_INTERVAL_MS;
use crate::tasks::bme690_task::MEASUREMENT_INTERVAL_MS as BME690_INTERVAL_MS;
use crate::tasks::bmp581_task::MEASUREMENT_INTERVAL_MS as BMP581_INTERVAL_MS;
use crate::tasks::scd41_task::MEASUREMENT_INTERVAL_MS as SCD41_INTERVAL_MS;
use crate::tasks::sps30_task::MEASUREMENT_INTERVAL_MS as SPS30_INTERVAL_MS;
use crate::utils::history::MeasurementHistory;
use crate::utils::psram::Psram;

/// Time span the retained readings cover: one full day.
pub const HISTORY_WINDOW_MS: u64 = 24 * 60 * 60 * 1000;

/// Number of SCD41 readings retained, enough to fill [`HISTORY_WINDOW_MS`].
pub const SCD41_CAPACITY: usize = (HISTORY_WINDOW_MS / SCD41_INTERVAL_MS) as usize;
/// Number of SPS30 readings retained, enough to fill [`HISTORY_WINDOW_MS`].
pub const SPS30_CAPACITY: usize = (HISTORY_WINDOW_MS / SPS30_INTERVAL_MS) as usize;
/// Number of BME690 readings retained, enough to fill [`HISTORY_WINDOW_MS`].
pub const BME690_CAPACITY: usize = (HISTORY_WINDOW_MS / BME690_INTERVAL_MS) as usize;
/// Number of AS7343 readings retained, enough to fill [`HISTORY_WINDOW_MS`].
pub const AS7343_CAPACITY: usize = (HISTORY_WINDOW_MS / AS7343_INTERVAL_MS) as usize;
/// Number of BMP581 readings retained, enough to fill [`HISTORY_WINDOW_MS`].
pub const BMP581_CAPACITY: usize = (HISTORY_WINDOW_MS / BMP581_INTERVAL_MS) as usize;

/// One reading together with the time it was taken.
#[derive(Clone, Copy)]
pub struct Sample<T: Copy> {
    /// The measured values.
    pub value: T,
    /// Uptime at which the reading completed.
    pub taken_at: Instant,
}

/// What a reader needs to know about one sensor's retained history.
///
/// Sequence numbers identify individual readings and never repeat, so a client
/// that remembers the last one it received can ask for exactly the readings
/// taken since then. See [`MeasurementHistory`] for how they are assigned.
#[derive(Clone, Copy)]
pub struct HistoryStatus {
    /// Scheduled time between two readings of this sensor.
    pub interval_ms: u64,
    /// Readings the ring buffer can hold; zero if [`init`] has not run.
    pub capacity: usize,
    /// Readings currently retained.
    pub len: usize,
    /// Sequence number of the oldest retained reading.
    pub first_sequence: u64,
    /// Sequence number the next reading will be given.
    pub next_sequence: u64,
}

impl HistoryStatus {
    /// The status of a sensor whose history has not been reserved.
    const fn absent(interval_ms: u64) -> Self {
        Self {
            interval_ms,
            capacity: 0,
            len: 0,
            first_sequence: 0,
            next_sequence: 0,
        }
    }
}

/// Retained readings of all five sensors.
///
/// Every field is `None` until [`init`] has placed the ring buffers in PSRAM.
struct SensorState {
    scd41: Option<MeasurementHistory<Sample<Scd41Measurement>>>,
    sps30: Option<MeasurementHistory<Sample<Sps30Measurement>>>,
    bme690: Option<MeasurementHistory<Sample<Bme690Measurement>>>,
    as7343: Option<MeasurementHistory<Sample<As7343Measurement>>>,
    bmp581: Option<MeasurementHistory<Sample<Bmp581Measurement>>>,
}

/// Process-wide storage for the retained readings.
static STATE: Mutex<CriticalSectionRawMutex, SensorState> = Mutex::new(SensorState {
    scd41: None,
    sps30: None,
    bme690: None,
    as7343: None,
    bmp581: None,
});

/// Reserve every ring buffer in PSRAM.
///
/// Call this once, before the sensor tasks are spawned. Returns the number of
/// PSRAM bytes taken, or `None` if the remaining PSRAM cannot hold a full day
/// of readings.
pub async fn init(psram: &mut Psram) -> Option<usize> {
    let before = psram.free_bytes();

    let scd41 = psram.alloc_slice::<Option<Sample<Scd41Measurement>>>(SCD41_CAPACITY, None)?;
    let sps30 = psram.alloc_slice::<Option<Sample<Sps30Measurement>>>(SPS30_CAPACITY, None)?;
    let bme690 = psram.alloc_slice::<Option<Sample<Bme690Measurement>>>(BME690_CAPACITY, None)?;
    let as7343 = psram.alloc_slice::<Option<Sample<As7343Measurement>>>(AS7343_CAPACITY, None)?;
    let bmp581 = psram.alloc_slice::<Option<Sample<Bmp581Measurement>>>(BMP581_CAPACITY, None)?;

    let mut state = STATE.lock().await;
    state.scd41 = Some(MeasurementHistory::new(scd41));
    state.sps30 = Some(MeasurementHistory::new(sps30));
    state.bme690 = Some(MeasurementHistory::new(bme690));
    state.as7343 = Some(MeasurementHistory::new(as7343));
    state.bmp581 = Some(MeasurementHistory::new(bmp581));

    Some(before - psram.free_bytes())
}

/// Append an SCD41 reading, discarding the oldest one when full.
pub async fn publish_scd41(measurement: Scd41Measurement) {
    if let Some(history) = STATE.lock().await.scd41.as_mut() {
        history.push(Sample {
            value: measurement,
            taken_at: Instant::now(),
        });
    }
}

/// Append an SPS30 reading, discarding the oldest one when full.
pub async fn publish_sps30(measurement: Sps30Measurement) {
    if let Some(history) = STATE.lock().await.sps30.as_mut() {
        history.push(Sample {
            value: measurement,
            taken_at: Instant::now(),
        });
    }
}

/// Append a BME690 reading, discarding the oldest one when full.
pub async fn publish_bme690(measurement: Bme690Measurement) {
    if let Some(history) = STATE.lock().await.bme690.as_mut() {
        history.push(Sample {
            value: measurement,
            taken_at: Instant::now(),
        });
    }
}

/// Append an AS7343 reading, discarding the oldest one when full.
pub async fn publish_as7343(measurement: As7343Measurement) {
    if let Some(history) = STATE.lock().await.as7343.as_mut() {
        history.push(Sample {
            value: measurement,
            taken_at: Instant::now(),
        });
    }
}

/// Append a BMP581 reading, discarding the oldest one when full.
pub async fn publish_bmp581(measurement: Bmp581Measurement) {
    if let Some(history) = STATE.lock().await.bmp581.as_mut() {
        history.push(Sample {
            value: measurement,
            taken_at: Instant::now(),
        });
    }
}

/// State of the retained SCD41 history.
pub async fn scd41_status() -> HistoryStatus {
    match STATE.lock().await.scd41.as_ref() {
        Some(history) => HistoryStatus {
            interval_ms: SCD41_INTERVAL_MS,
            capacity: history.capacity(),
            len: history.len(),
            first_sequence: history.first_sequence(),
            next_sequence: history.next_sequence(),
        },
        None => HistoryStatus::absent(SCD41_INTERVAL_MS),
    }
}

/// State of the retained SPS30 history.
pub async fn sps30_status() -> HistoryStatus {
    match STATE.lock().await.sps30.as_ref() {
        Some(history) => HistoryStatus {
            interval_ms: SPS30_INTERVAL_MS,
            capacity: history.capacity(),
            len: history.len(),
            first_sequence: history.first_sequence(),
            next_sequence: history.next_sequence(),
        },
        None => HistoryStatus::absent(SPS30_INTERVAL_MS),
    }
}

/// State of the retained BME690 history.
pub async fn bme690_status() -> HistoryStatus {
    match STATE.lock().await.bme690.as_ref() {
        Some(history) => HistoryStatus {
            interval_ms: BME690_INTERVAL_MS,
            capacity: history.capacity(),
            len: history.len(),
            first_sequence: history.first_sequence(),
            next_sequence: history.next_sequence(),
        },
        None => HistoryStatus::absent(BME690_INTERVAL_MS),
    }
}

/// State of the retained AS7343 history.
pub async fn as7343_status() -> HistoryStatus {
    match STATE.lock().await.as7343.as_ref() {
        Some(history) => HistoryStatus {
            interval_ms: AS7343_INTERVAL_MS,
            capacity: history.capacity(),
            len: history.len(),
            first_sequence: history.first_sequence(),
            next_sequence: history.next_sequence(),
        },
        None => HistoryStatus::absent(AS7343_INTERVAL_MS),
    }
}

/// State of the retained BMP581 history.
pub async fn bmp581_status() -> HistoryStatus {
    match STATE.lock().await.bmp581.as_ref() {
        Some(history) => HistoryStatus {
            interval_ms: BMP581_INTERVAL_MS,
            capacity: history.capacity(),
            len: history.len(),
            first_sequence: history.first_sequence(),
            next_sequence: history.next_sequence(),
        },
        None => HistoryStatus::absent(BMP581_INTERVAL_MS),
    }
}

/// Return the SCD41 reading with the given sequence number, if still retained.
///
/// One reading is copied per call, so the mutex is never held across the
/// network writes that consume the history.
pub async fn scd41_reading(sequence: u64) -> Option<Sample<Scd41Measurement>> {
    let state = STATE.lock().await;
    state.scd41.as_ref()?.get(sequence)
}

/// Return the SPS30 reading with the given sequence number, if still retained.
pub async fn sps30_reading(sequence: u64) -> Option<Sample<Sps30Measurement>> {
    let state = STATE.lock().await;
    state.sps30.as_ref()?.get(sequence)
}

/// Return the BME690 reading with the given sequence number, if still retained.
pub async fn bme690_reading(sequence: u64) -> Option<Sample<Bme690Measurement>> {
    let state = STATE.lock().await;
    state.bme690.as_ref()?.get(sequence)
}

/// Return the AS7343 reading with the given sequence number, if still retained.
pub async fn as7343_reading(sequence: u64) -> Option<Sample<As7343Measurement>> {
    let state = STATE.lock().await;
    state.as7343.as_ref()?.get(sequence)
}

/// Return the BMP581 reading with the given sequence number, if still retained.
pub async fn bmp581_reading(sequence: u64) -> Option<Sample<Bmp581Measurement>> {
    let state = STATE.lock().await;
    state.bmp581.as_ref()?.get(sequence)
}

/// Columns in a thermal image.
pub const THERMAL_COLUMNS: usize = COLUMNS;
/// Rows in a thermal image.
pub const THERMAL_ROWS: usize = ROWS;
/// Pixels in a thermal image.
pub const THERMAL_PIXEL_COUNT: usize = PIXEL_COUNT;

/// What one thermal image amounts to, without the image itself.
///
/// The camera task works these out while it still holds the temperatures, so
/// that a reader can describe an image without copying all 768 of them.
#[derive(Clone, Copy)]
pub struct ThermalSummary {
    /// Coldest pixel, in degrees Celsius.
    pub min_celsius: f32,
    /// Warmest pixel, in degrees Celsius.
    pub max_celsius: f32,
    /// Mean of every pixel, in degrees Celsius.
    pub mean_celsius: f32,
    /// Temperature of the camera's own die, which the pixels are compensated
    /// against. It runs warmer than the room.
    pub ambient_celsius: f32,
}

/// The newest thermal image and when it was taken.
pub struct ThermalStatus {
    /// Uptime at which the image was completed.
    pub taken_at: Instant,
    /// How many images the camera has taken; the first one is number 1.
    pub sequence: u64,
    /// What the image amounts to.
    pub summary: ThermalSummary,
}

/// Storage for the newest thermal image.
///
/// The image is held in place rather than as an `Option` that is replaced
/// wholesale: it is 3 kB, and swapping it in and out would build every new
/// image on the caller's stack first. `taken` distinguishes the buffer's
/// initial contents from a real image.
struct ThermalState {
    taken: bool,
    taken_at: Instant,
    sequence: u64,
    summary: ThermalSummary,
    pixels: [f32; THERMAL_PIXEL_COUNT],
}

static THERMAL: Mutex<CriticalSectionRawMutex, ThermalState> = Mutex::new(ThermalState {
    taken: false,
    taken_at: Instant::from_ticks(0),
    sequence: 0,
    summary: ThermalSummary {
        min_celsius: 0.0,
        max_celsius: 0.0,
        mean_celsius: 0.0,
        ambient_celsius: 0.0,
    },
    pixels: [0.0; THERMAL_PIXEL_COUNT],
});

/// Replace the newest thermal image.
///
/// The pixels are copied into the existing buffer, so nothing the size of an
/// image is ever built on the caller's stack.
pub async fn publish_thermal(pixels: &[f32; THERMAL_PIXEL_COUNT], summary: ThermalSummary) {
    let mut state = THERMAL.lock().await;
    state.pixels.copy_from_slice(pixels);
    state.summary = summary;
    state.taken_at = Instant::now();
    state.sequence += 1;
    state.taken = true;
}

/// Describe the newest thermal image, or `None` if none has been taken yet.
pub async fn thermal_status() -> Option<ThermalStatus> {
    let state = THERMAL.lock().await;
    state.taken.then(|| ThermalStatus {
        taken_at: state.taken_at,
        sequence: state.sequence,
        summary: state.summary,
    })
}

/// Copy one row of the newest thermal image into `out`.
///
/// Rows are numbered from the top, and a whole image is [`THERMAL_ROWS`] of
/// them. Reading an image a row at a time is what keeps the web server from
/// holding this mutex, or a copy of the whole image, while it writes the
/// image out to a socket. Returns `false` if no image has been taken yet or
/// `row` is past the bottom of the image, leaving `out` untouched.
pub async fn thermal_row(row: usize, out: &mut [f32; THERMAL_COLUMNS]) -> bool {
    if row >= THERMAL_ROWS {
        return false;
    }

    let state = THERMAL.lock().await;
    if !state.taken {
        return false;
    }

    let start = row * THERMAL_COLUMNS;
    out.copy_from_slice(&state.pixels[start..start + THERMAL_COLUMNS]);
    true
}
