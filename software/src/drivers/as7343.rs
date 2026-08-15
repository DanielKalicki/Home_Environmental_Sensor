//! Driver for the ams AS7343 14-channel spectral sensor.
//!
//! The device has six ADCs. In the 18-channel auto-SMUX mode used here the
//! internal multiplexer runs three integration cycles back to back and stores
//! six results per cycle, so one measurement yields 18 words: the twelve
//! filtered spectral channels (F1..F8, FZ, FY, FXL, NIR), three unfiltered
//! visible-light readings (the datasheet's "2x VIS"), and three flicker-detect
//! readings.
//!
//! The visible and flicker photodiodes stay connected to ADC4 and ADC5 for all
//! three cycles, so their three values are three independent integrations of
//! the same input taken ~50 ms apart. Measured cycle-to-cycle spread on a
//! steady source is around 0.05% of the reading, so the three visible values
//! usually round to the same integer while the much larger flicker values
//! usually differ by a count or two.
//!
//! Measurement flow:
//!
//! 1. `init` checks the chip ID, resets the device, powers the analog block on
//!    and programs gain, integration time and the 18-channel auto-SMUX mode.
//! 2. `start_measurement` asserts `SP_EN`.
//! 3. After [`Configuration::measurement_time_us`] has passed, `read_measurement`
//!    confirms `AVALID` in `REG_STATUS2` and reads `REG_ASTATUS` and the 36 data
//!    bytes in one transfer.
//!
//! Splitting the last two steps lets the caller release a shared bus while the
//! device converts, which is by far the longest part of a cycle.
//!
//! Registers are split into two banks selected through `CFG0.REG_BANK`. The
//! driver tracks which bank is mapped in and only writes `CFG0` when it has to
//! change, so the bank calls that guard each method cost nothing in the common
//! case; every method that selects bank 1 restores bank 0 before returning.
//! Multi-byte registers are little-endian.

#![allow(dead_code)]

use embassy_time::Timer;
use esp_hal::{
    i2c::{Error as I2cError, Instance, I2C},
    Blocking,
};

/// Factory-default 7-bit address of the AS7343.
pub const DEFAULT_ADDRESS: u8 = 0x39;

// Bank-1 registers (accessible when CFG0.REG_BANK = 1, addresses 0x58-0x6B).
const REG_AUXID: u8 = 0x58;
const REG_REVID: u8 = 0x59;
const REG_ID: u8 = 0x5A;
const REG_CFG10: u8 = 0x65; // FD_PERS configuration
const REG_CFG12: u8 = 0x66; // SP_TH_CH configuration
const REG_GPIO: u8 = 0x6B;

// Bank-0 registers (accessible when CFG0.REG_BANK = 0, addresses 0x80+).
const REG_ENABLE: u8 = 0x80;
const REG_ATIME: u8 = 0x81;
const REG_WTIME: u8 = 0x83;
const REG_SP_TH_L: u8 = 0x84; // Spectral low threshold (16-bit)
const REG_SP_TH_H: u8 = 0x86; // Spectral high threshold (16-bit)
const REG_STATUS2: u8 = 0x90; // AVALID, saturation
const REG_STATUS3: u8 = 0x91; // Interrupt source
const REG_STATUS: u8 = 0x93; // Main status register
const REG_ASTATUS: u8 = 0x94; // ADC status
const REG_DATA_0_L: u8 = 0x95; // First of 18 channel data registers (low byte)
const REG_STATUS5: u8 = 0xBB; // SINT_FD, SINT_SMUX
const REG_STATUS4: u8 = 0xBC; // FIFO_OV, triggers
const REG_CFG0: u8 = 0xBF; // REG_BANK, LOW_POWER
const REG_CFG1: u8 = 0xC6; // AGAIN
const REG_CFG3: u8 = 0xC7; // SAI
const REG_CFG6: u8 = 0xF5; // SMUX_CMD
const REG_CFG8: u8 = 0xC9; // FIFO_TH
const REG_CFG9: u8 = 0xCA; // SIEN_FD, SIEN_SMUX
const REG_LED: u8 = 0xCD;
const REG_PERS: u8 = 0xCF;
const REG_ASTEP_L: u8 = 0xD4;
const REG_ASTEP_H: u8 = 0xD5;
const REG_CFG20: u8 = 0xD6; // auto_smux, FD_FIFO_8b
const REG_AGC_GAIN_MAX: u8 = 0xD7;
const REG_AZ_CONFIG: u8 = 0xDE;
const REG_FD_CFG0: u8 = 0xDF;
const REG_FD_TIME_1: u8 = 0xE0; // Flicker detection time LSB
const REG_FD_TIME_2: u8 = 0xE2; // Flicker detection time MSB + gain
const REG_FD_STATUS: u8 = 0xE3;
const REG_INTENAB: u8 = 0xF9;
const REG_CONTROL: u8 = 0xFA;
const REG_FIFO_MAP: u8 = 0xFC;
const REG_FIFO_LVL: u8 = 0xFD;
const REG_FDATA_L: u8 = 0xFE;
const REG_FDATA_H: u8 = 0xFF;

// `REG_BANK` bit of `REG_CFG0`, occupying bit 4 of the register. Set to select
// bank 1 (0x58-0x7F), clear to select bank 0 (0x80+, the power-on default).
const CFG0_REG_BANK_MASK: u8 = 0b0001_0000;

// `SW_RESET` bit of `REG_CONTROL`. Writing 1 triggers a full software reset;
// the device clears the bit itself once the reset has completed.
const CONTROL_SW_RESET_MASK: u8 = 0b0000_1000;

// `PON` bit of `REG_ENABLE`. Must be set before any other block (`SP_EN`,
// `WEN`, `FDEN`, ...) in the same register can be enabled.
const ENABLE_PON_MASK: u8 = 0b0000_0001;
// `SP_EN` bit of `REG_ENABLE`, starting a spectral measurement.
const ENABLE_SP_EN_MASK: u8 = 0b0000_0010;
// `WEN` bit of `REG_ENABLE`, inserting the `WTIME` wait between measurements.
const ENABLE_WEN_MASK: u8 = 0b0000_1000;
// `FDEN` bit of `REG_ENABLE`, enabling the flicker-detection block.
const ENABLE_FDEN_MASK: u8 = 0b0100_0000;

// `AGAIN` field of `REG_CFG1`, a 5-bit value occupying bits 4:0.
const CFG1_AGAIN_MASK: u8 = 0b0001_1111;

// `auto_smux` field of `REG_CFG20`, occupying bits 6:5. It selects how many
// channels one measurement produces: 0b00 = 6, 0b10 = 12, 0b11 = 18.
const CFG20_AUTO_SMUX_MASK: u8 = 0b0110_0000;
const CFG20_AUTO_SMUX_18_CHANNEL: u8 = 0b0110_0000;

// Bits of `REG_STATUS2`.
const STATUS2_FDSAT_DIGITAL_MASK: u8 = 0b0000_0001;
const STATUS2_FDSAT_ANALOG_MASK: u8 = 0b0000_0010;
// Analog saturation: at least one ADC exceeded its input range.
const STATUS2_ASAT_ANALOG_MASK: u8 = 0b0000_1000;
// Digital saturation: at least one channel reached its full-scale count.
const STATUS2_ASAT_DIGITAL_MASK: u8 = 0b0001_0000;
// Set once a complete measurement is available in the data registers.
const STATUS2_AVALID_MASK: u8 = 0b0100_0000;

// `AGAIN_STATUS` field of `REG_ASTATUS`, occupying bits 3:0. It reports the
// gain that was actually applied to the measurement just read.
const ASTATUS_AGAIN_STATUS_MASK: u8 = 0b0000_1111;

// `LED_ACT` bit of `REG_LED`, switching the external LED driver on.
const LED_ACT_MASK: u8 = 0b1000_0000;
// `LED_DRIVE` field of `REG_LED`, occupying bits 6:0.
const LED_DRIVE_MASK: u8 = 0b0111_1111;

// Status registers clear their bits when a 1 is written to them.
const STATUS_CLEAR_ALL: u8 = 0xFF;

// Settling time after a software reset before the device is ready again.
const RESET_DELAY_MS: u64 = 10;

// Settling time after `PON` is asserted before other blocks may be enabled.
const POWER_ON_DELAY_US: u64 = 1000;

// Time between two `AVALID` polls while a measurement is running.
const DATA_READY_POLL_INTERVAL_MS: u64 = 5;
// Longest time `measure` waits for `AVALID` before giving up.
const DATA_READY_TIMEOUT_MS: u64 = 2000;

const EXPECTED_CHIP_ID: u8 = 0x81;

/// Number of words one 18-channel auto-SMUX measurement produces.
pub const CHANNEL_COUNT: usize = 18;

// Duration of one ADC step in nanoseconds (2.78 us, per the datasheet).
const ASTEP_UNIT_NS: u32 = 2780;
// Number of integration cycles the 18-channel auto-SMUX sequence runs.
const AUTO_SMUX_18_CHANNEL_CYCLES: u32 = 3;

// Smallest LED drive current the device can source, in milliamperes.
const LED_MIN_CURRENT_MA: u16 = 4;
// Largest LED drive current the device can source, in milliamperes.
const LED_MAX_CURRENT_MA: u16 = 258;
// Current added per `LED_DRIVE` step, in milliamperes.
const LED_CURRENT_STEP_MA: u16 = 2;

/// Spectral measurement gain (`AGAIN` field of `REG_CFG1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gain {
    Gain0_5x = 0,
    Gain1x = 1,
    Gain2x = 2,
    Gain4x = 3,
    Gain8x = 4,
    Gain16x = 5,
    Gain32x = 6,
    Gain64x = 7,
    Gain128x = 8,
    /// Power-on default.
    Gain256x = 9,
    Gain512x = 10,
    Gain1024x = 11,
    Gain2048x = 12,
}

impl Gain {
    /// Decode the raw `AGAIN` / `AGAIN_STATUS` field.
    pub fn from_raw(raw: u8) -> Option<Self> {
        Some(match raw {
            0 => Gain::Gain0_5x,
            1 => Gain::Gain1x,
            2 => Gain::Gain2x,
            3 => Gain::Gain4x,
            4 => Gain::Gain8x,
            5 => Gain::Gain16x,
            6 => Gain::Gain32x,
            7 => Gain::Gain64x,
            8 => Gain::Gain128x,
            9 => Gain::Gain256x,
            10 => Gain::Gain512x,
            11 => Gain::Gain1024x,
            12 => Gain::Gain2048x,
            _ => return None,
        })
    }

    /// Factor the raw counts were multiplied by: `0.5 * 2^raw`.
    pub fn multiplier(self) -> f32 {
        0.5 * (1u32 << (self as u8)) as f32
    }
}

/// One of the 18 words a measurement produces, in read-out order.
///
/// The 18-channel auto-SMUX sequence runs three integration cycles of six ADC
/// channels each; the discriminants are the indices into [`Measurement::raw`].
/// Wavelengths given below are the channel centres.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// 450 nm.
    Fz = 0,
    /// 555 nm.
    Fy = 1,
    /// 600 nm.
    Fxl = 2,
    /// 855 nm, near infrared.
    Nir = 3,
    /// Unfiltered visible light ("2x VIS"), cycle 1.
    Visible1 = 4,
    /// Flicker-detect ADC, cycle 1.
    FlickerDetect1 = 5,
    /// 425 nm.
    F2 = 6,
    /// 475 nm.
    F3 = 7,
    /// 515 nm.
    F4 = 8,
    /// 640 nm.
    F6 = 9,
    /// Unfiltered visible light ("2x VIS"), cycle 2.
    Visible2 = 10,
    /// Flicker-detect ADC, cycle 2.
    FlickerDetect2 = 11,
    /// 405 nm.
    F1 = 12,
    /// 690 nm.
    F7 = 13,
    /// 745 nm.
    F8 = 14,
    /// 550 nm.
    F5 = 15,
    /// Unfiltered visible light ("2x VIS"), cycle 3.
    Visible3 = 16,
    /// Flicker-detect ADC, cycle 3.
    FlickerDetect3 = 17,
}

/// The twelve filtered spectral channels, ordered by centre wavelength.
pub const SPECTRAL_CHANNELS: [Channel; 12] = [
    Channel::F1,
    Channel::F2,
    Channel::Fz,
    Channel::F3,
    Channel::F4,
    Channel::F5,
    Channel::Fy,
    Channel::Fxl,
    Channel::F6,
    Channel::F7,
    Channel::F8,
    Channel::Nir,
];

impl Channel {
    /// Centre wavelength in nanometres, or `None` for the unfiltered visible
    /// and flicker-detect channels.
    pub fn wavelength_nm(self) -> Option<u16> {
        Some(match self {
            Channel::F1 => 405,
            Channel::F2 => 425,
            Channel::Fz => 450,
            Channel::F3 => 475,
            Channel::F4 => 515,
            Channel::F5 => 550,
            Channel::Fy => 555,
            Channel::Fxl => 600,
            Channel::F6 => 640,
            Channel::F7 => 690,
            Channel::F8 => 745,
            Channel::Nir => 855,
            Channel::Visible1
            | Channel::Visible2
            | Channel::Visible3
            | Channel::FlickerDetect1
            | Channel::FlickerDetect2
            | Channel::FlickerDetect3 => return None,
        })
    }
}

/// Device identification read from bank 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identification {
    /// Auxiliary identification (`REG_AUXID`, bits 3:0 are significant).
    pub auxid: u8,
    /// Silicon revision (`REG_REVID`, bits 2:0 are significant).
    pub revid: u8,
    /// Part number; 0x81 for the AS7343.
    pub id: u8,
}

/// One complete 18-channel measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measurement {
    /// Raw ADC counts, indexed by [`Channel`].
    pub raw: [u16; CHANNEL_COUNT],
    /// Gain the device reports it applied (`AGAIN_STATUS` of `REG_ASTATUS`).
    pub gain: Option<Gain>,
    /// At least one ADC exceeded its analog input range; counts are invalid.
    pub analog_saturation: bool,
    /// At least one channel reached full scale; counts are clipped.
    pub digital_saturation: bool,
}

impl Measurement {
    /// Raw ADC counts of one channel.
    pub fn channel(&self, channel: Channel) -> u16 {
        self.raw[channel as usize]
    }

    /// Gain- and time-normalised counts of one channel ("basic counts").
    ///
    /// Dividing by gain and integration time makes readings taken with
    /// different settings comparable: `raw / (gain * t_int_ms)`. Returns `None`
    /// if the device reported an unknown gain code.
    pub fn basic_count(&self, channel: Channel, integration_time_us: u32) -> Option<f32> {
        let gain = self.gain?;
        let integration_time_ms = integration_time_us as f32 / 1000.0;
        Some(self.channel(channel) as f32 / (gain.multiplier() * integration_time_ms))
    }

    /// Whether either saturation flag was set for this measurement.
    pub fn saturated(&self) -> bool {
        self.analog_saturation || self.digital_saturation
    }
}

/// Settings applied by [`As7343::init`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Configuration {
    /// Spectral gain (`AGAIN`).
    pub gain: Gain,
    /// Number of integration steps minus one (`ATIME`).
    pub atime: u8,
    /// Length of one integration step minus one (`ASTEP`), in 2.78 us units.
    /// 0xFFFF is reserved and must not be used.
    pub astep: u16,
}

impl Configuration {
    /// Integration time of a single cycle in microseconds:
    /// `(atime + 1) * (astep + 1) * 2.78 us`.
    pub const fn integration_time_us(&self) -> u32 {
        (self.atime as u32 + 1) * (self.astep as u32 + 1) * ASTEP_UNIT_NS / 1000
    }

    /// Time one full 18-channel measurement takes, in microseconds.
    ///
    /// The auto-SMUX sequence runs three integration cycles back to back.
    pub const fn measurement_time_us(&self) -> u32 {
        self.integration_time_us() * AUTO_SMUX_18_CHANNEL_CYCLES
    }
}

/// Errors reported by the driver.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Error {
    /// The I2C transfer itself failed (no acknowledge, timeout, ...).
    Bus(I2cError),
    /// `REG_ID` did not hold the AS7343 part number.
    InvalidChipId(u8),
    /// `AVALID` was not set within the data-ready timeout.
    MeasurementTimeout,
}

impl From<I2cError> for Error {
    fn from(error: I2cError) -> Self {
        Error::Bus(error)
    }
}

pub struct As7343<'a, 'd, T> {
    i2c: &'a mut I2C<'d, T, Blocking>,
    address: u8,
    /// Which bank the device currently has mapped in.
    ///
    /// The bank bit lives in the device and survives between driver instances,
    /// so this mirrors it instead of reading it back: every method that selects
    /// bank 1 restores bank 0 before returning, and bank 0 is also what a reset
    /// and a power-on leave selected. `set_bank` therefore only touches the bus
    /// when the bank really has to change.
    bank1: bool,
}

impl<'a, 'd, T: Instance> As7343<'a, 'd, T> {
    /// Bind the driver to a bus, using the factory-default address.
    pub fn new(i2c: &'a mut I2C<'d, T, Blocking>) -> Self {
        Self::with_address(i2c, DEFAULT_ADDRESS)
    }

    /// Bind the driver to a bus using an explicit 7-bit address.
    pub fn with_address(i2c: &'a mut I2C<'d, T, Blocking>, address: u8) -> Self {
        Self {
            i2c,
            address,
            bank1: false,
        }
    }

    /// Read one or more consecutive registers starting at `register`.
    ///
    /// The register pointer auto-increments on reads, so a single transfer can
    /// pull a whole block such as a run of channel data registers.
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

    /// Switch which register bank is mapped in, if it is not already.
    ///
    /// `true` selects bank 1, exposing `REG_AUXID`/`REG_REVID`/`REG_ID`/
    /// `REG_CFG10`/`REG_CFG12`/`REG_GPIO` (addresses 0x58-0x6B). `false`
    /// selects bank 0 (the power-on default), exposing the measurement and
    /// configuration registers at 0x80 and above.
    ///
    /// The requested bank is compared against [`As7343::bank1`] first, so the
    /// calls that every method makes to guarantee its own register window cost
    /// nothing unless they actually change the window.
    async fn set_bank(&mut self, bank1: bool) -> Result<(), Error> {
        if self.bank1 == bank1 {
            return Ok(());
        }

        let value = if bank1 { CFG0_REG_BANK_MASK } else { 0 };
        self.update_register(REG_CFG0, CFG0_REG_BANK_MASK, value)
            .await?;
        self.bank1 = bank1;
        Ok(())
    }

    /// Read the identification registers; useful as a presence check.
    ///
    /// They live in bank 1; bank 0 is selected again before returning so the
    /// caller is left with the measurement registers mapped in.
    pub async fn identification(&mut self) -> Result<Identification, Error> {
        self.set_bank(/*bank1=*/ true).await?;
        let mut raw = [0u8; 3];
        let result = self.read_registers(REG_AUXID, &mut raw).await;
        self.set_bank(/*bank1=*/ false).await?;
        result?;

        Ok(Identification {
            auxid: raw[0],
            revid: raw[1],
            id: raw[2],
        })
    }

    /// Trigger a full software reset.
    ///
    /// `REG_CONTROL` lives at 0xFA, so bank 0 must be selected first. The
    /// device reloads its power-on defaults, which includes selecting bank 0,
    /// so the tracked bank stays correct without a further write.
    pub async fn reset(&mut self) -> Result<(), Error> {
        self.set_bank(false).await?;
        self.update_register(REG_CONTROL, CONTROL_SW_RESET_MASK, CONTROL_SW_RESET_MASK)
            .await?;
        Timer::after_millis(RESET_DELAY_MS).await;
        self.bank1 = false;
        Ok(())
    }

    /// Power on the analog block (`PON` in `REG_ENABLE`).
    ///
    /// Must be done before enabling `SP_EN`, `WEN`, or `FDEN` in the same
    /// register. The device needs a short settling time afterwards.
    pub async fn power_on(&mut self) -> Result<(), Error> {
        self.set_bank(false).await?;
        self.update_register(REG_ENABLE, ENABLE_PON_MASK, ENABLE_PON_MASK)
            .await?;
        Timer::after_micros(POWER_ON_DELAY_US).await;
        Ok(())
    }

    /// Power the analog block down again (`PON` cleared).
    pub async fn power_off(&mut self) -> Result<(), Error> {
        self.set_bank(false).await?;
        self.update_register(REG_ENABLE, ENABLE_PON_MASK, 0).await
    }

    /// Set the spectral measurement gain.
    ///
    /// `REG_CFG1` lives at 0xC6, so bank 0 must be selected first.
    pub async fn set_gain(&mut self, gain: Gain) -> Result<(), Error> {
        self.set_bank(false).await?;
        self.update_register(REG_CFG1, CFG1_AGAIN_MASK, gain as u8)
            .await
    }

    /// Set the integration time by writing the `ATIME` and `ASTEP` registers.
    ///
    /// Both registers live in bank 0, so bank 0 must be selected first.
    /// `ASTEP` is a 16-bit, little-endian register pair (`REG_ASTEP_L`/`REG_ASTEP_H`).
    /// Integration time is `t_int = (atime + 1) * (astep + 1) * 2.78 us`;
    /// `astep` values of 0xFFFF are reserved.
    pub async fn set_integration_time(&mut self, atime: u8, astep: u16) -> Result<(), Error> {
        self.set_bank(false).await?;
        self.write_register(REG_ATIME, atime).await?;
        self.write_register(REG_ASTEP_L, (astep & 0xFF) as u8)
            .await?;
        self.write_register(REG_ASTEP_H, (astep >> 8) as u8).await
    }

    /// Select the 18-channel auto-SMUX sequence.
    ///
    /// The multiplexer then runs three integration cycles per measurement and
    /// fills all 18 data registers, which is what [`As7343::read_measurement`]
    /// reads.
    async fn enable_18_channel_auto_smux(&mut self) -> Result<(), Error> {
        self.set_bank(false).await?;
        self.update_register(REG_CFG20, CFG20_AUTO_SMUX_MASK, CFG20_AUTO_SMUX_18_CHANNEL)
            .await
    }

    /// Start or stop a spectral measurement (`SP_EN` in `REG_ENABLE`).
    async fn set_spectral_measurement(&mut self, enabled: bool) -> Result<(), Error> {
        let value = if enabled { ENABLE_SP_EN_MASK } else { 0 };
        self.update_register(REG_ENABLE, ENABLE_SP_EN_MASK, value)
            .await
    }

    /// Drive the external illumination LED connected to the `LDR` pin.
    ///
    /// `current_ma` is clamped to the 4..=258 mA the driver supports and
    /// rounded down to the nearest 2 mA step.
    pub async fn set_led(&mut self, enabled: bool, current_ma: u16) -> Result<(), Error> {
        let clamped = current_ma.clamp(LED_MIN_CURRENT_MA, LED_MAX_CURRENT_MA);
        let drive = ((clamped - LED_MIN_CURRENT_MA) / LED_CURRENT_STEP_MA) as u8 & LED_DRIVE_MASK;
        let value = if enabled { LED_ACT_MASK | drive } else { drive };
        self.set_bank(false).await?;
        self.write_register(REG_LED, value).await
    }

    /// Poll `REG_STATUS2` until `AVALID` reports a complete measurement.
    ///
    /// Returns the status byte so the caller can also read the saturation
    /// flags it carries.
    async fn wait_for_measurement(&mut self) -> Result<u8, Error> {
        let mut waited_ms = 0;
        loop {
            let status2 = self.read_register(REG_STATUS2).await?;
            if status2 & STATUS2_AVALID_MASK != 0 {
                return Ok(status2);
            }
            if waited_ms >= DATA_READY_TIMEOUT_MS {
                return Err(Error::MeasurementTimeout);
            }
            Timer::after_millis(DATA_READY_POLL_INTERVAL_MS).await;
            waited_ms += DATA_READY_POLL_INTERVAL_MS;
        }
    }

    /// Start one 18-channel measurement.
    ///
    /// Any conversion still in flight is stopped first, so the counts read back
    /// afterwards belong to the measurement this call started. This costs three
    /// short register accesses and returns immediately; the device then
    /// converts for [`Configuration::measurement_time_us`] on its own, without
    /// needing the bus.
    pub async fn start_measurement(&mut self) -> Result<(), Error> {
        self.set_bank(false).await?;
        self.set_spectral_measurement(false).await?;
        // Status bits are write-1-to-clear; drop anything left from before.
        self.write_register(REG_STATUS, STATUS_CLEAR_ALL).await?;
        self.set_spectral_measurement(true).await
    }

    /// Collect the result of the measurement started by
    /// [`As7343::start_measurement`].
    ///
    /// `AVALID` is polled first, so calling this early only costs the extra
    /// polls; waiting [`Configuration::measurement_time_us`] beforehand makes
    /// the first poll succeed. `SP_EN` is cleared before returning, leaving the
    /// device powered but idle.
    pub async fn read_measurement(&mut self) -> Result<Measurement, Error> {
        self.set_bank(false).await?;

        let status2 = match self.wait_for_measurement().await {
            Ok(status2) => status2,
            Err(error) => {
                // Do not leave the device converting after a failed wait.
                let _ = self.set_spectral_measurement(false).await;
                return Err(error);
            }
        };

        // `REG_ASTATUS` directly precedes the 18 little-endian data words, so
        // one auto-incrementing read covers both.
        let mut raw = [0u8; 1 + 2 * CHANNEL_COUNT];
        self.read_registers(REG_ASTATUS, &mut raw).await?;
        self.set_spectral_measurement(false).await?;

        let mut counts = [0u16; CHANNEL_COUNT];
        for (count, bytes) in counts.iter_mut().zip(raw[1..].chunks_exact(2)) {
            *count = u16::from_le_bytes([bytes[0], bytes[1]]);
        }

        Ok(Measurement {
            raw: counts,
            gain: Gain::from_raw(raw[0] & ASTATUS_AGAIN_STATUS_MASK),
            analog_saturation: status2 & STATUS2_ASAT_ANALOG_MASK != 0,
            digital_saturation: status2 & STATUS2_ASAT_DIGITAL_MASK != 0,
        })
    }

    /// Bring the sensor into a known state and apply `configuration`.
    ///
    /// The chip ID is verified first, so a wiring or address fault is reported
    /// before anything is written. Afterwards the device is powered on and
    /// idle, with bank 0 selected; take readings with
    /// [`As7343::start_measurement`] and [`As7343::read_measurement`].
    ///
    /// The settings written here stay in the device's registers until it is
    /// reset or loses power, so they are not rewritten per measurement.
    pub async fn init(&mut self, configuration: &Configuration) -> Result<(), Error> {
        let identification = self.identification().await?;
        if identification.id != EXPECTED_CHIP_ID {
            return Err(Error::InvalidChipId(identification.id));
        }

        self.reset().await?;
        self.power_on().await?;
        self.set_gain(configuration.gain).await?;
        self.set_integration_time(configuration.atime, configuration.astep)
            .await?;
        self.enable_18_channel_auto_smux().await?;

        // self.set_led(true, 4).await?;
        Ok(())
    }
}