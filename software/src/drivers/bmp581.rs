//! Driver for the Bosch BMP581 barometric pressure / temperature sensor.
//!
//! Unlike the BME690 the BMP581 compensates on-chip: there are no calibration
//! coefficients to read out and no compensation maths in the host. The data
//! registers hold ready-made values, a 24-bit two's-complement temperature in
//! 1/65536 °C and a 24-bit unsigned pressure in 1/64 Pa.
//!
//! Only the forced ("one shot") measurement flow is implemented:
//!
//! 1. `init` resets the device, checks the chip ID and NVM status, and programs
//!    oversampling, the IIR filter and the data-ready status flag while the
//!    device sits in standby.
//! 2. `start_measurement` switches the device to forced mode, which takes one
//!    measurement and falls back to standby on its own.
//! 3. `data_ready` reports the `drdy_data_reg` flag, and `read_measurement`
//!    pulls the six data bytes in one transfer.
//!
//! Splitting those last steps lets the caller release a shared bus while the
//! device converts. [`Bmp581::measure`] chains them for callers that own the
//! bus anyway.
//!
//! Most configuration registers are only writable in standby mode, so every
//! method that programs one drives the device to standby first.
//!
//! Multi-byte quantities are little-endian: the register block starts with the
//! least significant byte.

#![allow(dead_code)]

use embassy_time::Timer;
use esp_hal::{
    i2c::{Error as I2cError, Instance, I2C},
    Blocking,
};

/// 7-bit address used when `SDO` is pulled low.
pub const DEFAULT_ADDRESS: u8 = 0x46;
/// 7-bit address used when `SDO` is pulled high.
pub const ALTERNATE_ADDRESS: u8 = 0x47;

const REG_CHIP_ID: u8 = 0x01;
const REG_REV_ID: u8 = 0x02;
const REG_CHIP_STATUS: u8 = 0x11;
const REG_DRIVE_CONFIG: u8 = 0x13;
const REG_INT_CONFIG: u8 = 0x14;
const REG_INT_SOURCE: u8 = 0x15;
const REG_FIFO_CONFIG: u8 = 0x16;
const REG_FIFO_COUNT: u8 = 0x17;
const REG_FIFO_SEL: u8 = 0x18;
/// First of the six data bytes: temperature XLSB/LSB/MSB then pressure
/// XLSB/LSB/MSB.
const REG_TEMP_DATA_XLSB: u8 = 0x1D;
const REG_PRESS_DATA_XLSB: u8 = 0x20;
const REG_INT_STATUS: u8 = 0x27;
const REG_STATUS: u8 = 0x28;
const REG_FIFO_DATA: u8 = 0x29;
const REG_DSP_CONFIG: u8 = 0x30;
const REG_DSP_IIR: u8 = 0x31;
const REG_OSR_CONFIG: u8 = 0x36;
const REG_ODR_CONFIG: u8 = 0x37;
const REG_OSR_EFF: u8 = 0x38;
const REG_CMD: u8 = 0x7E;

/// Value `REG_CHIP_ID` holds on a BMP581.
const CHIP_ID_PRIMARY: u8 = 0x50;
/// Second chip ID the BMP5 family reports; accepted as well.
const CHIP_ID_SECONDARY: u8 = 0x51;

/// Command that triggers a full software reset, written to `REG_CMD`.
const CMD_SOFT_RESET: u8 = 0xB6;

// `osr_t[2:0]` field of `REG_OSR_CONFIG`, occupying bits 2:0.
const OSR_CONFIG_OSR_T_MASK: u8 = 0b0000_0111;
// `osr_p[2:0]` field of `REG_OSR_CONFIG`, occupying bits 5:3.
const OSR_CONFIG_OSR_P_MASK: u8 = 0b0011_1000;
const OSR_CONFIG_OSR_P_SHIFT: u8 = 3;
// `press_en` bit of `REG_OSR_CONFIG`, occupying bit 6. Cleared, the device
// measures temperature only and the pressure registers stay at their last value.
const OSR_CONFIG_PRESS_EN_MASK: u8 = 0b0100_0000;

// `pwr_mode[1:0]` field of `REG_ODR_CONFIG`, occupying bits 1:0.
const ODR_CONFIG_POWER_MODE_MASK: u8 = 0b0000_0011;
// `odr[4:0]` field of `REG_ODR_CONFIG`, occupying bits 6:2. Only used by the
// periodic modes, so this driver leaves it at its reset value.
const ODR_CONFIG_ODR_MASK: u8 = 0b0111_1100;
// `deep_dis` bit of `REG_ODR_CONFIG`, occupying bit 7. Set to 1 the deep
// standby mode is disabled; cleared, standby with a slow ODR, bypassed IIR and
// a disabled FIFO silently becomes deep standby.
const ODR_CONFIG_DEEP_DISABLE_MASK: u8 = 0b1000_0000;

// `set_iir_t[2:0]` field of `REG_DSP_IIR`, occupying bits 2:0.
const DSP_IIR_SET_IIR_T_MASK: u8 = 0b0000_0111;
// `set_iir_p[2:0]` field of `REG_DSP_IIR`, occupying bits 5:3.
const DSP_IIR_SET_IIR_P_MASK: u8 = 0b0011_1000;
const DSP_IIR_SET_IIR_P_SHIFT: u8 = 3;

// `iir_flush_forced_en` bit of `REG_DSP_CONFIG`, occupying bit 2. Set, the IIR
// filter is flushed before each forced measurement, so a one-shot reading does
// not carry state over from the previous one.
const DSP_CONFIG_IIR_FLUSH_FORCED_EN_MASK: u8 = 0b0000_0100;
// `shdw_sel_iir_t` bit of `REG_DSP_CONFIG`, occupying bit 3. Set, the
// temperature data registers return the IIR-filtered value instead of the raw
// one.
const DSP_CONFIG_SHDW_SEL_IIR_T_MASK: u8 = 0b0000_1000;
// `shdw_sel_iir_p` bit of `REG_DSP_CONFIG`, occupying bit 5, the pressure
// counterpart of `shdw_sel_iir_t`.
const DSP_CONFIG_SHDW_SEL_IIR_P_MASK: u8 = 0b0010_0000;

// Bits of `REG_INT_SOURCE`, which gates which events reach `REG_INT_STATUS`.
// The register resets to zero, so a flag no source enables is never raised, no
// matter what the device does.
/// `drdy_data_reg_en`, occupying bit 0: report finished measurements.
const INT_SOURCE_DRDY_EN_MASK: u8 = 0b0000_0001;
const INT_SOURCE_FIFO_FULL_EN_MASK: u8 = 0b0000_0010;
const INT_SOURCE_FIFO_THRESHOLD_EN_MASK: u8 = 0b0000_0100;
const INT_SOURCE_PRESSURE_OOR_EN_MASK: u8 = 0b0000_1000;

// Bits of `REG_INT_STATUS`. The register is clear-on-read.
/// A measurement has been written to the data registers.
const INT_STATUS_DRDY_MASK: u8 = 0b0000_0001;
const INT_STATUS_FIFO_FULL_MASK: u8 = 0b0000_0010;
const INT_STATUS_FIFO_THRESHOLD_MASK: u8 = 0b0000_0100;
const INT_STATUS_PRESSURE_OOR_MASK: u8 = 0b0000_1000;
/// A power-on or software reset has completed.
const INT_STATUS_POR_MASK: u8 = 0b0001_0000;

// Bits of `REG_STATUS`.
const STATUS_NVM_RDY_MASK: u8 = 0b0000_0010;
const STATUS_NVM_ERR_MASK: u8 = 0b0000_0100;
const STATUS_NVM_CMD_ERR_MASK: u8 = 0b0000_1000;

// `osr_t_eff[2:0]` field of `REG_OSR_EFF`, occupying bits 2:0.
const OSR_EFF_OSR_T_MASK: u8 = 0b0000_0111;
// `osr_p_eff[2:0]` field of `REG_OSR_EFF`, occupying bits 5:3.
const OSR_EFF_OSR_P_MASK: u8 = 0b0011_1000;
const OSR_EFF_OSR_P_SHIFT: u8 = 3;
// `odr_is_valid` bit of `REG_OSR_EFF`, occupying bit 7. Cleared, the requested
// oversampling does not fit into the configured ODR period and the device
// reduced it to the effective values in the same register.
const OSR_EFF_ODR_IS_VALID_MASK: u8 = 0b1000_0000;

/// Execution time of a software reset, per the datasheet.
const SOFT_RESET_DELAY_US: u32 = 2000;
/// Settling time the device needs to reach standby after a mode change.
const STANDBY_DELAY_US: u32 = 2500;

/// How often [`Bmp581::measure`] polls `drdy_data_reg`.
const MEASUREMENT_POLL_INTERVAL_MS: u64 = 2;
/// How long [`Bmp581::measure`] waits for `drdy_data_reg` before giving up.
///
/// A forced measurement finishes in single-digit milliseconds even at the
/// highest oversampling, so this is a fault timeout, not a conversion time.
const MEASUREMENT_TIMEOUT_MS: u64 = 200;

/// Pressure LSBs per pascal: the register value is a 1/64 Pa fixed-point number.
const PRESSURE_LSB_PER_PASCAL: f32 = 64.0;
/// Temperature LSBs per degree Celsius: the register value is a 1/65536 °C
/// fixed-point number.
const TEMPERATURE_LSB_PER_CELSIUS: f32 = 65536.0;

/// Bit set in a 24-bit two's-complement value when it is negative.
const SIGN_BIT_24BIT: u32 = 0x0080_0000;
/// Bits added to sign-extend a negative 24-bit value to 32 bits.
const SIGN_EXTENSION_24BIT: u32 = 0xFF00_0000;

/// Oversampling applied to the temperature or pressure channel.
///
/// Each step doubles the number of averaged conversions, which lowers noise and
/// lengthens the measurement. It is independent of [`IirFilter`], which instead
/// smooths consecutive measurements together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Oversampling {
    X1,
    X2,
    X4,
    X8,
    X16,
    X32,
    X64,
    X128,
}

impl Oversampling {
    /// Register encoding of this setting.
    pub const fn raw(self) -> u8 {
        match self {
            Oversampling::X1 => 0,
            Oversampling::X2 => 1,
            Oversampling::X4 => 2,
            Oversampling::X8 => 3,
            Oversampling::X16 => 4,
            Oversampling::X32 => 5,
            Oversampling::X64 => 6,
            Oversampling::X128 => 7,
        }
    }

    /// Number of conversions averaged into one measurement.
    pub const fn factor(self) -> u16 {
        1 << self.raw()
    }

    /// Decode a register field. Every one of the eight encodings is valid.
    pub const fn from_raw(raw: u8) -> Self {
        match raw & 0x07 {
            0 => Oversampling::X1,
            1 => Oversampling::X2,
            2 => Oversampling::X4,
            3 => Oversampling::X8,
            4 => Oversampling::X16,
            5 => Oversampling::X32,
            6 => Oversampling::X64,
            _ => Oversampling::X128,
        }
    }
}

/// Coefficient of the on-chip IIR low-pass filter.
///
/// The filter averages the current measurement with the running filter state,
/// weighting the state by the coefficient. Higher coefficients suppress more
/// noise but take more measurements to follow a real change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IirFilter {
    /// Filter disabled; the data registers hold the unfiltered measurement.
    Bypass,
    Coefficient1,
    Coefficient3,
    Coefficient7,
    Coefficient15,
    Coefficient31,
    Coefficient63,
    Coefficient127,
}

impl IirFilter {
    /// Register encoding of this setting.
    pub const fn raw(self) -> u8 {
        match self {
            IirFilter::Bypass => 0,
            IirFilter::Coefficient1 => 1,
            IirFilter::Coefficient3 => 2,
            IirFilter::Coefficient7 => 3,
            IirFilter::Coefficient15 => 4,
            IirFilter::Coefficient31 => 5,
            IirFilter::Coefficient63 => 6,
            IirFilter::Coefficient127 => 7,
        }
    }

    /// Decode a register field. Every one of the eight encodings is valid.
    pub const fn from_raw(raw: u8) -> Self {
        match raw & 0x07 {
            0 => IirFilter::Bypass,
            1 => IirFilter::Coefficient1,
            2 => IirFilter::Coefficient3,
            3 => IirFilter::Coefficient7,
            4 => IirFilter::Coefficient15,
            5 => IirFilter::Coefficient31,
            6 => IirFilter::Coefficient63,
            _ => IirFilter::Coefficient127,
        }
    }
}

/// Power mode held in `pwr_mode[1:0]` of `REG_ODR_CONFIG`.
///
/// Deep standby is not represented: it is entered by clearing `deep_dis` rather
/// than through this field, and this driver always keeps `deep_dis` set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerMode {
    /// Idle; the configuration registers are writable only in this mode.
    Standby,
    /// Periodic measurements at the configured ODR.
    Normal,
    /// One measurement, after which the device returns to standby by itself.
    Forced,
    /// Back-to-back measurements, ignoring the ODR.
    Continuous,
}

impl PowerMode {
    /// Register encoding of this mode.
    pub const fn raw(self) -> u8 {
        match self {
            PowerMode::Standby => 0,
            PowerMode::Normal => 1,
            PowerMode::Forced => 2,
            PowerMode::Continuous => 3,
        }
    }

    /// Decode a register field. Every one of the four encodings is valid.
    pub const fn from_raw(raw: u8) -> Self {
        match raw & ODR_CONFIG_POWER_MODE_MASK {
            0 => PowerMode::Standby,
            1 => PowerMode::Normal,
            2 => PowerMode::Forced,
            _ => PowerMode::Continuous,
        }
    }
}

/// Settings applied by [`Bmp581::init`] and [`Bmp581::configure`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Configuration {
    /// Oversampling of the temperature channel.
    pub temperature_oversampling: Oversampling,
    /// Oversampling of the pressure channel.
    pub pressure_oversampling: Oversampling,
    /// Whether the pressure channel is measured at all. Temperature is always
    /// measured because the pressure compensation needs it.
    pub pressure_enabled: bool,
    /// IIR filter applied to the temperature channel.
    pub temperature_filter: IirFilter,
    /// IIR filter applied to the pressure channel.
    pub pressure_filter: IirFilter,
    /// Whether to flush the IIR filter before each forced measurement, so that
    /// a one-shot reading does not depend on how long ago the previous one was.
    pub flush_filter_on_forced: bool,
}

impl Default for Configuration {
    /// Pressure-oriented one-shot settings: heavy oversampling on pressure,
    /// light oversampling on temperature, and no IIR filtering because a forced
    /// measurement has no useful filter history.
    fn default() -> Self {
        Self {
            temperature_oversampling: Oversampling::X1,
            pressure_oversampling: Oversampling::X16,
            pressure_enabled: true,
            temperature_filter: IirFilter::Bypass,
            pressure_filter: IirFilter::Bypass,
            flush_filter_on_forced: true,
        }
    }
}

/// Contents of the identification registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identification {
    /// `REG_CHIP_ID`.
    pub chip_id: u8,
    /// `REG_REV_ID`, the mask revision.
    pub rev_id: u8,
}

/// Oversampling the device actually applied, read back from `REG_OSR_EFF`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveOversampling {
    pub temperature: Oversampling,
    pub pressure: Oversampling,
    /// Whether the configured ODR period is long enough for the requested
    /// oversampling. When false the device lowered the oversampling to the
    /// values above. Only meaningful in the periodic modes.
    pub odr_is_valid: bool,
}

/// One complete forced-mode result, straight out of the data registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measurement {
    /// Sign-extended 24-bit temperature in 1/65536 °C.
    pub temperature_raw: i32,
    /// 24-bit pressure in 1/64 Pa. Stale if the pressure channel is disabled.
    pub pressure_raw: u32,
}

impl Measurement {
    /// Temperature in degrees Celsius: `raw / 2^16`.
    pub fn temperature_celsius(&self) -> f32 {
        self.temperature_raw as f32 / TEMPERATURE_LSB_PER_CELSIUS
    }

    /// Pressure in pascals: `raw / 2^6`.
    pub fn pressure_pascals(&self) -> f32 {
        self.pressure_raw as f32 / PRESSURE_LSB_PER_PASCAL
    }

    /// Pressure in hectopascals, the unit weather reports use.
    pub fn pressure_hectopascals(&self) -> f32 {
        self.pressure_pascals() / 100.0
    }
}

/// Errors reported by the driver.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Error {
    /// The I2C transfer itself failed (no acknowledge, timeout, ...).
    Bus(I2cError),
    /// `REG_CHIP_ID` did not hold a BMP5-family part number.
    InvalidChipId(u8),
    /// The reset-complete flag was not set after a software reset.
    ResetFailed,
    /// The NVM trim data was not readable, so the on-chip compensation cannot
    /// be trusted. Carries `REG_STATUS`.
    NvmNotReady(u8),
    /// `drdy_data_reg` was not set within [`MEASUREMENT_TIMEOUT_MS`].
    MeasurementTimeout,
}

impl From<I2cError> for Error {
    fn from(error: I2cError) -> Self {
        Error::Bus(error)
    }
}

pub struct Bmp581<'a, 'd, T> {
    i2c: &'a mut I2C<'d, T, Blocking>,
    address: u8,
}

impl<'a, 'd, T: Instance> Bmp581<'a, 'd, T> {
    /// Bind the driver to a bus, using the `SDO`-high address.
    pub fn new(i2c: &'a mut I2C<'d, T, Blocking>) -> Self {
        Self::with_address(i2c, ALTERNATE_ADDRESS)
    }

    /// Bind the driver to a bus using an explicit 7-bit address.
    pub fn with_address(i2c: &'a mut I2C<'d, T, Blocking>, address: u8) -> Self {
        Self { i2c, address }
    }

    /// Read one or more consecutive registers starting at `register`.
    ///
    /// The register pointer auto-increments on reads, so one transfer can pull
    /// a whole block such as the six data bytes.
    async fn read_registers(&mut self, register: u8, raw: &mut [u8]) -> Result<(), Error> {
        self.i2c.write_read(self.address, &[register], raw)?;
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

    /// Read the identification registers; useful as a presence check.
    pub async fn identification(&mut self) -> Result<Identification, Error> {
        let mut raw = [0u8; 2];
        self.read_registers(REG_CHIP_ID, &mut raw).await?;
        Ok(Identification {
            chip_id: raw[0],
            rev_id: raw[1],
        })
    }

    /// Read and clear `REG_INT_STATUS`.
    ///
    /// The register is clear-on-read, so each flag is reported to exactly one
    /// caller. [`Bmp581::data_ready`] and [`Bmp581::measure`] consume it.
    pub async fn interrupt_status(&mut self) -> Result<u8, Error> {
        self.read_register(REG_INT_STATUS).await
    }

    /// Read `REG_STATUS`, which carries the NVM ready and error flags.
    pub async fn status(&mut self) -> Result<u8, Error> {
        self.read_register(REG_STATUS).await
    }

    /// Trigger a software reset and wait for it to complete.
    ///
    /// Afterwards every register holds its power-on default and the device is
    /// in standby. The reset-complete flag is checked through `REG_INT_STATUS`,
    /// whose read also clears it.
    pub async fn reset(&mut self) -> Result<(), Error> {
        self.write_register(REG_CMD, CMD_SOFT_RESET).await?;
        Timer::after_micros(SOFT_RESET_DELAY_US as u64).await;

        if self.interrupt_status().await? & INT_STATUS_POR_MASK == 0 {
            return Err(Error::ResetFailed);
        }
        Ok(())
    }

    /// Check that the factory trim data in NVM was loaded without error.
    ///
    /// The compensation the device applies to its raw ADC values comes from
    /// that data, so a failure here means the readings are meaningless even
    /// though the bus works.
    pub async fn check_nvm_ready(&mut self) -> Result<(), Error> {
        let status = self.status().await?;
        if status & STATUS_NVM_RDY_MASK == 0 || status & STATUS_NVM_ERR_MASK != 0 {
            return Err(Error::NvmNotReady(status));
        }
        Ok(())
    }

    /// Read back the currently selected power mode.
    pub async fn power_mode(&mut self) -> Result<PowerMode, Error> {
        let raw = self.read_register(REG_ODR_CONFIG).await?;
        Ok(PowerMode::from_raw(raw))
    }

    /// Select a power mode, keeping deep standby disabled.
    ///
    /// `deep_dis` is written together with the mode bits because a standby with
    /// a slow ODR, a bypassed IIR filter and a disabled FIFO turns into deep
    /// standby whenever `deep_dis` is clear, and deep standby drops the
    /// configuration this driver programmed.
    pub async fn set_power_mode(&mut self, mode: PowerMode) -> Result<(), Error> {
        self.update_register(
            REG_ODR_CONFIG,
            ODR_CONFIG_DEEP_DISABLE_MASK | ODR_CONFIG_POWER_MODE_MASK,
            ODR_CONFIG_DEEP_DISABLE_MASK | mode.raw(),
        )
        .await
    }

    /// Put the device into standby and wait for it to settle there.
    ///
    /// Standby is the only mode in which the oversampling and filter registers
    /// accept writes, and the datasheet requires a settling time before the
    /// next mode change.
    pub async fn standby(&mut self) -> Result<(), Error> {
        self.set_power_mode(PowerMode::Standby).await?;
        Timer::after_micros(STANDBY_DELAY_US as u64).await;
        Ok(())
    }

    /// Program oversampling, the IIR filter and the data-ready status flag.
    ///
    /// The device is driven to standby first, and left there.
    pub async fn configure(&mut self, configuration: &Configuration) -> Result<(), Error> {
        self.standby().await?;

        let mut osr_config = configuration.temperature_oversampling.raw()
            | (configuration.pressure_oversampling.raw() << OSR_CONFIG_OSR_P_SHIFT);
        if configuration.pressure_enabled {
            osr_config |= OSR_CONFIG_PRESS_EN_MASK;
        }
        self.update_register(
            REG_OSR_CONFIG,
            OSR_CONFIG_OSR_T_MASK | OSR_CONFIG_OSR_P_MASK | OSR_CONFIG_PRESS_EN_MASK,
            osr_config,
        )
        .await?;

        let dsp_iir = configuration.temperature_filter.raw()
            | (configuration.pressure_filter.raw() << DSP_IIR_SET_IIR_P_SHIFT);
        self.update_register(
            REG_DSP_IIR,
            DSP_IIR_SET_IIR_T_MASK | DSP_IIR_SET_IIR_P_MASK,
            dsp_iir,
        )
        .await?;

        // Route the filtered values into the data registers whenever a filter
        // is actually enabled; with the filter bypassed the two paths carry the
        // same value anyway.
        let mut dsp_config = 0u8;
        if configuration.temperature_filter != IirFilter::Bypass {
            dsp_config |= DSP_CONFIG_SHDW_SEL_IIR_T_MASK;
        }
        if configuration.pressure_filter != IirFilter::Bypass {
            dsp_config |= DSP_CONFIG_SHDW_SEL_IIR_P_MASK;
        }
        if configuration.flush_filter_on_forced {
            dsp_config |= DSP_CONFIG_IIR_FLUSH_FORCED_EN_MASK;
        }
        self.update_register(
            REG_DSP_CONFIG,
            DSP_CONFIG_SHDW_SEL_IIR_T_MASK
                | DSP_CONFIG_SHDW_SEL_IIR_P_MASK
                | DSP_CONFIG_IIR_FLUSH_FORCED_EN_MASK,
            dsp_config,
        )
        .await?;

        // Let finished measurements raise `drdy_data_reg` in `REG_INT_STATUS`.
        // The source register resets to zero, and with the data-ready source
        // off the status flag stays clear however long the caller polls it.
        // This only feeds the status register; driving the INT pin would take
        // `REG_INT_CONFIG` as well, and nothing here uses the pin.
        self.update_register(
            REG_INT_SOURCE,
            INT_SOURCE_DRDY_EN_MASK,
            INT_SOURCE_DRDY_EN_MASK,
        )
        .await
    }

    /// Reset the device, verify it, and apply `configuration`.
    ///
    /// The device is left in standby, ready for [`Bmp581::start_measurement`].
    pub async fn init(&mut self, configuration: &Configuration) -> Result<(), Error> {
        let identification = self.identification().await?;
        if identification.chip_id != CHIP_ID_PRIMARY && identification.chip_id != CHIP_ID_SECONDARY
        {
            return Err(Error::InvalidChipId(identification.chip_id));
        }

        self.reset().await?;
        self.check_nvm_ready().await?;
        self.configure(configuration).await
    }

    /// Read back the oversampling the device applied to the last measurement.
    pub async fn effective_oversampling(&mut self) -> Result<EffectiveOversampling, Error> {
        let raw = self.read_register(REG_OSR_EFF).await?;
        Ok(EffectiveOversampling {
            temperature: Oversampling::from_raw(raw & OSR_EFF_OSR_T_MASK),
            pressure: Oversampling::from_raw((raw & OSR_EFF_OSR_P_MASK) >> OSR_EFF_OSR_P_SHIFT),
            odr_is_valid: raw & OSR_EFF_ODR_IS_VALID_MASK != 0,
        })
    }

    /// Start one forced measurement.
    ///
    /// The device converts once and drops back to standby on its own, so the
    /// caller can release a shared bus until [`Bmp581::data_ready`] reports the
    /// result.
    pub async fn start_measurement(&mut self) -> Result<(), Error> {
        self.set_power_mode(PowerMode::Forced).await
    }

    /// Whether a measurement has been written to the data registers.
    ///
    /// This reads the clear-on-read `REG_INT_STATUS`, so a `true` is reported
    /// only once per measurement.
    pub async fn data_ready(&mut self) -> Result<bool, Error> {
        Ok(self.interrupt_status().await? & INT_STATUS_DRDY_MASK != 0)
    }

    /// Read the six data bytes in one transfer.
    ///
    /// Does not check `drdy_data_reg`, so a call made before the measurement
    /// finished returns the previous one.
    pub async fn read_measurement(&mut self) -> Result<Measurement, Error> {
        let mut raw = [0u8; 6];
        self.read_registers(REG_TEMP_DATA_XLSB, &mut raw).await?;

        let temperature = (raw[2] as u32) << 16 | (raw[1] as u32) << 8 | raw[0] as u32;
        let temperature_raw = if temperature & SIGN_BIT_24BIT != 0 {
            (temperature | SIGN_EXTENSION_24BIT) as i32
        } else {
            temperature as i32
        };

        Ok(Measurement {
            temperature_raw,
            pressure_raw: (raw[5] as u32) << 16 | (raw[4] as u32) << 8 | raw[3] as u32,
        })
    }

    /// Trigger a forced measurement, wait for it, and read it back.
    ///
    /// Convenience for callers that hold the bus for the whole cycle; a forced
    /// conversion is short enough that this is usually the right trade.
    pub async fn measure(&mut self) -> Result<Measurement, Error> {
        // Drop a stale data-ready flag so the poll below cannot see the
        // previous measurement's.
        self.interrupt_status().await?;
        self.start_measurement().await?;

        let mut waited_ms = 0;
        while waited_ms < MEASUREMENT_TIMEOUT_MS {
            Timer::after_millis(MEASUREMENT_POLL_INTERVAL_MS).await;
            waited_ms += MEASUREMENT_POLL_INTERVAL_MS;

            if self.data_ready().await? {
                return self.read_measurement().await;
            }
        }

        Err(Error::MeasurementTimeout)
    }
}
