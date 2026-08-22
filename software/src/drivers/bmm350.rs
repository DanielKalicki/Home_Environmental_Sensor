//! Driver for the Bosch BMM350 three-axis magnetometer.
//!
//! The BMM350 does not compensate on-chip. The data registers hold raw 24-bit
//! ADC counts for the three magnetic axes and for an internal temperature
//! channel, and the host has to apply a per-chip correction built from 32
//! one-time-programmable (OTP) words. The flow this driver implements is the
//! one from Bosch's reference driver:
//!
//! 1. [`Bmm350::init`] waits out the power-on time, issues a soft reset,
//!    verifies the chip ID, reads all 32 OTP words, powers the OTP block down,
//!    performs a magnetic reset, and programs the requested configuration. It
//!    returns the parsed [`Compensation`] coefficients.
//! 2. [`Bmm350::read_raw_measurement`] pulls the twelve data bytes in one
//!    transfer and sign-extends them into a [`RawMeasurement`].
//! 3. [`Compensation::compensate`] turns that into microtesla and degrees
//!    Celsius.
//!
//! The coefficients are returned to the caller rather than kept in the driver
//! because the driver only borrows the shared I2C bus for one transaction; the
//! task owns the [`Compensation`] between reads, exactly like the BME690
//! calibration data.
//!
//! Two protocol details are easy to get wrong:
//!
//! * Every read returns two dummy bytes before the first real register byte.
//!   [`Bmm350::read_registers`] strips them.
//! * Data-register words are little-endian: XLSB, LSB, MSB.
//!
//! Register addresses, masks, delays and the compensation maths were taken from
//! Bosch's BMM350_SensorAPI v1.10.0 (`bmm350.c` / `bmm350_defs.h`). That version
//! defines but no longer applies the "post solder" corrections to `sens_y` and
//! `tcs_z`, so this driver does not apply them either.

#![allow(dead_code)]

use embassy_time::Timer;
use esp_hal::{
    i2c::{Error as I2cError, Instance, I2C},
    Blocking,
};

/// 7-bit address used when the `ADSEL` pin is pulled low.
pub const DEFAULT_ADDRESS: u8 = 0x14;
/// 7-bit address used when the `ADSEL` pin is pulled high.
pub const ALTERNATE_ADDRESS: u8 = 0x15;

const REG_CHIP_ID: u8 = 0x00;
const REG_ERR_REG: u8 = 0x02;
const REG_PAD_CTRL: u8 = 0x03;
const REG_PMU_CMD_AGGR_SET: u8 = 0x04;
const REG_PMU_CMD_AXIS_EN: u8 = 0x05;
const REG_PMU_CMD: u8 = 0x06;
const REG_PMU_CMD_STATUS_0: u8 = 0x07;
const REG_PMU_CMD_STATUS_1: u8 = 0x08;
const REG_I3C_ERR: u8 = 0x09;
const REG_I2C_WDT_SET: u8 = 0x0A;
const REG_INT_CTRL: u8 = 0x2E;
const REG_INT_STATUS: u8 = 0x30;
/// First of the twelve data bytes: X, Y, Z and temperature, each XLSB/LSB/MSB.
const REG_MAG_X_XLSB: u8 = 0x31;
const REG_SENSORTIME_XLSB: u8 = 0x3D;
const REG_OTP_CMD: u8 = 0x50;
const REG_OTP_DATA_MSB: u8 = 0x52;
const REG_OTP_DATA_LSB: u8 = 0x53;
const REG_OTP_STATUS: u8 = 0x55;
const REG_CMD: u8 = 0x7E;

/// Value `REG_CHIP_ID` holds on a BMM350.
const CHIP_ID: u8 = 0x33;

/// Command that triggers a full software reset, written to `REG_CMD`.
const CMD_SOFT_RESET: u8 = 0xB6;

/// Number of bytes the device prepends to every read before the first real
/// register byte.
const DUMMY_BYTES: usize = 2;
/// Longest register block this driver reads in one transfer, the twelve data
/// bytes. Sizes the scratch buffer in [`Bmm350::read_registers`].
const MAX_READ_LENGTH: usize = 12;

/// Number of data bytes holding X, Y, Z and temperature.
const MAG_TEMP_DATA_LENGTH: usize = 12;
/// Number of OTP words that hold the per-chip compensation data.
pub const OTP_WORD_COUNT: usize = 32;

// Values written to `REG_PMU_CMD`.
const PMU_CMD_SUSPEND: u8 = 0x00;
const PMU_CMD_NORMAL: u8 = 0x01;
/// Latch a new ODR / averaging / axis-enable setting.
const PMU_CMD_UPDATE_ODR_AVG: u8 = 0x02;
const PMU_CMD_FORCED: u8 = 0x03;
const PMU_CMD_FORCED_FAST: u8 = 0x04;
/// Flux guide reset, the second half of a magnetic reset.
const PMU_CMD_FLUX_GUIDE_RESET: u8 = 0x05;
/// Bit reset, the first half of a magnetic reset.
const PMU_CMD_BIT_RESET: u8 = 0x07;

// `odr[3:0]` field of `REG_PMU_CMD_AGGR_SET`, occupying bits 3:0.
const AGGR_SET_ODR_MASK: u8 = 0b0000_1111;
// `avg[1:0]` field of `REG_PMU_CMD_AGGR_SET`, occupying bits 5:4.
const AGGR_SET_AVG_MASK: u8 = 0b0011_0000;
const AGGR_SET_AVG_SHIFT: u8 = 4;

// `en_x`, `en_y` and `en_z` bits of `REG_PMU_CMD_AXIS_EN`, occupying bits 2:0.
const AXIS_EN_X_MASK: u8 = 0b0000_0001;
const AXIS_EN_Y_MASK: u8 = 0b0000_0010;
const AXIS_EN_Z_MASK: u8 = 0b0000_0100;
const AXIS_EN_XYZ_MASK: u8 = 0b0000_0111;

// Bits of `REG_PMU_CMD_STATUS_0`.
/// The previous PMU command is still being processed.
const PMU_STATUS_0_BUSY_MASK: u8 = 0b0000_0001;
/// The requested ODR was overwritten because it did not fit the averaging.
const PMU_STATUS_0_ODR_OVERWRITTEN_MASK: u8 = 0b0000_0010;
/// The requested averaging was overwritten because it did not fit the ODR.
const PMU_STATUS_0_AVG_OVERWRITTEN_MASK: u8 = 0b0000_0100;
/// The device is in normal (periodic) mode.
const PMU_STATUS_0_NORMAL_MODE_MASK: u8 = 0b0000_1000;
/// The last value written to `REG_PMU_CMD` was not a legal command.
const PMU_STATUS_0_ILLEGAL_COMMAND_MASK: u8 = 0b0001_0000;
/// `pmu_cmd_value[2:0]`, the last command the PMU actually executed.
const PMU_STATUS_0_COMMAND_VALUE_MASK: u8 = 0b1110_0000;
const PMU_STATUS_0_COMMAND_VALUE_SHIFT: u8 = 5;

// `drdy_data_reg_en` bit of `REG_INT_CTRL`, occupying bit 7. Cleared, the
// data-ready flag in `REG_INT_STATUS` never goes high however long it is polled.
const INT_CTRL_DRDY_ENABLE_MASK: u8 = 0b1000_0000;
// `drdy_data_reg` bit of `REG_INT_STATUS`, occupying bit 2.
const INT_STATUS_DRDY_MASK: u8 = 0b0000_0100;

// Bits of `REG_OTP_CMD`. The low five bits carry the word address.
const OTP_CMD_DIRECT_READ: u8 = 0x20;
const OTP_CMD_POWER_OFF: u8 = 0x80;
const OTP_WORD_ADDRESS_MASK: u8 = 0x1F;

// Bits of `REG_OTP_STATUS`.
const OTP_STATUS_ERROR_MASK: u8 = 0xE0;
const OTP_STATUS_COMMAND_DONE_MASK: u8 = 0x01;

/// Time the device needs after power-on before it accepts commands.
const START_UP_DELAY_US: u64 = 3000;
/// Execution time of a software reset.
const SOFT_RESET_DELAY_US: u64 = 24000;
/// Settling time after a switch to suspend mode.
const GOTO_SUSPEND_DELAY_US: u64 = 6000;
/// Settling time of a switch from suspend to normal mode.
const SUSPEND_TO_NORMAL_DELAY_US: u64 = 38000;
/// Execution time of a `PMU_CMD_UPDATE_ODR_AVG`.
const UPDATE_ODR_AVG_DELAY_US: u64 = 1000;
/// Execution time of a bit reset.
const BIT_RESET_DELAY_US: u64 = 14000;
/// Execution time of a flux guide reset.
const FLUX_GUIDE_RESET_DELAY_US: u64 = 18000;

/// Conversion time of a forced measurement, indexed by the raw averaging value.
const SUSPEND_TO_FORCED_DELAY_US: [u64; 4] = [15000, 17000, 20000, 28000];
/// Conversion time of a fast forced measurement, indexed by the raw averaging
/// value.
const SUSPEND_TO_FORCED_FAST_DELAY_US: [u64; 4] = [4000, 5000, 9000, 16000];

/// Interval between two polls of `REG_OTP_STATUS` while an OTP word is read.
const OTP_POLL_INTERVAL_US: u64 = 300;
/// How many times [`Bmm350::read_otp_word`] polls before giving up. An OTP read
/// completes in a poll or two, so this is a fault timeout.
const OTP_POLL_ATTEMPTS: usize = 40;

// Analog front-end constants behind the raw-count-to-microtesla scale factors.
// They are fixed design values, not per-chip trim.
/// Sensitivity of the X and Y bridges.
const B_XY_SENSITIVITY: f32 = 14.55;
/// Sensitivity of the Z bridge.
const B_Z_SENSITIVITY: f32 = 9.0;
/// Sensitivity of the temperature channel.
const TEMPERATURE_SENSITIVITY: f32 = 0.00204;
/// Instrumentation-amplifier gain in front of the X and Y bridges.
const INA_XY_GAIN: f32 = 19.46;
/// Instrumentation-amplifier gain in front of the Z bridge.
const INA_Z_GAIN: f32 = 31.0;
const ADC_GAIN: f32 = 1.0 / 1.5;
const LUT_GAIN: f32 = 0.714607238769531;
/// Full-scale count of the 20-bit ADC.
const ADC_FULL_SCALE: f32 = 1048576.0;
const MICRO_PER_UNIT: f32 = 1000000.0;

/// Microtesla per raw count on the X and Y axes.
const LSB_TO_MICROTESLA_XY: f32 =
    (MICRO_PER_UNIT / ADC_FULL_SCALE) / (B_XY_SENSITIVITY * INA_XY_GAIN * ADC_GAIN * LUT_GAIN);
/// Microtesla per raw count on the Z axis.
const LSB_TO_MICROTESLA_Z: f32 =
    (MICRO_PER_UNIT / ADC_FULL_SCALE) / (B_Z_SENSITIVITY * INA_Z_GAIN * ADC_GAIN * LUT_GAIN);
/// Degrees Celsius per raw count on the temperature channel.
const LSB_TO_CELSIUS: f32 = 1.0 / (TEMPERATURE_SENSITIVITY * ADC_GAIN * LUT_GAIN * ADC_FULL_SCALE);
/// Fixed offset subtracted from the scaled temperature count.
const TEMPERATURE_OFFSET_CELSIUS: f32 = 25.49;

// Indices into the OTP word array. Several coefficients share a word, one in
// each byte, so the same index appears more than once.
const OTP_TEMPERATURE_OFFSET_AND_SENSITIVITY: usize = 0x0D;
const OTP_MAG_OFFSET_X: usize = 0x0E;
const OTP_MAG_OFFSET_Y: usize = 0x0F;
const OTP_MAG_OFFSET_Z: usize = 0x10;
const OTP_MAG_SENSITIVITY_X: usize = 0x10;
const OTP_MAG_SENSITIVITY_Y: usize = 0x11;
const OTP_MAG_SENSITIVITY_Z: usize = 0x11;
const OTP_MAG_TCO_X: usize = 0x12;
const OTP_MAG_TCO_Y: usize = 0x13;
const OTP_MAG_TCO_Z: usize = 0x14;
const OTP_MAG_TCS_X: usize = 0x12;
const OTP_MAG_TCS_Y: usize = 0x13;
const OTP_MAG_TCS_Z: usize = 0x14;
const OTP_CROSS_X_Y: usize = 0x15;
const OTP_CROSS_Y_X: usize = 0x15;
const OTP_CROSS_Z_X: usize = 0x16;
const OTP_CROSS_Z_Y: usize = 0x16;
const OTP_REFERENCE_TEMPERATURE: usize = 0x18;
/// Word holding the variant ID in bits 14:9.
const OTP_VARIANT_ID: usize = 30;

/// Errors reported by the driver.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Error {
    /// The I2C transfer itself failed (no acknowledge, timeout, ...).
    Bus(I2cError),
    /// `REG_CHIP_ID` did not hold `0x33`.
    InvalidChipId(u8),
    /// An OTP read reported an error. Carries the masked `REG_OTP_STATUS` code.
    Otp(u8),
    /// An OTP read never reported completion.
    OtpTimeout,
    /// The PMU executed a different command than the one just written, so the
    /// step it was meant to perform did not happen.
    UnexpectedPmuCommand {
        /// The command that was written to `REG_PMU_CMD`.
        expected: u8,
        /// The command `REG_PMU_CMD_STATUS_0` reported instead.
        actual: u8,
    },
    /// A configuration disabled all three magnetic axes, which the device does
    /// not accept.
    AllAxesDisabled,
}

impl From<I2cError> for Error {
    fn from(error: I2cError) -> Self {
        Error::Bus(error)
    }
}

/// Rate at which the device measures in normal mode.
///
/// The rate is ignored in forced mode, where each measurement is triggered by
/// the host, but it still constrains [`Averaging`]: the device silently lowers
/// an averaging setting that does not fit into the ODR period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataRate {
    Hz400,
    Hz200,
    Hz100,
    Hz50,
    Hz25,
    Hz12_5,
    Hz6_25,
    Hz3_125,
    Hz1_5625,
}

impl DataRate {
    /// Register encoding of this setting.
    pub const fn raw(self) -> u8 {
        match self {
            DataRate::Hz400 => 0x02,
            DataRate::Hz200 => 0x03,
            DataRate::Hz100 => 0x04,
            DataRate::Hz50 => 0x05,
            DataRate::Hz25 => 0x06,
            DataRate::Hz12_5 => 0x07,
            DataRate::Hz6_25 => 0x08,
            DataRate::Hz3_125 => 0x09,
            DataRate::Hz1_5625 => 0x0A,
        }
    }
}

/// Number of conversions the device averages into one measurement.
///
/// Averaging lowers noise and lengthens the measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Averaging {
    /// A single conversion per measurement.
    None,
    X2,
    X4,
    X8,
}

impl Averaging {
    /// Register encoding of this setting.
    pub const fn raw(self) -> u8 {
        match self {
            Averaging::None => 0,
            Averaging::X2 => 1,
            Averaging::X4 => 2,
            Averaging::X8 => 3,
        }
    }

    /// Decode a register field. Every one of the four encodings is valid.
    pub const fn from_raw(raw: u8) -> Self {
        match raw & 0x03 {
            0 => Averaging::None,
            1 => Averaging::X2,
            2 => Averaging::X4,
            _ => Averaging::X8,
        }
    }
}

/// Power mode held in `pmu_cmd[3:0]` of `REG_PMU_CMD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerMode {
    /// Idle. The magnetic reset commands are only accepted here.
    Suspend,
    /// Periodic measurements at the configured data rate.
    Normal,
    /// One measurement, after which the device returns to suspend by itself.
    Forced,
    /// Like [`PowerMode::Forced`] but with a shortened settling phase, which is
    /// faster and noisier.
    ForcedFast,
}

impl PowerMode {
    /// Register encoding of this mode.
    pub const fn raw(self) -> u8 {
        match self {
            PowerMode::Suspend => PMU_CMD_SUSPEND,
            PowerMode::Normal => PMU_CMD_NORMAL,
            PowerMode::Forced => PMU_CMD_FORCED,
            PowerMode::ForcedFast => PMU_CMD_FORCED_FAST,
        }
    }

    /// How long the device needs to reach this mode and finish the measurement
    /// it implies, given the averaging currently programmed.
    const fn settling_delay_us(self, averaging: Averaging) -> u64 {
        let index = averaging.raw() as usize;
        match self {
            PowerMode::Suspend => GOTO_SUSPEND_DELAY_US,
            PowerMode::Normal => SUSPEND_TO_NORMAL_DELAY_US,
            PowerMode::Forced => SUSPEND_TO_FORCED_DELAY_US[index],
            PowerMode::ForcedFast => SUSPEND_TO_FORCED_FAST_DELAY_US[index],
        }
    }
}

/// Which of the three magnetic axes the device measures.
///
/// A disabled axis saves power, but its data register keeps whatever it held
/// last, so the corresponding output is meaningless rather than zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Axes {
    pub x: bool,
    pub y: bool,
    pub z: bool,
}

impl Axes {
    /// All three axes enabled.
    pub const ALL: Self = Self {
        x: true,
        y: true,
        z: true,
    };

    /// Register encoding of this setting.
    pub const fn raw(self) -> u8 {
        let mut raw = 0;
        if self.x {
            raw |= AXIS_EN_X_MASK;
        }
        if self.y {
            raw |= AXIS_EN_Y_MASK;
        }
        if self.z {
            raw |= AXIS_EN_Z_MASK;
        }
        raw
    }

    /// Whether at least one axis is enabled.
    pub const fn any(self) -> bool {
        self.x || self.y || self.z
    }
}

impl Default for Axes {
    fn default() -> Self {
        Self::ALL
    }
}

/// Settings applied by [`Bmm350::init`] and [`Bmm350::configure`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Configuration {
    /// Measurement rate used in normal mode.
    pub data_rate: DataRate,
    /// Conversions averaged into one measurement.
    pub averaging: Averaging,
    /// Which magnetic axes to measure.
    pub axes: Axes,
    /// Whether finished measurements raise the data-ready flag in
    /// `REG_INT_STATUS`. Required for [`Bmm350::data_ready`]; it does not drive
    /// the physical interrupt pin.
    pub data_ready_status_enabled: bool,
}

impl Default for Configuration {
    /// Slow, heavily averaged one-shot settings, suited to a stationary sensor
    /// that is polled every few seconds.
    fn default() -> Self {
        Self {
            data_rate: DataRate::Hz25,
            averaging: Averaging::X4,
            axes: Axes::ALL,
            data_ready_status_enabled: true,
        }
    }
}

/// Decoded contents of `REG_PMU_CMD_STATUS_0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PmuStatus {
    /// The previous command is still being processed.
    pub busy: bool,
    /// The requested data rate was lowered to fit the averaging.
    pub data_rate_overwritten: bool,
    /// The requested averaging was lowered to fit the data rate.
    pub averaging_overwritten: bool,
    /// The device is in normal mode.
    pub normal_mode: bool,
    /// The last value written to `REG_PMU_CMD` was not a legal command.
    pub illegal_command: bool,
    /// The last command the PMU actually executed.
    pub command_value: u8,
}

impl PmuStatus {
    const fn from_raw(raw: u8) -> Self {
        Self {
            busy: raw & PMU_STATUS_0_BUSY_MASK != 0,
            data_rate_overwritten: raw & PMU_STATUS_0_ODR_OVERWRITTEN_MASK != 0,
            averaging_overwritten: raw & PMU_STATUS_0_AVG_OVERWRITTEN_MASK != 0,
            normal_mode: raw & PMU_STATUS_0_NORMAL_MODE_MASK != 0,
            illegal_command: raw & PMU_STATUS_0_ILLEGAL_COMMAND_MASK != 0,
            command_value: (raw & PMU_STATUS_0_COMMAND_VALUE_MASK)
                >> PMU_STATUS_0_COMMAND_VALUE_SHIFT,
        }
    }
}

/// One set of data registers, sign-extended but otherwise untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RawMeasurement {
    /// Sign-extended 24-bit X count.
    pub x: i32,
    /// Sign-extended 24-bit Y count.
    pub y: i32,
    /// Sign-extended 24-bit Z count.
    pub z: i32,
    /// Sign-extended 24-bit temperature count.
    pub temperature: i32,
}

/// One fully compensated result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    /// X flux density in microtesla.
    pub x_microtesla: f32,
    /// Y flux density in microtesla.
    pub y_microtesla: f32,
    /// Z flux density in microtesla.
    pub z_microtesla: f32,
    /// Temperature of the sensor die in degrees Celsius. This is the die, which
    /// self-heats, not the ambient air.
    pub temperature_celsius: f32,
}

impl Measurement {
    /// Squared magnitude of the flux-density vector, in microtesla squared.
    ///
    /// Useful as a plausibility check without pulling in a square root: on an
    /// undisturbed sensor it should stay close to the square of the local
    /// geomagnetic field strength however the device is turned.
    pub fn magnitude_squared_microtesla(&self) -> f32 {
        self.x_microtesla * self.x_microtesla
            + self.y_microtesla * self.y_microtesla
            + self.z_microtesla * self.z_microtesla
    }
}

/// Per-chip compensation coefficients, parsed out of the OTP words.
///
/// Read once by [`Bmm350::init`] and then kept by the caller, because the
/// driver itself does not survive between bus transactions.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Compensation {
    /// Additive offset of each magnetic axis, in microtesla.
    offset: [f32; 3],
    /// Relative sensitivity error of each magnetic axis.
    sensitivity: [f32; 3],
    /// Temperature coefficient of the offset, in microtesla per degree.
    tco: [f32; 3],
    /// Temperature coefficient of the sensitivity, per degree.
    tcs: [f32; 3],
    /// Additive offset of the temperature channel, in degrees Celsius.
    temperature_offset: f32,
    /// Relative sensitivity error of the temperature channel.
    temperature_sensitivity: f32,
    /// Temperature at which the offset and sensitivity coefficients apply.
    reference_temperature_celsius: f32,
    /// Fraction of the Y axis that leaks into the X reading.
    cross_x_y: f32,
    /// Fraction of the X axis that leaks into the Y reading.
    cross_y_x: f32,
    /// Fraction of the X axis that leaks into the Z reading.
    cross_z_x: f32,
    /// Fraction of the Y axis that leaks into the Z reading.
    cross_z_y: f32,
    /// Variant ID of the part, from OTP word 30.
    variant_id: u8,
}

impl Compensation {
    /// Parse the coefficients out of the 32 OTP words.
    pub fn from_otp(otp: &[u16; OTP_WORD_COUNT]) -> Self {
        // The three 12-bit magnetic offsets are packed across three words.
        let offset_x = otp[OTP_MAG_OFFSET_X] & 0x0FFF;
        let offset_y =
            ((otp[OTP_MAG_OFFSET_X] & 0xF000) >> 4) | (otp[OTP_MAG_OFFSET_Y] & 0x00FF);
        let offset_z = (otp[OTP_MAG_OFFSET_Y] & 0x0F00) | (otp[OTP_MAG_OFFSET_Z] & 0x00FF);
        let temperature_offset = otp[OTP_TEMPERATURE_OFFSET_AND_SENSITIVITY] & 0x00FF;

        let sensitivity_x = (otp[OTP_MAG_SENSITIVITY_X] & 0xFF00) >> 8;
        let sensitivity_y = otp[OTP_MAG_SENSITIVITY_Y] & 0x00FF;
        let sensitivity_z = (otp[OTP_MAG_SENSITIVITY_Z] & 0xFF00) >> 8;
        let temperature_sensitivity = (otp[OTP_TEMPERATURE_OFFSET_AND_SENSITIVITY] & 0xFF00) >> 8;

        let tco_x = otp[OTP_MAG_TCO_X] & 0x00FF;
        let tco_y = otp[OTP_MAG_TCO_Y] & 0x00FF;
        let tco_z = otp[OTP_MAG_TCO_Z] & 0x00FF;

        let tcs_x = (otp[OTP_MAG_TCS_X] & 0xFF00) >> 8;
        let tcs_y = (otp[OTP_MAG_TCS_Y] & 0xFF00) >> 8;
        let tcs_z = (otp[OTP_MAG_TCS_Z] & 0xFF00) >> 8;

        let cross_x_y = otp[OTP_CROSS_X_Y] & 0x00FF;
        let cross_y_x = (otp[OTP_CROSS_Y_X] & 0xFF00) >> 8;
        let cross_z_x = otp[OTP_CROSS_Z_X] & 0x00FF;
        let cross_z_y = (otp[OTP_CROSS_Z_Y] & 0xFF00) >> 8;

        Self {
            offset: [
                sign_extend(offset_x as u32, 12) as f32,
                sign_extend(offset_y as u32, 12) as f32,
                sign_extend(offset_z as u32, 12) as f32,
            ],
            sensitivity: [
                sign_extend(sensitivity_x as u32, 8) as f32 / 256.0,
                sign_extend(sensitivity_y as u32, 8) as f32 / 256.0,
                sign_extend(sensitivity_z as u32, 8) as f32 / 256.0,
            ],
            tco: [
                sign_extend(tco_x as u32, 8) as f32 / 32.0,
                sign_extend(tco_y as u32, 8) as f32 / 32.0,
                sign_extend(tco_z as u32, 8) as f32 / 32.0,
            ],
            tcs: [
                sign_extend(tcs_x as u32, 8) as f32 / 16384.0,
                sign_extend(tcs_y as u32, 8) as f32 / 16384.0,
                sign_extend(tcs_z as u32, 8) as f32 / 16384.0,
            ],
            temperature_offset: sign_extend(temperature_offset as u32, 8) as f32 / 5.0,
            temperature_sensitivity: sign_extend(temperature_sensitivity as u32, 8) as f32 / 512.0,
            reference_temperature_celsius: sign_extend(
                otp[OTP_REFERENCE_TEMPERATURE] as u32,
                16,
            ) as f32
                / 512.0
                + 23.0,
            cross_x_y: sign_extend(cross_x_y as u32, 8) as f32 / 800.0,
            cross_y_x: sign_extend(cross_y_x as u32, 8) as f32 / 800.0,
            cross_z_x: sign_extend(cross_z_x as u32, 8) as f32 / 800.0,
            cross_z_y: sign_extend(cross_z_y as u32, 8) as f32 / 800.0,
            variant_id: ((otp[OTP_VARIANT_ID] & 0x7F00) >> 9) as u8,
        }
    }

    /// Variant ID of the part, from OTP word 30.
    pub fn variant_id(&self) -> u8 {
        self.variant_id
    }

    /// Turn raw counts into microtesla and degrees Celsius.
    ///
    /// The steps, in the order the device's data sheet requires them: scale the
    /// counts, compensate the temperature channel, apply the per-axis
    /// sensitivity, offset and the two temperature coefficients, then undo the
    /// cross-axis leakage between the three axes.
    pub fn compensate(&self, raw: &RawMeasurement) -> Measurement {
        let temperature_count = raw.temperature as f32 * LSB_TO_CELSIUS - TEMPERATURE_OFFSET_CELSIUS;
        let temperature =
            (1.0 + self.temperature_sensitivity) * temperature_count + self.temperature_offset;
        let temperature_delta = temperature - self.reference_temperature_celsius;

        let mut axis = [
            raw.x as f32 * LSB_TO_MICROTESLA_XY,
            raw.y as f32 * LSB_TO_MICROTESLA_XY,
            raw.z as f32 * LSB_TO_MICROTESLA_Z,
        ];

        for index in 0..3 {
            axis[index] *= 1.0 + self.sensitivity[index];
            axis[index] += self.offset[index];
            axis[index] += self.tco[index] * temperature_delta;
            axis[index] /= 1.0 + self.tcs[index] * temperature_delta;
        }

        let denominator = 1.0 - self.cross_y_x * self.cross_x_y;
        let x = (axis[0] - self.cross_x_y * axis[1]) / denominator;
        let y = (axis[1] - self.cross_y_x * axis[0]) / denominator;
        let z = axis[2]
            + (axis[0] * (self.cross_y_x * self.cross_z_y - self.cross_z_x)
                - axis[1] * (self.cross_z_y - self.cross_x_y * self.cross_z_x))
                / denominator;

        Measurement {
            x_microtesla: x,
            y_microtesla: y,
            z_microtesla: z,
            temperature_celsius: temperature,
        }
    }
}

/// Reinterpret the low `bits` of `value` as a two's-complement signed number.
const fn sign_extend(value: u32, bits: u32) -> i32 {
    let half = 1i32 << (bits - 1);
    let value = (value & ((1u32 << bits) - 1)) as i32;
    if value >= half {
        value - half * 2
    } else {
        value
    }
}

/// BMM350 attached to an esp-hal I2C bus.
///
/// The I2C transfers themselves are blocking, but every datasheet-mandated
/// execution delay yields to the Embassy executor instead of busy-waiting.
pub struct Bmm350<'a, 'd, T> {
    i2c: &'a mut I2C<'d, T, Blocking>,
    address: u8,
}

impl<'a, 'd, T: Instance> Bmm350<'a, 'd, T> {
    /// Bind the driver to a bus, using the `ADSEL`-low address.
    pub fn new(i2c: &'a mut I2C<'d, T, Blocking>) -> Self {
        Self::with_address(i2c, DEFAULT_ADDRESS)
    }

    /// Bind the driver to a bus using an explicit 7-bit address.
    pub fn with_address(i2c: &'a mut I2C<'d, T, Blocking>, address: u8) -> Self {
        Self { i2c, address }
    }

    /// Read one or more consecutive registers starting at `register`.
    ///
    /// The device sends two dummy bytes before the first real register byte, so
    /// the transfer is two bytes longer than `out` and the dummies are dropped.
    async fn read_registers(&mut self, register: u8, out: &mut [u8]) -> Result<(), Error> {
        let mut raw = [0u8; DUMMY_BYTES + MAX_READ_LENGTH];
        let total = DUMMY_BYTES + out.len();
        self.i2c
            .write_read(self.address, &[register], &mut raw[..total])?;
        out.copy_from_slice(&raw[DUMMY_BYTES..total]);
        Ok(())
    }

    /// Read a single register.
    async fn read_register(&mut self, register: u8) -> Result<u8, Error> {
        let mut raw = [0u8; 1];
        self.read_registers(register, &mut raw).await?;
        Ok(raw[0])
    }

    /// Write a single register.
    async fn write_register(&mut self, register: u8, value: u8) -> Result<(), Error> {
        self.i2c.write(self.address, &[register, value])?;
        Ok(())
    }

    /// Read a register, replace the bits selected by `mask`, and write it back.
    ///
    /// Used wherever a register mixes fields owned by different settings, so
    /// that changing one does not clear the others.
    async fn update_register(&mut self, register: u8, mask: u8, value: u8) -> Result<(), Error> {
        let current = self.read_register(register).await?;
        let updated = (current & !mask) | (value & mask);
        self.write_register(register, updated).await
    }

    /// Read `REG_CHIP_ID`; useful as a presence check.
    pub async fn chip_id(&mut self) -> Result<u8, Error> {
        self.read_register(REG_CHIP_ID).await
    }

    /// Read `REG_ERR_REG`, which flags a rejected PMU command.
    pub async fn error_register(&mut self) -> Result<u8, Error> {
        self.read_register(REG_ERR_REG).await
    }

    /// Read and decode `REG_PMU_CMD_STATUS_0`.
    pub async fn pmu_status(&mut self) -> Result<PmuStatus, Error> {
        let raw = self.read_register(REG_PMU_CMD_STATUS_0).await?;
        Ok(PmuStatus::from_raw(raw))
    }

    /// Trigger a software reset and wait for it to complete.
    ///
    /// Afterwards every register holds its power-on default and the device is in
    /// suspend mode with the OTP block powered up but not yet read out.
    pub async fn soft_reset(&mut self) -> Result<(), Error> {
        self.write_register(REG_CMD, CMD_SOFT_RESET).await?;
        Timer::after_micros(SOFT_RESET_DELAY_US).await;
        Ok(())
    }

    /// Read the averaging setting currently programmed.
    ///
    /// Needed to pick the right settling delay, because a forced measurement
    /// takes longer the more conversions it averages.
    pub async fn averaging(&mut self) -> Result<Averaging, Error> {
        let raw = self.read_register(REG_PMU_CMD_AGGR_SET).await?;
        Ok(Averaging::from_raw(
            (raw & AGGR_SET_AVG_MASK) >> AGGR_SET_AVG_SHIFT,
        ))
    }

    /// Read one OTP word through the direct-read command.
    ///
    /// Only the low five bits of `word_address` are used, matching the 32-word
    /// OTP block.
    pub async fn read_otp_word(&mut self, word_address: u8) -> Result<u16, Error> {
        self.write_register(
            REG_OTP_CMD,
            OTP_CMD_DIRECT_READ | (word_address & OTP_WORD_ADDRESS_MASK),
        )
        .await?;

        for _ in 0..OTP_POLL_ATTEMPTS {
            Timer::after_micros(OTP_POLL_INTERVAL_US).await;

            let status = self.read_register(REG_OTP_STATUS).await?;
            let error = status & OTP_STATUS_ERROR_MASK;
            if error != 0 {
                return Err(Error::Otp(error));
            }
            if status & OTP_STATUS_COMMAND_DONE_MASK != 0 {
                let mut raw = [0u8; 2];
                self.read_registers(REG_OTP_DATA_MSB, &mut raw).await?;
                return Ok(u16::from_be_bytes(raw));
            }
        }

        Err(Error::OtpTimeout)
    }

    /// Read the whole 32-word OTP block.
    pub async fn read_otp(&mut self) -> Result<[u16; OTP_WORD_COUNT], Error> {
        let mut otp = [0u16; OTP_WORD_COUNT];
        for (index, word) in otp.iter_mut().enumerate() {
            *word = self.read_otp_word(index as u8).await?;
        }
        Ok(otp)
    }

    /// Power the OTP block down.
    ///
    /// The coefficients have been copied into RAM by then, so keeping the block
    /// alive only costs current.
    pub async fn power_off_otp(&mut self) -> Result<(), Error> {
        self.write_register(REG_OTP_CMD, OTP_CMD_POWER_OFF).await
    }

    /// Write a raw PMU command and wait `delay_us`, then confirm the PMU
    /// executed exactly that command.
    async fn run_pmu_command(&mut self, command: u8, delay_us: u64) -> Result<(), Error> {
        self.write_register(REG_PMU_CMD, command).await?;
        Timer::after_micros(delay_us).await;

        let status = self.pmu_status().await?;
        if status.command_value != command {
            return Err(Error::UnexpectedPmuCommand {
                expected: command,
                actual: status.command_value,
            });
        }
        Ok(())
    }

    /// Select a power mode and wait for the device to reach it.
    ///
    /// A mode change is only accepted from suspend, so a device that is in
    /// normal mode (or still latching a new ODR) is driven to suspend first.
    /// The settling delay depends on the averaging currently programmed, which
    /// is therefore read back here.
    pub async fn set_power_mode(&mut self, mode: PowerMode) -> Result<(), Error> {
        let current = self.read_register(REG_PMU_CMD).await?;
        if current == PMU_CMD_NORMAL || current == PMU_CMD_UPDATE_ODR_AVG {
            self.write_register(REG_PMU_CMD, PMU_CMD_SUSPEND).await?;
            Timer::after_micros(GOTO_SUSPEND_DELAY_US).await;
        }

        let averaging = self.averaging().await?;
        self.write_register(REG_PMU_CMD, mode.raw()).await?;
        Timer::after_micros(mode.settling_delay_us(averaging)).await;
        Ok(())
    }

    /// Degauss the sensor and restore the mode it was in.
    ///
    /// The magnetoresistive bridges keep a remanent magnetisation after a strong
    /// external field, which shows up as a large fixed offset. A bit reset
    /// followed by a flux guide reset clears it. Both commands are only accepted
    /// in suspend mode, so a device in normal mode is parked and restarted.
    pub async fn magnetic_reset(&mut self) -> Result<(), Error> {
        let was_normal = self.pmu_status().await?.normal_mode;
        if was_normal {
            self.set_power_mode(PowerMode::Suspend).await?;
        }

        self.run_pmu_command(PMU_CMD_BIT_RESET, BIT_RESET_DELAY_US)
            .await?;
        self.run_pmu_command(PMU_CMD_FLUX_GUIDE_RESET, FLUX_GUIDE_RESET_DELAY_US)
            .await?;

        if was_normal {
            self.set_power_mode(PowerMode::Normal).await?;
        }
        Ok(())
    }

    /// Program the data rate and averaging, then latch them.
    ///
    /// The device silently lowers an averaging setting that does not fit into
    /// the ODR period, so the combination is clamped here instead, which keeps
    /// the value written and the value in effect in agreement.
    pub async fn set_data_rate_and_averaging(
        &mut self,
        data_rate: DataRate,
        averaging: Averaging,
    ) -> Result<(), Error> {
        let averaging = match data_rate {
            DataRate::Hz400 if averaging > Averaging::None => Averaging::None,
            DataRate::Hz200 if averaging > Averaging::X2 => Averaging::X2,
            DataRate::Hz100 if averaging > Averaging::X4 => Averaging::X4,
            _ => averaging,
        };

        self.update_register(
            REG_PMU_CMD_AGGR_SET,
            AGGR_SET_ODR_MASK | AGGR_SET_AVG_MASK,
            data_rate.raw() | (averaging.raw() << AGGR_SET_AVG_SHIFT),
        )
        .await?;

        self.write_register(REG_PMU_CMD, PMU_CMD_UPDATE_ODR_AVG)
            .await?;
        Timer::after_micros(UPDATE_ODR_AVG_DELAY_US).await;
        Ok(())
    }

    /// Select which magnetic axes are measured, then latch the choice.
    pub async fn set_axes(&mut self, axes: Axes) -> Result<(), Error> {
        if !axes.any() {
            return Err(Error::AllAxesDisabled);
        }

        self.update_register(REG_PMU_CMD_AXIS_EN, AXIS_EN_XYZ_MASK, axes.raw())
            .await?;

        self.write_register(REG_PMU_CMD, PMU_CMD_UPDATE_ODR_AVG)
            .await?;
        Timer::after_micros(UPDATE_ODR_AVG_DELAY_US).await;
        Ok(())
    }

    /// Enable or disable the data-ready flag in `REG_INT_STATUS`.
    ///
    /// This only gates the status bit. Driving the physical interrupt pin would
    /// need the remaining fields of `REG_INT_CTRL`, and nothing here uses it.
    pub async fn set_data_ready_status_enabled(&mut self, enabled: bool) -> Result<(), Error> {
        let value = if enabled { INT_CTRL_DRDY_ENABLE_MASK } else { 0 };
        self.update_register(REG_INT_CTRL, INT_CTRL_DRDY_ENABLE_MASK, value)
            .await
    }

    /// Apply `configuration` to a device that is already in suspend mode.
    pub async fn configure(&mut self, configuration: &Configuration) -> Result<(), Error> {
        self.set_axes(configuration.axes).await?;
        self.set_data_rate_and_averaging(configuration.data_rate, configuration.averaging)
            .await?;
        self.set_data_ready_status_enabled(configuration.data_ready_status_enabled)
            .await
    }

    /// Reset the device, verify it, load its compensation data, and apply
    /// `configuration`.
    ///
    /// The device is left in suspend mode, ready for
    /// [`Bmm350::measure_forced`], and the returned coefficients have to be kept
    /// by the caller for [`Compensation::compensate`].
    pub async fn init(&mut self, configuration: &Configuration) -> Result<Compensation, Error> {
        if !configuration.axes.any() {
            return Err(Error::AllAxesDisabled);
        }

        Timer::after_micros(START_UP_DELAY_US).await;
        self.soft_reset().await?;

        let chip_id = self.chip_id().await?;
        if chip_id != CHIP_ID {
            return Err(Error::InvalidChipId(chip_id));
        }

        let otp = self.read_otp().await?;
        self.power_off_otp().await?;
        let compensation = Compensation::from_otp(&otp);

        // Clear any remanent magnetisation left over from before the reset, so
        // the first measurements are not offset by it.
        self.magnetic_reset().await?;

        self.configure(configuration).await?;
        Ok(compensation)
    }

    /// Whether a measurement has been written to the data registers.
    ///
    /// Reads the clear-on-read `REG_INT_STATUS`, so a `true` is reported only
    /// once per measurement, and only if the data-ready status flag was enabled.
    pub async fn data_ready(&mut self) -> Result<bool, Error> {
        let status = self.read_register(REG_INT_STATUS).await?;
        Ok(status & INT_STATUS_DRDY_MASK != 0)
    }

    /// Read the twelve data bytes in one transfer.
    ///
    /// Does not check the data-ready flag, so a call made before the measurement
    /// finished returns the previous one. Values for disabled axes are stale
    /// rather than zero.
    pub async fn read_raw_measurement(&mut self) -> Result<RawMeasurement, Error> {
        let mut raw = [0u8; MAG_TEMP_DATA_LENGTH];
        self.read_registers(REG_MAG_X_XLSB, &mut raw).await?;

        let word = |chunk: &[u8]| {
            sign_extend(
                chunk[0] as u32 | (chunk[1] as u32) << 8 | (chunk[2] as u32) << 16,
                24,
            )
        };

        Ok(RawMeasurement {
            x: word(&raw[0..3]),
            y: word(&raw[3..6]),
            z: word(&raw[6..9]),
            temperature: word(&raw[9..12]),
        })
    }

    /// Read the free-running 24-bit sensor time counter, in ticks.
    pub async fn read_sensor_time(&mut self) -> Result<u32, Error> {
        let mut raw = [0u8; 3];
        self.read_registers(REG_SENSORTIME_XLSB, &mut raw).await?;
        Ok(raw[0] as u32 | (raw[1] as u32) << 8 | (raw[2] as u32) << 16)
    }

    /// Trigger one forced measurement, wait for it, and return it compensated.
    ///
    /// [`Bmm350::set_power_mode`] already waits out the conversion, which is why
    /// there is no data-ready poll here. The device returns to suspend by
    /// itself.
    pub async fn measure_forced(
        &mut self,
        compensation: &Compensation,
    ) -> Result<Measurement, Error> {
        self.set_power_mode(PowerMode::Forced).await?;
        let raw = self.read_raw_measurement().await?;
        Ok(compensation.compensate(&raw))
    }
}
