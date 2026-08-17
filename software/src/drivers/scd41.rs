//! Minimal driver for the Sensirion SCD41 CO2 / temperature / humidity sensor.
//!
//! Only the single-shot ("on demand") measurement flow is implemented:
//!
//! 1. `measure_single_shot` (0x219D) triggers one conversion.
//! 2. The sensor needs 5000 ms to finish it.
//! 3. `read_measurement` (0xEC05) returns 3 words, each followed by a CRC-8.
//!
//! Every word on the wire is big-endian and protected by a CRC-8 with
//! polynomial 0x31, initial value 0xFF, no reflection and no final XOR.

#![allow(dead_code)]

use embassy_time::Timer;
use esp_hal::{
    i2c::{Error as I2cError, Instance, I2C},
    Blocking,
};

/// Factory-default 7-bit address of the SCD41.
pub const DEFAULT_ADDRESS: u8 = 0x62;

const CMD_STOP_PERIODIC_MEASUREMENT: u16 = 0x3F86;
const CMD_WAKE_UP: u16 = 0x36F6;
const CMD_REINIT: u16 = 0x3646;
const CMD_MEASURE_SINGLE_SHOT: u16 = 0x219D;
const CMD_MEASURE_SINGLE_SHOT_RHT_ONLY: u16 = 0x2196;
const CMD_READ_MEASUREMENT: u16 = 0xEC05;
const CMD_GET_SERIAL_NUMBER: u16 = 0x3682;
const CMD_GET_SENSOR_VARIANT: u16 = 0x202F;
const CMD_GET_TEMPERATURE_OFFSET: u16 = 0x2318;
const CMD_GET_SENSOR_ALTITUDE: u16 = 0x2322;
const CMD_GET_AMBIENT_PRESSURE: u16 = 0xE000;
const CMD_GET_AUTOMATIC_SELF_CALIBRATION_ENABLED: u16 = 0x2313;
const CMD_GET_AUTOMATIC_SELF_CALIBRATION_TARGET: u16 = 0x233F;
const CMD_GET_AUTOMATIC_SELF_CALIBRATION_INITIAL_PERIOD: u16 = 0x2340;
const CMD_GET_AUTOMATIC_SELF_CALIBRATION_STANDARD_PERIOD: u16 = 0x234B;

/// Execution time of `stop_periodic_measurement`, per the datasheet.
const STOP_PERIODIC_MEASUREMENT_DELAY_MS: u64 = 500;
/// Execution time of `reinit`, per the datasheet.
const REINIT_DELAY_MS: u64 = 30;
/// Execution time of `wake_up`, per the datasheet.
const WAKE_UP_DELAY_MS: u64 = 30;
/// Execution time of a full single-shot measurement, per the datasheet.
pub const SINGLE_SHOT_MEASUREMENT_DELAY_MS: u64 = 5000;
/// Execution time of an RH/T-only single-shot measurement, per the datasheet.
const MEASURE_SINGLE_SHOT_RHT_ONLY_DELAY_MS: u64 = 50;
/// Execution time of short commands such as `read_measurement`.
const SHORT_COMMAND_DELAY_MS: u64 = 1;

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

/// Sensor type reported by `get_sensor_variant` (0x202F).
///
/// The variant is encoded in bits 15..12 of the returned word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorVariant {
    /// Bits 15..12 == 0b0000.
    Scd40,
    /// Bits 15..12 == 0b0001.
    Scd41,
    /// Bits 15..12 == 0b0101.
    Scd43,
    /// Any other encoding, carrying the raw 4-bit field.
    Unknown(u8),
}

impl SensorVariant {
    /// Decode the raw word returned by `get_sensor_variant`.
    fn from_raw(raw: u16) -> Self {
        match (raw >> 12) as u8 & 0x0F {
            0b0000 => SensorVariant::Scd40,
            0b0001 => SensorVariant::Scd41,
            0b0101 => SensorVariant::Scd43,
            other => SensorVariant::Unknown(other),
        }
    }
}

/// One complete single-shot result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measurement {
    /// CO2 concentration in ppm, as reported by the sensor.
    pub co2_ppm: u16,
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

    /// Relative humidity in percent: `100 * raw / 2^16`.
    pub fn humidity_percent(&self) -> f32 {
        100.0 * (self.humidity_raw as f32) / 65535.0
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

/// SCD41 attached to an esp-hal I2C bus.
///
/// The I2C transfers themselves are blocking, but every datasheet-mandated
/// execution delay yields to the Embassy executor instead of busy-waiting.
pub struct Scd41<'a, 'd, T> {
    i2c: &'a mut I2C<'d, T, Blocking>,
    address: u8,
}

impl<'a, 'd, T> Scd41<'a, 'd, T>
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

    /// Send a bare 16-bit command and wait for its execution time.
    async fn send_command(&mut self, command: u16, execution_delay_ms: u64) -> Result<(), Error> {
        self.i2c.write(self.address, &command.to_be_bytes())?;
        Timer::after_millis(execution_delay_ms).await;
        Ok(())
    }

    /// Send a command and read one CRC-checked word back.
    ///
    /// The SCD4x requires the write and the read to be separate transactions,
    /// so this is deliberately not a repeated-start `write_read`.
    async fn read_word(&mut self, command: u16, execution_delay_ms: u64) -> Result<u16, Error> {
        self.send_command(command, execution_delay_ms).await?;

        let mut buffer = [0u8; 3];
        self.i2c.read(self.address, &mut buffer)?;

        if crc8(&buffer[..2]) != buffer[2] {
            return Err(Error::Crc);
        }

        Ok(u16::from_be_bytes([buffer[0], buffer[1]]))
    }

    /// Send a command and read three CRC-checked words back.
    ///
    /// The SCD4x requires the write and the read to be separate transactions,
    /// so this is deliberately not a repeated-start `write_read`.
    async fn read_three_words(
        &mut self,
        command: u16,
        execution_delay_ms: u64,
    ) -> Result<[u16; 3], Error> {
        self.send_command(command, execution_delay_ms).await?;

        let mut buffer = [0u8; 9];
        self.i2c.read(self.address, &mut buffer)?;

        let mut words = [0u16; 3];
        for (word, chunk) in words.iter_mut().zip(buffer.chunks_exact(3)) {
            if crc8(&chunk[..2]) != chunk[2] {
                return Err(Error::Crc);
            }
            *word = u16::from_be_bytes([chunk[0], chunk[1]]);
        }

        Ok(words)
    }

    /// Bring the sensor into idle mode so that single-shot commands are accepted.
    ///
    /// Safe to call after a reset even if no periodic measurement was running.
    pub async fn stop_periodic_measurement(&mut self) -> Result<(), Error> {
        self.send_command(
            CMD_STOP_PERIODIC_MEASUREMENT,
            STOP_PERIODIC_MEASUREMENT_DELAY_MS,
        )
        .await
    }

    /// Reload user settings from EEPROM. The sensor must be idle.
    pub async fn reinit(&mut self) -> Result<(), Error> {
        self.send_command(CMD_REINIT, REINIT_DELAY_MS).await
    }
    /// Leave sleep mode.
    ///
    /// The SCD41 does **not** acknowledge this command, so `Bus(AckCheckFailed)`
    /// is the expected outcome and can be ignored. It is a no-op unless the
    /// sensor was put into power-down mode.
    pub async fn wake_up(&mut self) -> Result<(), Error> {
        let result = self.i2c.write(self.address, &CMD_WAKE_UP.to_be_bytes());
        // Wait for the wake-up time even though the command was not acked.
        Timer::after_millis(WAKE_UP_DELAY_MS).await;
        result.map_err(Error::from)
    }

    /// Read the 48-bit unique serial number; useful as a presence check.
    pub async fn serial_number(&mut self) -> Result<u64, Error> {
        let words = self
            .read_three_words(CMD_GET_SERIAL_NUMBER, SHORT_COMMAND_DELAY_MS)
            .await?;
        Ok(((words[0] as u64) << 32) | ((words[1] as u64) << 16) | (words[2] as u64))
    }

    /// Read which SCD4x variant is attached. The sensor must be idle.
    pub async fn sensor_variant(&mut self) -> Result<SensorVariant, Error> {
        let raw = self
            .read_word(CMD_GET_SENSOR_VARIANT, SHORT_COMMAND_DELAY_MS)
            .await?;
        Ok(SensorVariant::from_raw(raw))
    }

    /// Read the configured temperature offset in degrees Celsius.
    ///
    /// The raw word is converted with `175 * raw / 2^16`. The offset is only
    /// applied to the reported temperature and humidity, not to CO2.
    /// The sensor must be idle.
    pub async fn temperature_offset_celsius(&mut self) -> Result<f32, Error> {
        let raw = self
            .read_word(CMD_GET_TEMPERATURE_OFFSET, SHORT_COMMAND_DELAY_MS)
            .await?;
        Ok(175.0 * (raw as f32) / 65535.0)
    }

    /// Read the configured sensor altitude in metres above sea level.
    ///
    /// The sensor must be idle.
    pub async fn sensor_altitude_meters(&mut self) -> Result<u16, Error> {
        self.read_word(CMD_GET_SENSOR_ALTITUDE, SHORT_COMMAND_DELAY_MS)
            .await
    }

    /// Read the ambient pressure used for CO2 compensation, in pascals.
    ///
    /// The sensor stores the pressure in hectopascals, so the returned value
    /// is the raw word multiplied by 100.
    pub async fn ambient_pressure_pascals(&mut self) -> Result<u32, Error> {
        let raw = self
            .read_word(CMD_GET_AMBIENT_PRESSURE, SHORT_COMMAND_DELAY_MS)
            .await?;
        Ok(raw as u32 * 100)
    }

    /// Read whether automatic self-calibration (ASC) is enabled.
    ///
    /// The sensor returns 1 when ASC is active and 0 when it is off.
    /// The sensor must be idle.
    pub async fn automatic_self_calibration_enabled(&mut self) -> Result<bool, Error> {
        let raw = self
            .read_word(
                CMD_GET_AUTOMATIC_SELF_CALIBRATION_ENABLED,
                SHORT_COMMAND_DELAY_MS,
            )
            .await?;
        Ok(raw != 0)
    }

    /// Read the CO2 concentration in ppm that ASC assumes for fresh air.
    ///
    /// The sensor must be idle.
    pub async fn automatic_self_calibration_target_ppm(&mut self) -> Result<u16, Error> {
        self.read_word(
            CMD_GET_AUTOMATIC_SELF_CALIBRATION_TARGET,
            SHORT_COMMAND_DELAY_MS,
        )
        .await
    }

    /// Read the ASC initial period in hours.
    ///
    /// This is the operating time after a reset before the first automatic
    /// self-calibration is applied; it is always a multiple of 4 hours.
    /// The sensor must be idle.
    pub async fn automatic_self_calibration_initial_period_hours(&mut self) -> Result<u16, Error> {
        self.read_word(
            CMD_GET_AUTOMATIC_SELF_CALIBRATION_INITIAL_PERIOD,
            SHORT_COMMAND_DELAY_MS,
        )
        .await
    }

    /// Read the ASC standard period in hours.
    ///
    /// This is the operating time between two automatic self-calibrations
    /// after the initial period; it is always a multiple of 4 hours.
    /// The sensor must be idle.
    pub async fn automatic_self_calibration_standard_period_hours(&mut self) -> Result<u16, Error> {
        self.read_word(
            CMD_GET_AUTOMATIC_SELF_CALIBRATION_STANDARD_PERIOD,
            SHORT_COMMAND_DELAY_MS,
        )
        .await
    }

    /// Read the result of a conversion that has already completed.
    pub async fn read_measurement(&mut self) -> Result<Measurement, Error> {
        let words = self
            .read_three_words(CMD_READ_MEASUREMENT, SHORT_COMMAND_DELAY_MS)
            .await?;

        Ok(Measurement {
            co2_ppm: words[0],
            temperature_raw: words[1],
            humidity_raw: words[2],
        })
    }

    /// Start an on-demand CO2 + RH/T conversion without waiting for its result.
    ///
    /// Call [`Scd41::read_measurement`] after
    /// [`SINGLE_SHOT_MEASUREMENT_DELAY_MS`] has elapsed. This split operation
    /// lets callers release a shared I2C bus while the sensor converts.
    pub fn start_single_shot(&mut self) -> Result<(), Error> {
        self.i2c
            .write(self.address, &CMD_MEASURE_SINGLE_SHOT.to_be_bytes())
            .map_err(Error::from)
    }

    /// Trigger one on-demand CO2 + RH/T conversion and return its result.
    ///
    /// This yields to the executor for roughly 5 seconds while the sensor converts.
    pub async fn measure_single_shot(&mut self) -> Result<Measurement, Error> {
        self.start_single_shot()?;
        Timer::after_millis(SINGLE_SHOT_MEASUREMENT_DELAY_MS).await;
        self.read_measurement().await
    }

    /// Trigger one on-demand RH/T-only conversion (no CO2 update, ~50 ms).
    ///
    /// The returned `co2_ppm` field is not updated by this command.
    pub async fn measure_single_shot_rht_only(&mut self) -> Result<Measurement, Error> {
        self.send_command(
            CMD_MEASURE_SINGLE_SHOT_RHT_ONLY,
            MEASURE_SINGLE_SHOT_RHT_ONLY_DELAY_MS,
        )
        .await?;
        self.read_measurement().await
    }
}
