//! Minimal driver for the Sensirion SPS30 particulate matter sensor.
//!
//! The sensor is used in its continuous measurement mode:
//!
//! 1. `start_measurement` (0x0010) starts the fan and the measurement loop.
//! 2. `is_data_ready` (0x0202) reports when a new result is available (~1/s).
//! 3. `read_measured_values` (0x0300) returns 10 IEEE-754 big-endian floats.
//!
//! Every word on the wire is big-endian and protected by a CRC-8 with
//! polynomial 0x31, initial value 0xFF, no reflection and no final XOR.

#![allow(dead_code)]

use embassy_time::Timer;
use esp_hal::{
    i2c::{Error as I2cError, Instance, I2C},
    Blocking,
};

/// Fixed 7-bit address of the SPS30.
pub const DEFAULT_ADDRESS: u8 = 0x69;

const CMD_START_MEASUREMENT: u16 = 0x0010;
const CMD_STOP_MEASUREMENT: u16 = 0x0104;
const CMD_READ_DATA_READY_FLAG: u16 = 0x0202;
const CMD_READ_MEASURED_VALUES: u16 = 0x0300;
const CMD_SLEEP: u16 = 0x1001;
const CMD_WAKE_UP: u16 = 0x1103;
const CMD_START_FAN_CLEANING: u16 = 0x5607;
const CMD_READ_SERIAL_NUMBER: u16 = 0xD033;
const CMD_DEVICE_RESET: u16 = 0xD304;

/// Argument selecting big-endian IEEE-754 float output format.
const MEASUREMENT_FORMAT_FLOAT: u16 = 0x0300;

/// Execution time of `start_measurement` / `stop_measurement`.
const MEASUREMENT_CONTROL_DELAY_MS: u64 = 20;
/// Execution time of `device_reset`.
const RESET_DELAY_MS: u64 = 100;
/// Execution time of short read commands.
const SHORT_COMMAND_DELAY_MS: u64 = 5;
/// Length of the serial number string, including the terminating NUL.
pub const SERIAL_NUMBER_LEN: usize = 32;

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

/// One complete set of measured values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    /// Mass concentration PM1.0 in ug/m3.
    pub pm1_0: f32,
    /// Mass concentration PM2.5 in ug/m3.
    pub pm2_5: f32,
    /// Mass concentration PM4.0 in ug/m3.
    pub pm4_0: f32,
    /// Mass concentration PM10 in ug/m3.
    pub pm10: f32,
    /// Number concentration PM0.5 in particles/cm3.
    pub nc0_5: f32,
    /// Number concentration PM1.0 in particles/cm3.
    pub nc1_0: f32,
    /// Number concentration PM2.5 in particles/cm3.
    pub nc2_5: f32,
    /// Number concentration PM4.0 in particles/cm3.
    pub nc4_0: f32,
    /// Number concentration PM10 in particles/cm3.
    pub nc10: f32,
    /// Typical particle size in um.
    pub typical_particle_size: f32,
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

/// Strip the CRC bytes from a raw `word + crc` stream, verifying each one.
///
/// `raw` must be a multiple of 3 bytes; `data` receives 2 bytes per triple.
fn strip_crc(raw: &[u8], data: &mut [u8]) -> Result<(), Error> {
    for (chunk, out) in raw.chunks_exact(3).zip(data.chunks_exact_mut(2)) {
        if crc8(&chunk[..2]) != chunk[2] {
            return Err(Error::Crc);
        }
        out[0] = chunk[0];
        out[1] = chunk[1];
    }

    Ok(())
}

/// SPS30 attached to an esp-hal I2C bus.
///
/// The I2C transfers themselves are blocking, but every datasheet-mandated
/// execution delay yields to the Embassy executor instead of busy-waiting.
pub struct Sps30<'a, 'd, T> {
    i2c: &'a mut I2C<'d, T, Blocking>,
    address: u8,
}

impl<'a, 'd, T> Sps30<'a, 'd, T>
where
    T: Instance,
{
    /// Bind the driver to a bus, using the fixed SPS30 address.
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

    /// Send a 16-bit command followed by one CRC-protected argument word.
    async fn send_command_with_argument(
        &mut self,
        command: u16,
        argument: u16,
        execution_delay_ms: u64,
    ) -> Result<(), Error> {
        let command = command.to_be_bytes();
        let argument = argument.to_be_bytes();
        let frame = [
            command[0],
            command[1],
            argument[0],
            argument[1],
            crc8(&argument),
        ];

        self.i2c.write(self.address, &frame)?;
        Timer::after_millis(execution_delay_ms).await;
        Ok(())
    }

    /// Send a command and read `raw.len() / 3` CRC-checked words back.
    ///
    /// The SPS30 requires the write and the read to be separate transactions,
    /// so this is deliberately not a repeated-start `write_read`.
    async fn read_response(
        &mut self,
        command: u16,
        raw: &mut [u8],
        data: &mut [u8],
    ) -> Result<(), Error> {
        self.send_command(command, SHORT_COMMAND_DELAY_MS).await?;
        self.i2c.read(self.address, raw)?;
        strip_crc(raw, data)
    }

    /// Start continuous measurement in big-endian float output format.
    ///
    /// The fan spins up immediately; the first values are available after
    /// roughly one second and are only stable after ~30 seconds.
    pub async fn start_measurement(&mut self) -> Result<(), Error> {
        self.send_command_with_argument(
            CMD_START_MEASUREMENT,
            MEASUREMENT_FORMAT_FLOAT,
            MEASUREMENT_CONTROL_DELAY_MS,
        )
        .await
    }

    /// Stop continuous measurement and switch back to idle mode.
    pub async fn stop_measurement(&mut self) -> Result<(), Error> {
        self.send_command(CMD_STOP_MEASUREMENT, MEASUREMENT_CONTROL_DELAY_MS)
            .await
    }

    /// Reset the sensor; it comes back in idle mode.
    pub async fn device_reset(&mut self) -> Result<(), Error> {
        self.send_command(CMD_DEVICE_RESET, RESET_DELAY_MS).await
    }

    /// Run the fan-cleaning routine (10 s at maximum speed).
    pub async fn start_fan_cleaning(&mut self) -> Result<(), Error> {
        self.send_command(CMD_START_FAN_CLEANING, MEASUREMENT_CONTROL_DELAY_MS)
            .await
    }

    /// Enter sleep mode. Only valid in idle mode.
    pub async fn sleep(&mut self) -> Result<(), Error> {
        self.send_command(CMD_SLEEP, SHORT_COMMAND_DELAY_MS).await
    }

    /// Leave sleep mode.
    ///
    /// While asleep the sensor does not acknowledge its address, so the first
    /// attempt is expected to fail; the command is sent twice for that reason.
    pub async fn wake_up(&mut self) -> Result<(), Error> {
        let _ = self.i2c.write(self.address, &CMD_WAKE_UP.to_be_bytes());
        Timer::after_millis(SHORT_COMMAND_DELAY_MS).await;
        self.send_command(CMD_WAKE_UP, SHORT_COMMAND_DELAY_MS).await
    }

    /// Report whether a new measurement result is available.
    pub async fn is_data_ready(&mut self) -> Result<bool, Error> {
        let mut raw = [0u8; 3];
        let mut data = [0u8; 2];
        self.read_response(CMD_READ_DATA_READY_FLAG, &mut raw, &mut data)
            .await?;

        Ok(data[1] == 0x01)
    }

    /// Read the ASCII serial number; useful as a presence check.
    ///
    /// Returns the NUL-terminated buffer and the number of characters in it.
    pub async fn serial_number(&mut self) -> Result<([u8; SERIAL_NUMBER_LEN], usize), Error> {
        let mut raw = [0u8; 3 * SERIAL_NUMBER_LEN / 2];
        let mut data = [0u8; SERIAL_NUMBER_LEN];
        self.read_response(CMD_READ_SERIAL_NUMBER, &mut raw, &mut data)
            .await?;

        let len = data
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(data.len());
        Ok((data, len))
    }

    /// Read the latest set of measured values.
    ///
    /// Only meaningful while a measurement is running; in idle mode the sensor
    /// returns the last result or zeros.
    pub async fn read_measured_values(&mut self) -> Result<Measurement, Error> {
        let mut raw = [0u8; 60];
        let mut data = [0u8; 40];
        self.read_response(CMD_READ_MEASURED_VALUES, &mut raw, &mut data)
            .await?;

        let mut values = [0f32; 10];
        for (value, chunk) in values.iter_mut().zip(data.chunks_exact(4)) {
            *value = f32::from_bits(u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }

        Ok(Measurement {
            pm1_0: values[0],
            pm2_5: values[1],
            pm4_0: values[2],
            pm10: values[3],
            nc0_5: values[4],
            nc1_0: values[5],
            nc2_5: values[6],
            nc4_0: values[7],
            nc10: values[8],
            typical_particle_size: values[9],
        })
    }
}
