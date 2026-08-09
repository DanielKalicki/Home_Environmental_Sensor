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
//! driver and the network stack also need, so both ring buffers are placed in
//! the external PSRAM by [`init`]. Until that call succeeds the histories are
//! absent: readings are then dropped rather than stored, and the web server
//! reports an empty history.

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::Instant;

use crate::drivers::scd41::Measurement as Scd41Measurement;
use crate::drivers::sps30::Measurement as Sps30Measurement;
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

/// Retained readings of both sensors.
///
/// Both fields are `None` until [`init`] has placed the ring buffers in PSRAM.
struct SensorState {
    scd41: Option<MeasurementHistory<Sample<Scd41Measurement>>>,
    sps30: Option<MeasurementHistory<Sample<Sps30Measurement>>>,
}

/// Process-wide storage for the retained readings.
static STATE: Mutex<CriticalSectionRawMutex, SensorState> = Mutex::new(SensorState {
    scd41: None,
    sps30: None,
});

/// Reserve both ring buffers in PSRAM.
///
/// Call this once, before the sensor tasks are spawned. Returns the number of
/// PSRAM bytes taken, or `None` if the remaining PSRAM cannot hold a full day
/// of readings.
pub async fn init(psram: &mut Psram) -> Option<usize> {
    let before = psram.free_bytes();

    let scd41 = psram.alloc_slice::<Option<Sample<Scd41Measurement>>>(SCD41_CAPACITY, None)?;
    let sps30 = psram.alloc_slice::<Option<Sample<Sps30Measurement>>>(SPS30_CAPACITY, None)?;

    let mut state = STATE.lock().await;
    state.scd41 = Some(MeasurementHistory::new(scd41));
    state.sps30 = Some(MeasurementHistory::new(sps30));

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
