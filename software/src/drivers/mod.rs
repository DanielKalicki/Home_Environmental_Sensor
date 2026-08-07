//! Sensor drivers and the shared I2C bus they sit on.
//!
//! Each sensor driver only speaks to its device over I2C; the periodic
//! read-out logic lives in the matching module under [`crate::tasks`].

pub mod i2c_bus;
pub mod scd41;
pub mod sps30;
