//! Latest sensor readings, shared between the sensor tasks and the web server.
//!
//! The sensor tasks are the only writers; the web server task only reads. The
//! data is protected by a critical-section mutex so it can be published from
//! any executor without assuming a particular task priority.

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;

use crate::drivers::scd41::Measurement as Scd41Measurement;
use crate::drivers::sps30::Measurement as Sps30Measurement;

/// The most recent successful reading of each sensor.
///
/// A field is `None` until that sensor has produced its first reading.
#[derive(Clone, Copy, Default)]
pub struct LatestReadings {
    /// Newest SCD41 CO2 / temperature / humidity reading.
    pub scd41: Option<Scd41Measurement>,
    /// Newest SPS30 particulate matter reading.
    pub sps30: Option<Sps30Measurement>,
}

/// Process-wide storage for the newest readings.
pub static LATEST: Mutex<CriticalSectionRawMutex, LatestReadings> =
    Mutex::new(LatestReadings {
        scd41: None,
        sps30: None,
    });

/// Publish a new SCD41 reading, replacing the previous one.
pub async fn publish_scd41(measurement: Scd41Measurement) {
    LATEST.lock().await.scd41 = Some(measurement);
}

/// Publish a new SPS30 reading, replacing the previous one.
pub async fn publish_sps30(measurement: Sps30Measurement) {
    LATEST.lock().await.sps30 = Some(measurement);
}

/// Return a copy of the newest readings.
pub async fn snapshot() -> LatestReadings {
    *LATEST.lock().await
}
