//! Minimal driver for the Sensirion SHT41 temperature / humidity sensor.
//!
//! The SHT4x family has no register map: every transaction is a single command
//! byte, followed by the datasheet execution time, followed by a read of the
//! result. A measurement returns two words, each followed by a CRC-8:
//!
//! 1. Send one of the `measure_*` commands (0xFD / 0xF6 / 0xE0).
//! 2. Wait for the precision-dependent conversion time.
//! 3. Read 6 bytes: temperature word + CRC, humidity word + CRC.
//!
//! Every word on the wire is big-endian and protected by a CRC-8 with
//! polynomial 0x31, initial value 0xFF, no reflection and no final XOR, i.e.
//! the same CRC the SCD41 uses.

#![allow(dead_code)]

use embassy_time::Timer;
use esp_hal::{
    i2c::{Error as I2cError, Instance, I2C},
    Blocking,
};

/// Factory-default 7-bit address of the SHT41.
pub const DEFAULT_ADDRESS: u8 = 0x44;
/// Alternative 7-bit address of the SHT41-B variant.
pub const ALTERNATIVE_ADDRESS: u8 = 0x45;

const CMD_MEASURE_HIGH_PRECISION: u8 = 0xFD;
const CMD_MEASURE_MEDIUM_PRECISION: u8 = 0xF6;
const CMD_MEASURE_LOW_PRECISION: u8 = 0xE0;
const CMD_READ_SERIAL_NUMBER: u8 = 0x89;
const CMD_SOFT_RESET: u8 = 0x94;
const CMD_HEATER_200MW_1S: u8 = 0x39;
const CMD_HEATER_200MW_100MS: u8 = 0x32;
const CMD_HEATER_110MW_1S: u8 = 0x2F;
const CMD_HEATER_110MW_100MS: u8 = 0x24;
const CMD_HEATER_20MW_1S: u8 = 0x1E;
const CMD_HEATER_20MW_100MS: u8 = 0x15;

/// Maximum conversion time of a high-precision measurement, per the datasheet
/// (8.3 ms), rounded up.
pub const HIGH_PRECISION_MEASUREMENT_DELAY_MS: u64 = 10;
/// Maximum conversion time of a medium-precision measurement (4.5 ms).
pub const MEDIUM_PRECISION_MEASUREMENT_DELAY_MS: u64 = 5;
/// Maximum conversion time of a low-precision measurement (1.6 ms).
pub const LOW_PRECISION_MEASUREMENT_DELAY_MS: u64 = 2;
/// Execution time of `soft_reset`, per the datasheet (1 ms).
const SOFT_RESET_DELAY_MS: u64 = 2;
/// Execution time of `read_serial_number`, per the datasheet.
const SHORT_COMMAND_DELAY_MS: u64 = 1;
/// Heater on-time of the long heater pulses, plus the trailing measurement.
const HEATER_LONG_PULSE_DELAY_MS: u64 = 1100;
/// Heater on-time of the short heater pulses, plus the trailing measurement.
const HEATER_SHORT_PULSE_DELAY_MS: u64 = 110;

const CRC8_POLYNOMIAL: u8 = 0x31;
const CRC8_INIT: u8 = 0xFF;

/// Errors reported by the driver.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Error {
    /// The I2C transfer itself failed (no acknowledge, timeout, ...).
    Bus(I2cError),
    /// A word came back with a CRC that does not match its data bytes.
    Crc,
}

impl From<I2cError> for Error {
    fn from(error: I2cError) -> Self {
        Error::Bus(error)
    }
}

/// Repeatability of a measurement, trading noise against time and energy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precision {
    /// 0xFD, lowest noise, longest conversion.
    High,
    /// 0xF6.
    Medium,
    /// 0xE0, highest noise, shortest conversion.
    Low,
}

impl Precision {
    fn command(self) -> u8 {
        match self {
            Precision::High => CMD_MEASURE_HIGH_PRECISION,
            Precision::Medium => CMD_MEASURE_MEDIUM_PRECISION,
            Precision::Low => CMD_MEASURE_LOW_PRECISION,
        }
    }

    fn conversion_delay_ms(self) -> u64 {
        match self {
            Precision::High => HIGH_PRECISION_MEASUREMENT_DELAY_MS,
            Precision::Medium => MEDIUM_PRECISION_MEASUREMENT_DELAY_MS,
            Precision::Low => LOW_PRECISION_MEASUREMENT_DELAY_MS,
        }
    }
}

/// Heater power and on-time of one heater pulse.
///
/// The heater is only meant for short pulses; the datasheet limits the duty
/// cycle to 5 %. Every heater command ends with a high-precision measurement
/// taken while the heater is still on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaterPulse {
    /// 200 mW for 1 s.
    Power200mW1s,
    /// 200 mW for 0.1 s.
    Power200mW100ms,
    /// 110 mW for 1 s.
    Power110mW1s,
    /// 110 mW for 0.1 s.
    Power110mW100ms,
    /// 20 mW for 1 s.
    Power20mW1s,
    /// 20 mW for 0.1 s.
    Power20mW100ms,
}

impl HeaterPulse {
    fn command(self) -> u8 {
        match self {
            HeaterPulse::Power200mW1s => CMD_HEATER_200MW_1S,
            HeaterPulse::Power200mW100ms => CMD_HEATER_200MW_100MS,
            HeaterPulse::Power110mW1s => CMD_HEATER_110MW_1S,
            HeaterPulse::Power110mW100ms => CMD_HEATER_110MW_100MS,
            HeaterPulse::Power20mW1s => CMD_HEATER_20MW_1S,
            HeaterPulse::Power20mW100ms => CMD_HEATER_20MW_100MS,
        }
    }

    fn execution_delay_ms(self) -> u64 {
        match self {
            HeaterPulse::Power200mW1s | HeaterPulse::Power110mW1s | HeaterPulse::Power20mW1s => {
                HEATER_LONG_PULSE_DELAY_MS
            }
            HeaterPulse::Power200mW100ms
            | HeaterPulse::Power110mW100ms
            | HeaterPulse::Power20mW100ms => HEATER_SHORT_PULSE_DELAY_MS,
        }
    }
}

/// One complete temperature / humidity result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measurement {
    /// Raw temperature word; convert with [`Measurement::temperature_celsius`].
    pub temperature_raw: u16,
    /// Raw humidity word; convert with [`Measurement::humidity_percent`].
    pub humidity_raw: u16,
}

impl Measurement {
    /// Temperature in degrees Celsius: `-45 + 175 * raw / 2^16`.
    pub fn temperature_celsius(&self) -> f32 {
        -45.0 + 175.0 * (self.temperature_raw as f32) / 65535.0
    }

    /// Relative humidity in percent: `-6 + 125 * raw / 2^16`.
    ///
    /// The transfer function can return values slightly outside 0..100 %RH for
    /// very dry or fully saturated air, so the result is clamped.
    pub fn humidity_percent(&self) -> f32 {
        let humidity = -6.0 + 125.0 * (self.humidity_raw as f32) / 65535.0;
        humidity.clamp(0.0, 100.0)
    }

    /// Relative humidity in percent without the 0..100 clamp.
    pub fn humidity_percent_unclamped(&self) -> f32 {
        -6.0 + 125.0 * (self.humidity_raw as f32) / 65535.0
    }
}

/// Compute the Sensirion CRC-8 over a data word.
fn crc8(data: &[u8]) -> u8 {
    let mut crc = CRC8_INIT;

    for byte in data {
        crc ^= byte;
        for _ in 0..8 {
            if crc & 0x80 != 0 {
                crc = (crc << 1) ^ CRC8_POLYNOMIAL;
            } else {
                crc <<= 1;
            }
        }
    }

    crc
}

/// SHT41 attached to an esp-hal I2C bus.
///
/// The I2C transfers themselves are blocking, but every datasheet-mandated
/// execution delay yields to the Embassy executor instead of busy-waiting.
pub struct Sht41<'a, 'd, T> {
    i2c: &'a mut I2C<'d, T, Blocking>,
    address: u8,
}

impl<'a, 'd, T> Sht41<'a, 'd, T>
where
    T: Instance,
{
    /// Bind the driver to a bus, using the factory-default address.
    pub fn new(i2c: &'a mut I2C<'d, T, Blocking>) -> Self {
        Self::with_address(i2c, DEFAULT_ADDRESS)
    }

    /// Bind the driver to a bus using an explicit 7-bit address.
    pub fn with_address(i2c: &'a mut I2C<'d, T, Blocking>, address: u8) -> Self {
        Self { i2c, address }
    }

    /// Send a bare command byte and wait for its execution time.
    async fn send_command(&mut self, command: u8, execution_delay_ms: u64) -> Result<(), Error> {
        self.i2c.write(self.address, &[command])?;
        Timer::after_millis(execution_delay_ms).await;
        Ok(())
    }

    /// Send a command and read two CRC-checked words back.
    ///
    /// The SHT4x needs the conversion time to pass between the command and the
    /// read, so this is deliberately not a repeated-start `write_read`.
    async fn read_two_words(
        &mut self,
        command: u8,
        execution_delay_ms: u64,
    ) -> Result<[u16; 2], Error> {
        self.send_command(command, execution_delay_ms).await?;

        let mut buffer = [0u8; 6];
        self.i2c.read(self.address, &mut buffer)?;

        let mut words = [0u16; 2];
        for (word, chunk) in words.iter_mut().zip(buffer.chunks_exact(3)) {
            if crc8(&chunk[..2]) != chunk[2] {
                return Err(Error::Crc);
            }
            *word = u16::from_be_bytes([chunk[0], chunk[1]]);
        }

        Ok(words)
    }

    /// Restart the sensor and return it to its power-on state.
    pub async fn soft_reset(&mut self) -> Result<(), Error> {
        self.send_command(CMD_SOFT_RESET, SOFT_RESET_DELAY_MS).await
    }

    /// Read the 32-bit unique serial number; useful as a presence check.
    pub async fn serial_number(&mut self) -> Result<u32, Error> {
        let words = self
            .read_two_words(CMD_READ_SERIAL_NUMBER, SHORT_COMMAND_DELAY_MS)
            .await?;
        Ok(((words[0] as u32) << 16) | (words[1] as u32))
    }

    /// Run one measurement at the given repeatability.
    pub async fn measure(&mut self, precision: Precision) -> Result<Measurement, Error> {
        let words = self
            .read_two_words(precision.command(), precision.conversion_delay_ms())
            .await?;

        Ok(Measurement {
            temperature_raw: words[0],
            humidity_raw: words[1],
        })
    }

    /// Run one high-precision measurement.
    pub async fn measure_high_precision(&mut self) -> Result<Measurement, Error> {
        self.measure(Precision::High).await
    }

    /// Fire one heater pulse and return the measurement taken at its end.
    ///
    /// The reading is taken while the heater is still on, so it is only useful
    /// for removing condensation or for a plausibility check, not as an ambient
    /// value. Keep the heater duty cycle below 5 %.
    pub async fn heater_pulse(&mut self, pulse: HeaterPulse) -> Result<Measurement, Error> {
        let words = self
            .read_two_words(pulse.command(), pulse.execution_delay_ms())
            .await?;

        Ok(Measurement {
            temperature_raw: words[0],
            humidity_raw: words[1],
        })
    }
}
