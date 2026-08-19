//! Driver for the Texas Instruments OPT4048 tristimulus XYZ colour sensor.
//!
//! The device has four photodiode channels behind precision optical filters:
//! channels 0, 1 and 2 approximate the CIE 1931 X, Y and Z colour matching
//! functions, and channel 3 is an unfiltered wide-band ("clear") channel. One
//! measurement converts all four channels in sequence, so a full result takes
//! four conversion periods, not one.
//!
//! Each channel is reported as a floating-point number rather than a plain
//! integer: a 20-bit `RESULT` mantissa plus a 4-bit `EXPONENT`, where the
//! linear ADC code is `mantissa << exponent`. The exponent is what the
//! full-scale range setting selects, so a fixed [`Range`] pins the exponent
//! and [`Range::Auto`] lets the device pick one per measurement. Alongside
//! those, every channel carries a 4-bit sample counter and a 4-bit CRC that
//! the device computes over its own output.
//!
//! Measurement flow, mirroring the other drivers here:
//!
//! 1. `init` checks the device ID and programs range and conversion time.
//!    The device is left powered down.
//! 2. `start_measurement` writes the control register with `OPERATING_MODE`
//!    set to one-shot, which converts all four channels once and then returns
//!    the device to power-down on its own.
//! 3. After [`Configuration::measurement_time_us`] has passed,
//!    `read_measurement` confirms `CONVERSION_READY` in `REG_FLAGS` and pulls
//!    all sixteen data bytes in one transfer.
//!
//! Splitting the last two steps lets the caller release the shared bus while
//! the device converts, which at the longer conversion times is by far the
//! bulk of a cycle.
//!
//! Unlike the other sensors on this bus, every OPT4048 register is 16 bits
//! wide and **big-endian**: the most significant byte comes first. There is no
//! register bank switching.
//!
//! Photometric and colorimetric conversions ([`Measurement::lux`],
//! [`Measurement::tristimulus`], [`Measurement::chromaticity`],
//! [`Measurement::correlated_color_temperature_kelvin`]) are applied on the
//! host from the datasheet's coefficients; the device itself only reports ADC
//! codes.

#![allow(dead_code)]

use embassy_time::Timer;
use esp_hal::{
    i2c::{Error as I2cError, Instance, I2C},
    Blocking,
};

/// 7-bit address used when `ADDR` is tied to GND. This is the factory default.
pub const DEFAULT_ADDRESS: u8 = 0x44;
/// 7-bit address used when `ADDR` is tied to VDD.
pub const ADDRESS_VDD: u8 = 0x45;
/// 7-bit address used when `ADDR` is tied to SDA.
pub const ADDRESS_SDA: u8 = 0x46;
/// 7-bit address used when `ADDR` is tied to SCL.
pub const ADDRESS_SCL: u8 = 0x47;

// Per-channel result registers. Each channel occupies two consecutive 16-bit
// registers, and the four channels are contiguous from 0x00, so one
// auto-incrementing read covers the whole block.
const REG_CH0_EXPONENT_RESULT_MSB: u8 = 0x00;
const REG_CH0_RESULT_LSB_COUNTER_CRC: u8 = 0x01;
const REG_CH1_EXPONENT_RESULT_MSB: u8 = 0x02;
const REG_CH1_RESULT_LSB_COUNTER_CRC: u8 = 0x03;
const REG_CH2_EXPONENT_RESULT_MSB: u8 = 0x04;
const REG_CH2_RESULT_LSB_COUNTER_CRC: u8 = 0x05;
const REG_CH3_EXPONENT_RESULT_MSB: u8 = 0x06;
const REG_CH3_RESULT_LSB_COUNTER_CRC: u8 = 0x07;

const REG_THRESHOLD_LOW: u8 = 0x08;
const REG_THRESHOLD_HIGH: u8 = 0x09;
/// Range, conversion time, operating mode, interrupt latch/polarity, fault count.
const REG_CONTROL: u8 = 0x0A;
/// Interrupt direction and mechanism, threshold channel select, I2C burst.
const REG_INT_CONTROL: u8 = 0x0B;
const REG_FLAGS: u8 = 0x0C;
const REG_DEVICE_ID: u8 = 0x11;

/// Value `REG_DEVICE_ID` holds on an OPT4048.
///
/// The register splits the identifier into `DIDL` (bits 11:0) and `DIDH`
/// (bits 13:12); on this part `DIDH` is zero, so the whole register reads as
/// this single constant and is compared as one word.
const EXPECTED_DEVICE_ID: u16 = 0x0821;

// Fields of `REG_CONTROL`. Reset value is 0x3208, which decodes as
// range = auto, conversion time = 100 ms, latch = 1, mode = power-down.
// `FAULT_COUNT`, occupying bits 1:0.
const CONTROL_FAULT_COUNT_MASK: u16 = 0b0000_0000_0000_0011;
const CONTROL_FAULT_COUNT_SHIFT: u16 = 0;
// `INT_POL`, occupying bit 2. Set, the INT pin is active high.
const CONTROL_INT_POL_MASK: u16 = 0b0000_0000_0000_0100;
// `LATCH`, occupying bit 3. Set, the flags in `REG_FLAGS` latch until the
// register is read; clear, they track the comparator transparently.
const CONTROL_LATCH_MASK: u16 = 0b0000_0000_0000_1000;
// `OPERATING_MODE`, occupying bits 5:4.
const CONTROL_OPERATING_MODE_MASK: u16 = 0b0000_0000_0011_0000;
const CONTROL_OPERATING_MODE_SHIFT: u16 = 4;
// `CONVERSION_TIME`, occupying bits 9:6.
const CONTROL_CONVERSION_TIME_MASK: u16 = 0b0000_0011_1100_0000;
const CONTROL_CONVERSION_TIME_SHIFT: u16 = 6;
// `RANGE`, occupying bits 13:10.
const CONTROL_RANGE_MASK: u16 = 0b0011_1100_0000_0000;
const CONTROL_RANGE_SHIFT: u16 = 10;
// `QWAKE`, occupying bit 15. Set, part of the analog front end stays biased
// during power-down so the next one-shot starts faster, at a higher idle
// current.
const CONTROL_QWAKE_MASK: u16 = 0b1000_0000_0000_0000;

// Fields of `REG_INT_CONTROL`.
// `I2C_BURST`, occupying bit 0.
const INT_CONTROL_I2C_BURST_MASK: u16 = 0b0000_0000_0000_0001;
// `INT_CFG`, occupying bits 3:2.
const INT_CONTROL_INT_CFG_MASK: u16 = 0b0000_0000_0000_1100;
const INT_CONTROL_INT_CFG_SHIFT: u16 = 2;
// `INT_DIR`, occupying bit 4. Clear, the INT pin is an output; set, it is an
// input that triggers conversions.
const INT_CONTROL_INT_DIR_MASK: u16 = 0b0000_0000_0001_0000;
// `THRESHOLD_CH_SEL`, occupying bits 6:5.
const INT_CONTROL_THRESHOLD_CH_SEL_MASK: u16 = 0b0000_0000_0110_0000;
const INT_CONTROL_THRESHOLD_CH_SEL_SHIFT: u16 = 5;

// Bits of `REG_FLAGS`.
/// Measured value fell below the low threshold.
const FLAGS_FLAG_LOW_MASK: u16 = 0b0000_0000_0000_0001;
/// Measured value rose above the high threshold.
const FLAGS_FLAG_HIGH_MASK: u16 = 0b0000_0000_0000_0010;
/// A full four-channel conversion has been written to the result registers.
const FLAGS_CONVERSION_READY_MASK: u16 = 0b0000_0000_0000_0100;
/// The input light exceeded the selected full-scale range; counts are clipped.
const FLAGS_OVERLOAD_MASK: u16 = 0b0000_0000_0000_1000;

// Layout of the first result register of a channel.
// `EXPONENT`, occupying bits 15:12.
const RESULT_EXPONENT_SHIFT: u16 = 12;
// `RESULT_MSB`, occupying bits 11:0.
const RESULT_MSB_MASK: u16 = 0x0FFF;
// Layout of the second result register of a channel.
// `RESULT_LSB`, occupying bits 15:8.
const RESULT_LSB_SHIFT: u16 = 8;
// `COUNTER`, occupying bits 7:4.
const RESULT_COUNTER_SHIFT: u16 = 4;
const RESULT_COUNTER_MASK: u16 = 0x000F;
// `CRC`, occupying bits 3:0.
const RESULT_CRC_MASK: u16 = 0x000F;

/// Number of channels one measurement produces.
pub const CHANNEL_COUNT: usize = 4;

/// Bytes the four channels occupy in the contiguous result register block.
const RESULT_BLOCK_LEN: usize = 4 * CHANNEL_COUNT;

/// Number of conversions one measurement performs, one per channel.
const CONVERSIONS_PER_MEASUREMENT: u32 = CHANNEL_COUNT as u32;

/// Lux per ADC count on channel 1, from the datasheet's conversion table.
///
/// Channel 1 tracks the CIE luminosity function, so illuminance needs only
/// this single scale factor.
const LUX_PER_CH1_COUNT: f32 = 2.15e-3;

/// Time between two `CONVERSION_READY` polls while a measurement is running.
const DATA_READY_POLL_INTERVAL_MS: u64 = 5;

/// Longest time [`Opt4048::read_measurement`] waits for `CONVERSION_READY`.
///
/// The slowest possible measurement is four 800 ms conversions, so this is a
/// fault timeout with margin, not a conversion time.
const DATA_READY_TIMEOUT_MS: u64 = 4000;

/// Settling time after a general-call reset before the device responds again.
const RESET_DELAY_MS: u64 = 2;

/// Coefficients converting the channel 0/1/2 ADC codes into CIE XYZ
/// tristimulus values, from the datasheet's application section.
///
/// Indexed `[channel][output]`, so `CIE_MATRIX[c][0]` is the weight channel
/// `c` contributes to X.
const CIE_MATRIX: [[f32; 3]; 3] = [
    [2.34892992e-4, -1.89652390e-5, 1.20811684e-5],
    [4.07467441e-5, 1.98958202e-4, -1.58848115e-5],
    [9.28619404e-5, -1.69739553e-5, 6.74021520e-4],
];

/// Full-scale light level (`RANGE` field of `REG_CONTROL`).
///
/// Each step doubles the range and costs one bit of resolution at the bottom
/// of the scale. [`Range::Auto`] is the reset default and lets the device
/// choose per measurement, which is the right choice unless a fixed exponent
/// is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    Lux2_2k = 0,
    Lux4_5k = 1,
    Lux9k = 2,
    Lux18k = 3,
    Lux36k = 4,
    Lux72k = 5,
    Lux144k = 6,
    /// Automatic full-scale range selection; the power-on default.
    Auto = 12,
}

impl Range {
    /// Register encoding of this setting.
    pub const fn raw(self) -> u8 {
        self as u8
    }

    /// Decode the raw `RANGE` field.
    pub const fn from_raw(raw: u8) -> Option<Self> {
        Some(match raw {
            0 => Range::Lux2_2k,
            1 => Range::Lux4_5k,
            2 => Range::Lux9k,
            3 => Range::Lux18k,
            4 => Range::Lux36k,
            5 => Range::Lux72k,
            6 => Range::Lux144k,
            12 => Range::Auto,
            _ => return None,
        })
    }

    /// Nominal full-scale illuminance in lux, or `None` for [`Range::Auto`].
    pub const fn full_scale_lux(self) -> Option<u32> {
        Some(match self {
            Range::Lux2_2k => 2_200,
            Range::Lux4_5k => 4_500,
            Range::Lux9k => 9_000,
            Range::Lux18k => 18_000,
            Range::Lux36k => 36_000,
            Range::Lux72k => 72_000,
            Range::Lux144k => 144_000,
            Range::Auto => return None,
        })
    }
}

/// Per-channel conversion time (`CONVERSION_TIME` field of `REG_CONTROL`).
///
/// A measurement converts all four channels in turn, so the time a full
/// result takes is four times the value selected here; see
/// [`Configuration::measurement_time_us`]. Longer conversions integrate more
/// charge and so lower the noise floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionTime {
    Us600 = 0,
    Ms1 = 1,
    Ms1_8 = 2,
    Ms3_4 = 3,
    Ms6_5 = 4,
    Ms12_7 = 5,
    Ms25 = 6,
    Ms50 = 7,
    /// Power-on default.
    Ms100 = 8,
    Ms200 = 9,
    Ms400 = 10,
    Ms800 = 11,
}

impl ConversionTime {
    /// Register encoding of this setting.
    pub const fn raw(self) -> u8 {
        self as u8
    }

    /// Decode the raw `CONVERSION_TIME` field.
    pub const fn from_raw(raw: u8) -> Option<Self> {
        Some(match raw {
            0 => ConversionTime::Us600,
            1 => ConversionTime::Ms1,
            2 => ConversionTime::Ms1_8,
            3 => ConversionTime::Ms3_4,
            4 => ConversionTime::Ms6_5,
            5 => ConversionTime::Ms12_7,
            6 => ConversionTime::Ms25,
            7 => ConversionTime::Ms50,
            8 => ConversionTime::Ms100,
            9 => ConversionTime::Ms200,
            10 => ConversionTime::Ms400,
            11 => ConversionTime::Ms800,
            _ => return None,
        })
    }

    /// Time one single-channel conversion takes, in microseconds.
    pub const fn microseconds(self) -> u32 {
        match self {
            ConversionTime::Us600 => 600,
            ConversionTime::Ms1 => 1_000,
            ConversionTime::Ms1_8 => 1_800,
            ConversionTime::Ms3_4 => 3_400,
            ConversionTime::Ms6_5 => 6_500,
            ConversionTime::Ms12_7 => 12_700,
            ConversionTime::Ms25 => 25_000,
            ConversionTime::Ms50 => 50_000,
            ConversionTime::Ms100 => 100_000,
            ConversionTime::Ms200 => 200_000,
            ConversionTime::Ms400 => 400_000,
            ConversionTime::Ms800 => 800_000,
        }
    }
}

/// Device operating mode (`OPERATING_MODE` field of `REG_CONTROL`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatingMode {
    /// Idle; the power-on default. A one-shot measurement returns here on its
    /// own once it has finished.
    PowerDown = 0,
    /// One measurement, forced to run through the auto-range logic first.
    /// Costs an extra conversion but guarantees a sensible exponent when the
    /// light level has changed a lot since the last reading.
    ForcedAutoRangeOneShot = 1,
    /// One measurement of all four channels, then back to power-down.
    OneShot = 2,
    /// Measure continuously, restarting as soon as a measurement completes.
    Continuous = 3,
}

impl OperatingMode {
    /// Register encoding of this setting.
    pub const fn raw(self) -> u8 {
        self as u8
    }

    /// Decode the raw `OPERATING_MODE` field.
    pub const fn from_raw(raw: u8) -> Self {
        match raw & 0b11 {
            0 => OperatingMode::PowerDown,
            1 => OperatingMode::ForcedAutoRangeOneShot,
            2 => OperatingMode::OneShot,
            _ => OperatingMode::Continuous,
        }
    }
}

/// One of the four channels a measurement produces, in read-out order.
///
/// The discriminants are the indices into [`Measurement::channels`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Channel 0, approximating the CIE 1931 X colour matching function.
    X = 0,
    /// Channel 1, approximating the CIE 1931 Y colour matching function.
    /// This is the channel illuminance is derived from.
    Y = 1,
    /// Channel 2, approximating the CIE 1931 Z colour matching function.
    Z = 2,
    /// Channel 3, an unfiltered wide-band channel with no CIE equivalent.
    Wideband = 3,
}

/// Contents of `REG_FLAGS`.
///
/// With `LATCH` set (the reset default, and what [`Configuration`] programs)
/// these latch until the register is read, so each event is reported once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flags {
    /// The threshold channel fell below the low threshold.
    pub low: bool,
    /// The threshold channel rose above the high threshold.
    pub high: bool,
    /// A complete four-channel measurement is in the result registers.
    pub conversion_ready: bool,
    /// The input exceeded the selected full-scale range; counts are clipped.
    pub overload: bool,
}

impl Flags {
    const fn from_raw(raw: u16) -> Self {
        Self {
            low: raw & FLAGS_FLAG_LOW_MASK != 0,
            high: raw & FLAGS_FLAG_HIGH_MASK != 0,
            conversion_ready: raw & FLAGS_CONVERSION_READY_MASK != 0,
            overload: raw & FLAGS_OVERLOAD_MASK != 0,
        }
    }
}

/// The two result registers of a single channel, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelResult {
    /// `EXPONENT`, the binary scale the mantissa has to be shifted by.
    pub exponent: u8,
    /// `RESULT`, the 20-bit mantissa.
    pub mantissa: u32,
    /// `COUNTER`, a 4-bit sample counter the device increments once per
    /// measurement and wraps at 16. Comparing it between reads distinguishes
    /// a fresh measurement from a re-read of the previous one.
    pub counter: u8,
    /// `CRC`, the 4-bit checksum the device computed over this channel's
    /// exponent, mantissa and counter.
    ///
    /// Reported as read; this driver does not recompute or verify it.
    pub crc: u8,
}

impl ChannelResult {
    /// Linear ADC code of this channel: `mantissa << exponent`.
    pub const fn adc_code(&self) -> u32 {
        self.mantissa << self.exponent
    }
}

/// CIE 1931 XYZ tristimulus values derived from a measurement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tristimulus {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// CIE 1931 chromaticity coordinates, the tristimulus values normalised so
/// that they describe colour independently of brightness.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chromaticity {
    pub x: f32,
    pub y: f32,
}

/// One complete four-channel measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measurement {
    /// Decoded per-channel results, indexed by [`Channel`].
    pub channels: [ChannelResult; CHANNEL_COUNT],
    /// The input exceeded the selected full-scale range on at least one
    /// channel, so the counts are clipped.
    pub overload: bool,
}

impl Measurement {
    /// Decoded result of one channel.
    pub const fn channel(&self, channel: Channel) -> ChannelResult {
        self.channels[channel as usize]
    }

    /// Linear ADC code of one channel.
    pub const fn adc_code(&self, channel: Channel) -> u32 {
        self.channels[channel as usize].adc_code()
    }

    /// Illuminance in lux, derived from channel 1 alone.
    pub fn lux(&self) -> f32 {
        self.adc_code(Channel::Y) as f32 * LUX_PER_CH1_COUNT
    }

    /// CIE 1931 XYZ tristimulus values, from channels 0, 1 and 2.
    ///
    /// These are unnormalised and scale with brightness; use
    /// [`Measurement::chromaticity`] for the brightness-independent colour.
    pub fn tristimulus(&self) -> Tristimulus {
        let counts = [
            self.adc_code(Channel::X) as f32,
            self.adc_code(Channel::Y) as f32,
            self.adc_code(Channel::Z) as f32,
        ];

        let mut xyz = [0.0f32; 3];
        for (count, weights) in counts.iter().zip(CIE_MATRIX.iter()) {
            for (output, weight) in xyz.iter_mut().zip(weights.iter()) {
                *output += count * weight;
            }
        }

        Tristimulus {
            x: xyz[0],
            y: xyz[1],
            z: xyz[2],
        }
    }

    /// CIE 1931 chromaticity coordinates.
    ///
    /// Returns `None` in complete darkness, where the tristimulus values sum
    /// to zero and the coordinates are undefined.
    pub fn chromaticity(&self) -> Option<Chromaticity> {
        let Tristimulus { x, y, z } = self.tristimulus();
        let sum = x + y + z;
        if sum <= 0.0 {
            return None;
        }
        Some(Chromaticity {
            x: x / sum,
            y: y / sum,
        })
    }

    /// Correlated colour temperature in kelvin.
    ///
    /// Uses McCamy's cubic approximation about the epicentre (0.3320, 0.1858),
    /// which is what the datasheet specifies. It is only meaningful for light
    /// that is reasonably close to the Planckian locus; saturated colours
    /// still produce a number, but not a physically useful one.
    ///
    /// Returns `None` when the chromaticity is undefined, or when the light
    /// sits on the `y = 0.1858` line where the approximation divides by zero.
    pub fn correlated_color_temperature_kelvin(&self) -> Option<f32> {
        let chromaticity = self.chromaticity()?;
        let denominator = 0.1858 - chromaticity.y;
        if denominator == 0.0 {
            return None;
        }

        let n = (chromaticity.x - 0.3320) / denominator;
        Some(437.0 * n * n * n + 3601.0 * n * n + 6861.0 * n + 5517.0)
    }
}

/// Settings applied by [`Opt4048::init`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Configuration {
    /// Full-scale light level (`RANGE`).
    pub range: Range,
    /// Per-channel conversion time (`CONVERSION_TIME`).
    pub conversion_time: ConversionTime,
    /// Latch the flags in `REG_FLAGS` until the register is read (`LATCH`).
    ///
    /// Keep this set when polling `CONVERSION_READY`, so a finished
    /// measurement cannot be missed between two polls.
    pub latch_flags: bool,
}

impl Default for Configuration {
    /// Auto-ranging with 100 ms conversions, matching the device's own reset
    /// values: a 400 ms measurement that covers the full dynamic range.
    fn default() -> Self {
        Self {
            range: Range::Auto,
            conversion_time: ConversionTime::Ms100,
            latch_flags: true,
        }
    }
}

impl Configuration {
    /// Time one full four-channel measurement takes, in microseconds.
    ///
    /// The device converts the channels one after another, so this is four
    /// times the per-channel [`ConversionTime`].
    pub const fn measurement_time_us(&self) -> u32 {
        self.conversion_time.microseconds() * CONVERSIONS_PER_MEASUREMENT
    }

    /// Build the `REG_CONTROL` word for this configuration in `mode`.
    const fn control_word(&self, mode: OperatingMode) -> u16 {
        let mut word = ((self.range.raw() as u16) << CONTROL_RANGE_SHIFT)
            | ((self.conversion_time.raw() as u16) << CONTROL_CONVERSION_TIME_SHIFT)
            | ((mode.raw() as u16) << CONTROL_OPERATING_MODE_SHIFT);
        if self.latch_flags {
            word |= CONTROL_LATCH_MASK;
        }
        word
    }
}

/// Errors reported by the driver.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Error {
    /// The I2C transfer itself failed (no acknowledge, timeout, ...).
    Bus(I2cError),
    /// `REG_DEVICE_ID` did not hold the OPT4048 identifier.
    InvalidDeviceId(u16),
    /// `CONVERSION_READY` was not set within [`DATA_READY_TIMEOUT_MS`].
    MeasurementTimeout,
}

impl From<I2cError> for Error {
    fn from(error: I2cError) -> Self {
        Error::Bus(error)
    }
}

pub struct Opt4048<'a, 'd, T> {
    i2c: &'a mut I2C<'d, T, Blocking>,
    address: u8,
    /// The configuration [`Opt4048::init`] programmed.
    ///
    /// `REG_CONTROL` packs the operating mode into the same word as range and
    /// conversion time, so triggering a measurement means rewriting all three.
    /// Keeping the settings here lets `start_measurement` do that with a
    /// single write instead of a read-modify-write.
    configuration: Configuration,
}

impl<'a, 'd, T: Instance> Opt4048<'a, 'd, T> {
    /// Bind the driver to a bus, using the factory-default address.
    pub fn new(i2c: &'a mut I2C<'d, T, Blocking>) -> Self {
        Self::with_address(i2c, DEFAULT_ADDRESS)
    }

    /// Bind the driver to a bus using an explicit 7-bit address.
    pub fn with_address(i2c: &'a mut I2C<'d, T, Blocking>, address: u8) -> Self {
        Self {
            i2c,
            address,
            configuration: Configuration::default(),
        }
    }

    /// The configuration this driver last programmed.
    pub const fn configuration(&self) -> Configuration {
        self.configuration
    }

    /// Read a block of consecutive 16-bit registers starting at `register`.
    ///
    /// The register pointer auto-increments on reads, so one transfer can
    /// pull the whole result block. `raw` must be a whole number of registers
    /// long; each register arrives most significant byte first.
    async fn read_registers(&mut self, register: u8, raw: &mut [u8]) -> Result<(), Error> {
        self.i2c.write_read(self.address, &[register], raw)?;
        Ok(())
    }

    /// Read a single 16-bit register.
    async fn read_register(&mut self, register: u8) -> Result<u16, Error> {
        let mut raw = [0u8; 2];
        self.read_registers(register, &mut raw).await?;
        Ok(u16::from_be_bytes(raw))
    }

    /// Write a single 16-bit register.
    async fn write_register(&mut self, register: u8, value: u16) -> Result<(), Error> {
        let bytes = value.to_be_bytes();
        self.i2c
            .write(self.address, &[register, bytes[0], bytes[1]])?;
        Ok(())
    }

    /// Read a register, replace the bits selected by `mask`, and write it back.
    ///
    /// Used wherever a register mixes fields owned by different settings, so
    /// that changing one does not clear the others.
    async fn update_register(&mut self, register: u8, mask: u16, value: u16) -> Result<(), Error> {
        let current = self.read_register(register).await?;
        let updated = (current & !mask) | (value & mask);
        self.write_register(register, updated).await
    }

    /// Read the device identification register; useful as a presence check.
    pub async fn device_id(&mut self) -> Result<u16, Error> {
        self.read_register(REG_DEVICE_ID).await
    }

    /// Read and clear `REG_FLAGS`.
    ///
    /// With `LATCH` set the register is clear-on-read, so each flag is
    /// reported to exactly one caller. [`Opt4048::read_measurement`] consumes
    /// it while waiting for `CONVERSION_READY`.
    pub async fn flags(&mut self) -> Result<Flags, Error> {
        Ok(Flags::from_raw(self.read_register(REG_FLAGS).await?))
    }

    /// Whether a complete measurement is sitting in the result registers.
    ///
    /// This consumes the latched `CONVERSION_READY` flag, so a `true` is
    /// reported only once per measurement.
    pub async fn data_ready(&mut self) -> Result<bool, Error> {
        Ok(self.flags().await?.conversion_ready)
    }

    /// Reset every device on the bus with the I2C general-call reset command.
    ///
    /// The OPT4048 has no reset bit of its own. General call addresses every
    /// device on the bus at once, so this also resets any other device that
    /// implements the command; the sensors sharing this bus are reset by their
    /// own drivers during start-up, so it is only safe before they are
    /// configured.
    pub async fn general_call_reset(&mut self) -> Result<(), Error> {
        // General call address 0x00, reset command 0x06.
        self.i2c.write(0x00, &[0x06])?;
        Timer::after_millis(RESET_DELAY_MS).await;
        Ok(())
    }

    /// Program range, conversion time and the flag latch, and select `mode`.
    ///
    /// All four fields share `REG_CONTROL`, so they are written as one word.
    pub async fn configure(
        &mut self,
        configuration: &Configuration,
        mode: OperatingMode,
    ) -> Result<(), Error> {
        self.configuration = *configuration;
        self.write_register(REG_CONTROL, configuration.control_word(mode))
            .await
    }

    /// Select an operating mode, keeping the programmed settings.
    pub async fn set_operating_mode(&mut self, mode: OperatingMode) -> Result<(), Error> {
        let configuration = self.configuration;
        self.write_register(REG_CONTROL, configuration.control_word(mode))
            .await
    }

    /// Read back the currently selected operating mode.
    pub async fn operating_mode(&mut self) -> Result<OperatingMode, Error> {
        let raw = self.read_register(REG_CONTROL).await?;
        Ok(OperatingMode::from_raw(
            ((raw & CONTROL_OPERATING_MODE_MASK) >> CONTROL_OPERATING_MODE_SHIFT) as u8,
        ))
    }

    /// Choose which channel the high and low thresholds compare against.
    pub async fn set_threshold_channel(&mut self, channel: Channel) -> Result<(), Error> {
        self.update_register(
            REG_INT_CONTROL,
            INT_CONTROL_THRESHOLD_CH_SEL_MASK,
            (channel as u16) << INT_CONTROL_THRESHOLD_CH_SEL_SHIFT,
        )
        .await
    }

    /// Verify the device and apply `configuration`.
    ///
    /// The device ID is checked first, so a wiring or address fault is
    /// reported before anything is written. The device is left powered down
    /// and ready for [`Opt4048::start_measurement`].
    ///
    /// The settings written here stay in the device's registers until it is
    /// reset or loses power, so they are not rewritten per measurement.
    pub async fn init(&mut self, configuration: &Configuration) -> Result<(), Error> {
        let device_id = self.device_id().await?;
        if device_id != EXPECTED_DEVICE_ID {
            return Err(Error::InvalidDeviceId(device_id));
        }

        self.configure(configuration, OperatingMode::PowerDown)
            .await?;

        // Drop any flag left latched from before, so the first
        // `read_measurement` cannot mistake it for a finished conversion.
        let _ = self.flags().await?;
        Ok(())
    }

    /// Start one four-channel measurement.
    ///
    /// Writing `OPERATING_MODE` triggers the conversion, and the device drops
    /// back to power-down on its own once it has finished, so the caller can
    /// release a shared bus for the whole of
    /// [`Configuration::measurement_time_us`]. This costs a single register
    /// write and returns immediately.
    pub async fn start_measurement(&mut self) -> Result<(), Error> {
        self.set_operating_mode(OperatingMode::OneShot).await
    }

    /// Start one measurement, running the auto-range logic first.
    ///
    /// Costs an extra conversion period over [`Opt4048::start_measurement`]
    /// but picks a fresh exponent, which matters when the light level may have
    /// changed by orders of magnitude since the last reading.
    pub async fn start_auto_range_measurement(&mut self) -> Result<(), Error> {
        self.set_operating_mode(OperatingMode::ForcedAutoRangeOneShot)
            .await
    }

    /// Poll `REG_FLAGS` until `CONVERSION_READY` reports a finished
    /// measurement, and return the flags that came with it.
    async fn wait_for_measurement(&mut self) -> Result<Flags, Error> {
        let mut waited_ms = 0;
        loop {
            let flags = self.flags().await?;
            if flags.conversion_ready {
                return Ok(flags);
            }
            if waited_ms >= DATA_READY_TIMEOUT_MS {
                return Err(Error::MeasurementTimeout);
            }
            Timer::after_millis(DATA_READY_POLL_INTERVAL_MS).await;
            waited_ms += DATA_READY_POLL_INTERVAL_MS;
        }
    }

    /// Collect the result of the measurement started by
    /// [`Opt4048::start_measurement`].
    ///
    /// `CONVERSION_READY` is polled first, so calling this early only costs
    /// the extra polls; waiting [`Configuration::measurement_time_us`]
    /// beforehand makes the first poll succeed. The four channels sit in one
    /// contiguous register block, so they are pulled in a single transfer and
    /// therefore all belong to the same measurement.
    pub async fn read_measurement(&mut self) -> Result<Measurement, Error> {
        let flags = self.wait_for_measurement().await?;

        let mut raw = [0u8; RESULT_BLOCK_LEN];
        self.read_registers(REG_CH0_EXPONENT_RESULT_MSB, &mut raw)
            .await?;

        let mut channels = [ChannelResult {
            exponent: 0,
            mantissa: 0,
            counter: 0,
            crc: 0,
        }; CHANNEL_COUNT];

        for (channel, bytes) in channels.iter_mut().zip(raw.chunks_exact(4)) {
            let msb = u16::from_be_bytes([bytes[0], bytes[1]]);
            let lsb = u16::from_be_bytes([bytes[2], bytes[3]]);

            let result_msb = (msb & RESULT_MSB_MASK) as u32;
            let result_lsb = (lsb >> RESULT_LSB_SHIFT) as u32;

            *channel = ChannelResult {
                exponent: (msb >> RESULT_EXPONENT_SHIFT) as u8,
                mantissa: (result_msb << 8) | result_lsb,
                counter: ((lsb >> RESULT_COUNTER_SHIFT) & RESULT_COUNTER_MASK) as u8,
                crc: (lsb & RESULT_CRC_MASK) as u8,
            };
        }

        Ok(Measurement {
            channels,
            overload: flags.overload,
        })
    }

    /// Start a measurement, wait out the conversion, and read the result.
    ///
    /// Convenient for callers that own the bus anyway; callers sharing it
    /// should use [`Opt4048::start_measurement`] and
    /// [`Opt4048::read_measurement`] so the bus is free while the device
    /// converts.
    pub async fn measure(&mut self) -> Result<Measurement, Error> {
        self.start_measurement().await?;
        Timer::after_micros(self.configuration.measurement_time_us() as u64).await;
        self.read_measurement().await
    }
}
