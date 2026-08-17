//! Driver for the Melexis MLX90640ESF-BAA 32x24 far-infrared thermal array.
//!
//! The device is a 768-pixel thermopile array with a 110 deg x 75 deg field of
//! view (the `BAA` suffix; the `BAB` part is the narrow 55 deg x 35 deg one).
//! It measures continuously on its own and only has to be read out; there is
//! no "start a conversion" command in the normal, continuous mode used here.
//!
//! # What a read-out actually returns
//!
//! The array is never sampled as a whole. Every refresh the device measures one
//! *subpage*, which is half of the pixels, and writes it into the same RAM
//! block. Which half depends on the reading pattern:
//!
//! * **Chess** (the factory default and the pattern the sensor is calibrated
//!   for): the two subpages are the black and the white squares of a
//!   chessboard.
//! * **Interleaved**: the two subpages are the odd and the even rows.
//!
//! So one [`Mlx90640::read_frame`] fills in half the image and leaves the other
//! half holding whatever the previous read-out put there. A complete, coherent
//! image therefore costs two read-outs, one per subpage, and
//! [`Frame::object_temperatures`] only overwrites the pixels belonging to the
//! subpage it was given. Keep the destination array between calls and it fills
//! in alternately.
//!
//! # Read-out flow
//!
//! 1. [`Mlx90640::init`] dumps the 832-word calibration EEPROM, turns it into
//!    [`Parameters`], and writes the refresh rate, ADC resolution and reading
//!    pattern.
//! 2. [`Mlx90640::data_ready`] reports whether the device has finished a
//!    subpage since the last read-out. It is one word on the bus, so it can be
//!    polled with the shared bus released in between.
//! 3. [`Mlx90640::read_frame`] clears the flag and copies the whole 832-word
//!    RAM block plus the control register into a [`Frame`].
//! 4. [`Frame::object_temperatures`] turns the raw counts into degrees Celsius
//!    using the [`Parameters`], an emissivity and a reflected temperature.
//!
//! # Bus time
//!
//! A frame is 1664 bytes of payload. On the 100 kHz bus this board runs, one
//! read-out occupies the bus for roughly 150 ms, and the shared bus is held for
//! all of it. That is why the refresh rate should stay at 1 or 2 Hz per
//! subpage: at 4 Hz a new subpage lands every 250 ms and the read has to race
//! it, and at 8 Hz and above it cannot keep up at all. The device also has to
//! be addressed 16 bits at a time and the I2C peripheral refuses transfers
//! longer than 254 bytes, so [`Mlx90640::read_words`] splits every block into
//! chunks of at most [`MAX_WORDS_PER_TRANSFER`] words.
//!
//! # Memory
//!
//! [`Parameters`] and [`Frame`] are large (about 9.5 kB and 1.7 kB). They are
//! deliberately owned by the caller rather than by the driver, which is created
//! per transaction like the other drivers here, so that they can live in a
//! `static` instead of on a task's stack.
//!
//! Every register address is 16 bits and every value on the wire is
//! big-endian.

#![allow(dead_code)]

use embassy_time::Timer;
use esp_hal::{
    i2c::{Error as I2cError, Instance, I2C},
    Blocking,
};
use libm::sqrtf;

/// Factory-default 7-bit address of the MLX90640.
pub const DEFAULT_ADDRESS: u8 = 0x33;

/// Status register: last measured subpage, data-ready flag, overwrite enable.
const REG_STATUS: u16 = 0x8000;
/// Control register 1: refresh rate, ADC resolution, reading pattern.
const REG_CONTROL1: u16 = 0x800D;
/// I2C configuration register (FM+ enable, SDA current limit). Never written.
const REG_I2C_CONFIG: u16 = 0x800F;
/// First word of the measurement RAM (832 words, 0x0400-0x073F).
const RAM_BASE: u16 = 0x0400;
/// First word of the calibration EEPROM (832 words, 0x2400-0x273F).
const EEPROM_BASE: u16 = 0x2400;

/// Bits 2:0 of the status register: the subpage the device measured last.
const STATUS_SUBPAGE_MASK: u16 = 0x0007;
/// Bit 3 of the status register: a subpage has been written since it was last
/// cleared. Cleared by writing a 0 to it.
const STATUS_DATA_READY_MASK: u16 = 0x0008;
/// Bit 4 of the status register: let the device overwrite RAM that has not
/// been read yet. Without it a missed read-out would stall the device.
const STATUS_ENABLE_OVERWRITE_MASK: u16 = 0x0010;
/// Bit 5 of the status register: start one measurement in step mode. Ignored
/// in the continuous mode this driver uses.
const STATUS_START_MEASUREMENT_MASK: u16 = 0x0020;

/// Value written to acknowledge a frame: clears the data-ready flag and leaves
/// overwriting enabled.
const STATUS_ACKNOWLEDGE: u16 = STATUS_ENABLE_OVERWRITE_MASK | STATUS_START_MEASUREMENT_MASK;

/// Bit 0 of control register 1: alternate between the two subpages. Clearing it
/// makes the device repeat one subpage forever.
const CONTROL1_SUBPAGE_MODE_MASK: u16 = 0x0001;
/// Bit 2 of control register 1: measure only the subpage selected by bits 6:4.
const CONTROL1_SUBPAGE_REPEAT_MASK: u16 = 0x0004;
/// Bits 6:4 of control register 1: which subpage to repeat.
const CONTROL1_SELECTED_SUBPAGE_MASK: u16 = 0x0070;
/// Bits 9:7 of control register 1: refresh rate, see [`RefreshRate`].
const CONTROL1_REFRESH_RATE_MASK: u16 = 0x0380;
const CONTROL1_REFRESH_RATE_SHIFT: u32 = 7;
/// Bits 11:10 of control register 1: ADC resolution, see [`Resolution`].
const CONTROL1_RESOLUTION_MASK: u16 = 0x0C00;
const CONTROL1_RESOLUTION_SHIFT: u32 = 10;
/// Bit 12 of control register 1: set for the chess pattern, clear for the
/// interleaved (television) pattern.
const CONTROL1_CHESS_PATTERN_MASK: u16 = 0x1000;

/// Columns in the array.
pub const COLUMNS: usize = 32;
/// Rows in the array.
pub const ROWS: usize = 24;
/// Pixels in the array. Half of them are refreshed per subpage.
pub const PIXEL_COUNT: usize = COLUMNS * ROWS;
/// Words in the measurement RAM block: 768 pixels plus 64 auxiliary words.
pub const RAM_WORDS: usize = 832;
/// Words in the calibration EEPROM block.
pub const EEPROM_WORDS: usize = 832;

// Indices into the RAM block of the auxiliary words the compensation needs.
/// PTAT reading taken with the auxiliary (art) sensor (RAM 0x0700).
const RAM_INDEX_PTAT_ART: usize = 768;
/// Compensation-pixel reading of subpage 0 (RAM 0x0708).
const RAM_INDEX_CP_SUBPAGE0: usize = 776;
/// Gain reading (RAM 0x070A).
const RAM_INDEX_GAIN: usize = 778;
/// PTAT reading of the main sensor (RAM 0x0720).
const RAM_INDEX_PTAT: usize = 800;
/// Compensation-pixel reading of subpage 1 (RAM 0x0728).
const RAM_INDEX_CP_SUBPAGE1: usize = 808;
/// Supply-voltage reading (RAM 0x072A).
const RAM_INDEX_VDD: usize = 810;

/// Largest number of 16-bit words one I2C transfer may carry.
///
/// The esp-hal I2C driver rejects transfers longer than 254 bytes, so blocks
/// are read 127 words at a time.
pub const MAX_WORDS_PER_TRANSFER: usize = 127;

/// How many times [`Mlx90640::read_frame`] re-reads RAM that the device
/// overwrote while it was being read.
const FRAME_READ_ATTEMPTS: usize = 4;

/// Number of pixels the EEPROM may flag as deviating before the part counts as
/// out of specification.
pub const MAX_DEVIATING_PIXELS: usize = 4;

/// Supply voltage the calibration data are referenced to, in volts.
const NOMINAL_VDD: f32 = 3.3;
/// Ambient temperature the calibration data are referenced to, in Celsius.
const NOMINAL_TA: f32 = 25.0;
/// Zero Celsius in kelvin.
const KELVIN_OFFSET: f32 = 273.15;

/// A raw copy of the calibration EEPROM, as read by [`Mlx90640::read_eeprom`].
pub type Eeprom = [u16; EEPROM_WORDS];

/// Errors reported by the driver.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Error {
    /// The I2C transfer itself failed (no acknowledge, timeout, ...).
    Bus(I2cError),
    /// A register did not read back the value that was just written to it.
    WriteVerification {
        register: u16,
        written: u16,
        read: u16,
    },
    /// [`Mlx90640::read_frame`] was called before the device had a new subpage.
    FrameNotReady,
    /// The device finished a new subpage during every attempt to read the
    /// current one, so no read-out could be shown to hold a single subpage.
    /// The refresh rate is too high for the bus speed.
    FrameOverwritten,
    /// The EEPROM flags more than [`MAX_DEVIATING_PIXELS`] pixels as broken or
    /// out of specification, which the datasheet treats as a faulty part.
    TooManyDeviatingPixels(usize),
}

impl From<I2cError> for Error {
    fn from(error: I2cError) -> Self {
        Error::Bus(error)
    }
}

/// Refresh rate of a *subpage* (bits 9:7 of control register 1).
///
/// A complete image needs both subpages, so the image rate is half of this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshRate {
    Hz0_5 = 0,
    Hz1 = 1,
    /// Power-on default.
    Hz2 = 2,
    Hz4 = 3,
    Hz8 = 4,
    Hz16 = 5,
    Hz32 = 6,
    Hz64 = 7,
}

impl RefreshRate {
    /// Decode the raw 3-bit field.
    pub fn from_raw(raw: u16) -> Self {
        match raw & 0x07 {
            0 => RefreshRate::Hz0_5,
            1 => RefreshRate::Hz1,
            2 => RefreshRate::Hz2,
            3 => RefreshRate::Hz4,
            4 => RefreshRate::Hz8,
            5 => RefreshRate::Hz16,
            6 => RefreshRate::Hz32,
            _ => RefreshRate::Hz64,
        }
    }

    /// Time between two subpages, in microseconds: `2 s / 2^raw`.
    pub const fn subpage_period_us(self) -> u64 {
        2_000_000 >> (self as u32)
    }

    /// Time between two complete images, in microseconds.
    pub const fn image_period_us(self) -> u64 {
        2 * self.subpage_period_us()
    }
}

/// ADC resolution (bits 11:10 of control register 1).
///
/// The calibration was taken at one particular resolution; reading at a
/// different one is corrected for in [`Frame::supply_voltage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Bits16 = 0,
    Bits17 = 1,
    /// Power-on default.
    Bits18 = 2,
    Bits19 = 3,
}

impl Resolution {
    /// Decode the raw 2-bit field.
    pub fn from_raw(raw: u16) -> Self {
        match raw & 0x03 {
            0 => Resolution::Bits16,
            1 => Resolution::Bits17,
            2 => Resolution::Bits18,
            _ => Resolution::Bits19,
        }
    }
}

/// Which half of the array each subpage covers (bit 12 of control register 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    /// Odd and even rows. Needs the correction terms in
    /// [`Frame::object_temperatures`] because the part is calibrated in chess
    /// mode.
    Interleaved,
    /// The squares of a chessboard. The factory default, and what the device is
    /// calibrated for.
    Chess,
}

/// Settings applied by [`Mlx90640::init`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Configuration {
    /// How often the device measures one subpage.
    pub refresh_rate: RefreshRate,
    /// ADC resolution.
    pub resolution: Resolution,
    /// Which half of the array a subpage covers.
    pub pattern: Pattern,
}

impl Configuration {
    /// The device's own power-on settings: 2 Hz per subpage, 18-bit ADC, chess.
    pub const DEFAULT: Configuration = Configuration {
        refresh_rate: RefreshRate::Hz2,
        resolution: Resolution::Bits18,
        pattern: Pattern::Chess,
    };

    /// Time between two subpages, in microseconds.
    pub const fn subpage_period_us(&self) -> u64 {
        self.refresh_rate.subpage_period_us()
    }

    /// Time between two complete images, in microseconds.
    pub const fn image_period_us(&self) -> u64 {
        self.refresh_rate.image_period_us()
    }

    /// Encode the fields into the bits of control register 1 they own.
    const fn control_bits(&self) -> u16 {
        let mut bits = (self.refresh_rate as u16) << CONTROL1_REFRESH_RATE_SHIFT;
        bits |= (self.resolution as u16) << CONTROL1_RESOLUTION_SHIFT;
        if let Pattern::Chess = self.pattern {
            bits |= CONTROL1_CHESS_PATTERN_MASK;
        }
        bits
    }

    /// Decode a control register 1 value.
    fn from_control(control: u16) -> Self {
        Configuration {
            refresh_rate: RefreshRate::from_raw(
                (control & CONTROL1_REFRESH_RATE_MASK) >> CONTROL1_REFRESH_RATE_SHIFT,
            ),
            resolution: Resolution::from_raw(
                (control & CONTROL1_RESOLUTION_MASK) >> CONTROL1_RESOLUTION_SHIFT,
            ),
            pattern: if control & CONTROL1_CHESS_PATTERN_MASK != 0 {
                Pattern::Chess
            } else {
                Pattern::Interleaved
            },
        }
    }
}

impl Default for Configuration {
    fn default() -> Self {
        Configuration::DEFAULT
    }
}

/// Decoded status register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    /// The subpage the device measured last, 0 or 1.
    pub subpage: u8,
    /// A subpage has been written since the flag was last cleared.
    pub data_ready: bool,
    /// The device may overwrite RAM that has not been read out.
    pub overwrite_enabled: bool,
}

impl Status {
    fn from_raw(raw: u16) -> Self {
        Status {
            subpage: (raw & STATUS_SUBPAGE_MASK) as u8,
            data_ready: raw & STATUS_DATA_READY_MASK != 0,
            overwrite_enabled: raw & STATUS_ENABLE_OVERWRITE_MASK != 0,
        }
    }
}

/// Sign-extend the low `bits` of `value` to a signed integer.
///
/// Every calibration constant in the EEPROM is a two's-complement field of its
/// own width packed into a word, so this replaces the datasheet's repeated
/// "if the value is greater than `2^(n-1) - 1`, subtract `2^n`".
const fn sign_extend(value: u16, bits: u32) -> i32 {
    let shift = 32 - bits;
    ((value as i32) << shift) >> shift
}

/// `2^exponent` as a float, for the many EEPROM scaling factors.
fn exp2(exponent: u32) -> f32 {
    (1u64 << exponent) as f32
}

/// Fourth root, clamped at zero.
///
/// The temperature solution takes the fourth root of a Stefan-Boltzmann style
/// term. That term only goes negative for a signal colder than the model can
/// represent, where the real root does not exist; returning 0 there keeps a
/// single bad pixel from turning into a NaN that spreads through whatever the
/// caller does with the image.
fn fourth_root(value: f32) -> f32 {
    if value <= 0.0 {
        0.0
    } else {
        sqrtf(sqrtf(value))
    }
}

/// Which of the four calibration quadrants a pixel belongs to.
///
/// `Kv` and the pixel `Kta` base value are stored per (odd/even row,
/// odd/even column) combination rather than per pixel. Rows and columns are
/// numbered from 1 in the datasheet, so index 0 is an odd row.
const fn quadrant(pixel: usize) -> usize {
    let row = pixel / COLUMNS;
    2 * (row & 1) + (pixel & 1)
}

/// The calibration constants held in the device's EEPROM, unpacked.
///
/// This is about 9.5 kB, so it belongs in a `static` (a `StaticCell`, or PSRAM)
/// rather than on a task's stack. Build it once with
/// [`Parameters::zeroed`] and fill it in with [`Mlx90640::init`] or
/// [`Parameters::extract`]; the contents never change afterwards.
pub struct Parameters {
    /// Supply-voltage sensor sensitivity, in ADC counts per volt.
    k_vdd: f32,
    /// Supply-voltage sensor reading at 3.3 V.
    vdd25: f32,
    /// Supply-voltage dependence of the ambient-temperature sensor.
    kv_ptat: f32,
    /// Temperature dependence of the ambient-temperature sensor.
    kt_ptat: f32,
    /// Ambient-temperature sensor reading at 25 degrees Celsius.
    v_ptat25: f32,
    /// Ratio between the two ambient-temperature sensors.
    alpha_ptat: f32,
    /// Reference gain the measured gain is normalised against.
    gain_ee: f32,
    /// Temperature-gradient coefficient, weighting the compensation pixel.
    tgc: f32,
    /// Ambient-temperature dependence of the pixel sensitivity.
    ks_ta: f32,
    /// Sensitivity correction per object-temperature range.
    ks_to: [f32; 5],
    /// Corner temperatures of those ranges, in degrees Celsius.
    ct: [i16; 5],
    /// ADC resolution the part was calibrated at.
    resolution_ee: Resolution,
    /// Whether the part was calibrated in the chess pattern.
    calibration_chess: bool,
    /// Per-pixel sensitivity.
    alpha: [f32; PIXEL_COUNT],
    /// Per-pixel offset, in ADC counts.
    offset: [i32; PIXEL_COUNT],
    /// Per-pixel ambient-temperature coefficient of the offset.
    kta: [f32; PIXEL_COUNT],
    /// Supply-voltage coefficient of the offset, per quadrant.
    kv: [f32; 4],
    /// Sensitivity of the two compensation pixels.
    cp_alpha: [f32; 2],
    /// Offset of the two compensation pixels.
    cp_offset: [f32; 2],
    /// Ambient-temperature coefficient of the compensation pixels.
    cp_kta: f32,
    /// Supply-voltage coefficient of the compensation pixels.
    cp_kv: f32,
    /// Corrections applied when reading interleaved from a chess-calibrated
    /// part.
    il_chess_c: [f32; 3],
    /// Pixels the EEPROM marks as dead.
    broken: [u16; MAX_DEVIATING_PIXELS],
    broken_count: u8,
    /// Pixels the EEPROM marks as outside the specified tolerance.
    outlier: [u16; MAX_DEVIATING_PIXELS],
    outlier_count: u8,
}

impl Parameters {
    /// An empty set, to be filled in by [`Parameters::extract`].
    ///
    /// `const` so that it can initialise a `static` without a runtime copy of
    /// the whole structure.
    pub const fn zeroed() -> Self {
        Parameters {
            k_vdd: 0.0,
            vdd25: 0.0,
            kv_ptat: 0.0,
            kt_ptat: 0.0,
            v_ptat25: 0.0,
            alpha_ptat: 0.0,
            gain_ee: 0.0,
            tgc: 0.0,
            ks_ta: 0.0,
            ks_to: [0.0; 5],
            ct: [0; 5],
            resolution_ee: Resolution::Bits18,
            calibration_chess: true,
            alpha: [0.0; PIXEL_COUNT],
            offset: [0; PIXEL_COUNT],
            kta: [0.0; PIXEL_COUNT],
            kv: [0.0; 4],
            cp_alpha: [0.0; 2],
            cp_offset: [0.0; 2],
            cp_kta: 0.0,
            cp_kv: 0.0,
            il_chess_c: [0.0; 3],
            broken: [0; MAX_DEVIATING_PIXELS],
            broken_count: 0,
            outlier: [0; MAX_DEVIATING_PIXELS],
            outlier_count: 0,
        }
    }

    /// Pixels the EEPROM marks as dead; their temperatures are meaningless.
    pub fn broken_pixels(&self) -> &[u16] {
        &self.broken[..self.broken_count as usize]
    }

    /// Pixels the EEPROM marks as outside the specified tolerance.
    pub fn outlier_pixels(&self) -> &[u16] {
        &self.outlier[..self.outlier_count as usize]
    }

    /// Whether a pixel is flagged as broken or as an outlier.
    pub fn is_deviating(&self, pixel: usize) -> bool {
        let pixel = pixel as u16;
        self.broken_pixels().contains(&pixel) || self.outlier_pixels().contains(&pixel)
    }

    /// The ADC resolution the part was calibrated at.
    pub fn calibration_resolution(&self) -> Resolution {
        self.resolution_ee
    }

    /// Unpack a raw EEPROM image into calibration constants.
    ///
    /// The layout is the one the datasheet gives in its "restoring the ...
    /// parameters" sections: constants are two's-complement bit fields packed
    /// into words, most of them scaled by a power of two that is itself stored
    /// in the EEPROM. Everything is converted to floats here so that the
    /// per-pixel work in [`Frame::object_temperatures`] is plain arithmetic.
    ///
    /// The order matters in one place: the compensation-pixel sensitivity is
    /// subtracted from every pixel's sensitivity, so it is unpacked first.
    pub fn extract(&mut self, eeprom: &Eeprom) -> Result<(), Error> {
        self.extract_supply_voltage(eeprom);
        self.extract_ambient_temperature(eeprom);
        self.extract_gain_and_resolution(eeprom);
        self.extract_sensitivity_corrections(eeprom);
        self.extract_compensation_pixels(eeprom);
        self.extract_alpha(eeprom);
        self.extract_offset(eeprom);
        self.extract_kta(eeprom);
        self.extract_kv(eeprom);
        self.extract_interleaved_corrections(eeprom);
        self.extract_deviating_pixels(eeprom)
    }

    /// `kVdd` and `Vdd25`, the two constants of the supply-voltage sensor.
    fn extract_supply_voltage(&mut self, eeprom: &Eeprom) {
        self.k_vdd = (sign_extend(eeprom[51] >> 8, 8) * 32) as f32;
        self.vdd25 = ((((eeprom[51] & 0x00FF) as i32) - 256) * 32 - 8192) as f32;
    }

    /// The constants of the two ambient-temperature (PTAT) sensors.
    fn extract_ambient_temperature(&mut self, eeprom: &Eeprom) {
        self.kv_ptat = sign_extend((eeprom[50] & 0xFC00) >> 10, 6) as f32 / 4096.0;
        self.kt_ptat = sign_extend(eeprom[50] & 0x03FF, 10) as f32 / 8.0;
        self.v_ptat25 = eeprom[49] as f32;
        self.alpha_ptat = ((eeprom[16] & 0xF000) >> 12) as f32 / 4.0 + 8.0;
    }

    /// The reference gain and the resolution the part was calibrated at.
    fn extract_gain_and_resolution(&mut self, eeprom: &Eeprom) {
        self.gain_ee = sign_extend(eeprom[48], 16) as f32;
        self.resolution_ee = Resolution::from_raw((eeprom[56] & 0x3000) >> 12);
        // Bit 11 of word 10 is clear on a chess-calibrated part.
        self.calibration_chess = eeprom[10] & 0x0800 == 0;
    }

    /// `TGC`, `KsTa` and the `KsTo` table with its corner temperatures.
    fn extract_sensitivity_corrections(&mut self, eeprom: &Eeprom) {
        self.tgc = sign_extend(eeprom[60] & 0x00FF, 8) as f32 / 32.0;
        self.ks_ta = sign_extend((eeprom[60] & 0xFF00) >> 8, 8) as f32 / 8192.0;

        // The two middle corner temperatures are stored as multiples of a step
        // that is itself in the EEPROM; the outer two are fixed.
        let step = (((eeprom[63] & 0x3000) >> 12) * 10) as i16;
        let ct2 = ((eeprom[63] & 0x00F0) >> 4) as i16 * step;
        let ct3 = ct2 + ((eeprom[63] & 0x0F00) >> 8) as i16 * step;
        self.ct = [-40, 0, ct2, ct3, 400];

        let scale = exp2(((eeprom[63] & 0x000F) + 8) as u32);
        self.ks_to[0] = sign_extend(eeprom[61] & 0x00FF, 8) as f32 / scale;
        self.ks_to[1] = sign_extend((eeprom[61] & 0xFF00) >> 8, 8) as f32 / scale;
        self.ks_to[2] = sign_extend(eeprom[62] & 0x00FF, 8) as f32 / scale;
        self.ks_to[3] = sign_extend((eeprom[62] & 0xFF00) >> 8, 8) as f32 / scale;
        // Fixed by the datasheet for the range above the last corner, which
        // this driver's four-range solution does not reach.
        self.ks_to[4] = -0.0002;
    }

    /// The two compensation pixels, which carry the array's common drift.
    fn extract_compensation_pixels(&mut self, eeprom: &Eeprom) {
        let alpha_scale = exp2((((eeprom[32] & 0xF000) >> 12) + 27) as u32);

        let offset0 = sign_extend(eeprom[58] & 0x03FF, 10);
        // The second compensation pixel is stored as a difference to the first.
        let offset1 = offset0 + sign_extend((eeprom[58] & 0xFC00) >> 10, 6);
        self.cp_offset = [offset0 as f32, offset1 as f32];

        let alpha0 = sign_extend(eeprom[57] & 0x03FF, 10) as f32 / alpha_scale;
        let alpha1 = (1.0 + sign_extend((eeprom[57] & 0xFC00) >> 10, 6) as f32 / 128.0) * alpha0;
        self.cp_alpha = [alpha0, alpha1];

        let kta_scale = exp2((((eeprom[56] & 0x00F0) >> 4) + 8) as u32);
        self.cp_kta = sign_extend(eeprom[59] & 0x00FF, 8) as f32 / kta_scale;

        let kv_scale = exp2(((eeprom[56] & 0x0F00) >> 8) as u32);
        self.cp_kv = sign_extend((eeprom[59] & 0xFF00) >> 8, 8) as f32 / kv_scale;
    }

    /// Per-pixel sensitivity.
    ///
    /// Stored as a reference value plus a per-row, a per-column and a
    /// per-pixel remainder, each with its own scale, so that 768 sensitivities
    /// fit in the EEPROM. The compensation pixels' average sensitivity,
    /// weighted by `TGC`, is subtracted here because
    /// [`Frame::object_temperatures`] subtracts their signal from every pixel.
    fn extract_alpha(&mut self, eeprom: &Eeprom) {
        let remainder_scale = (eeprom[32] & 0x000F) as u32;
        let column_scale = ((eeprom[32] & 0x00F0) >> 4) as u32;
        let row_scale = ((eeprom[32] & 0x0F00) >> 8) as u32;
        let scale = exp2((((eeprom[32] & 0xF000) >> 12) + 30) as u32);
        let reference = eeprom[33] as i32;

        let mut rows = [0i32; ROWS];
        let mut columns = [0i32; COLUMNS];
        // The 24 row corrections come first, six words of four nibbles, and
        // the 32 column corrections follow in eight words. This is the same
        // order the offset uses at words 18 and 24.
        unpack_nibbles(&eeprom[34..40], &mut rows);
        unpack_nibbles(&eeprom[40..48], &mut columns);

        let compensation = self.tgc * (self.cp_alpha[0] + self.cp_alpha[1]) / 2.0;

        for pixel in 0..PIXEL_COUNT {
            let remainder = sign_extend((eeprom[64 + pixel] & 0x03F0) >> 4, 6) << remainder_scale;
            let raw = reference
                + (rows[pixel / COLUMNS] << row_scale)
                + (columns[pixel % COLUMNS] << column_scale)
                + remainder;
            self.alpha[pixel] = raw as f32 / scale - compensation;
        }
    }

    /// Per-pixel offset, packed the same way as the sensitivity.
    fn extract_offset(&mut self, eeprom: &Eeprom) {
        let remainder_scale = (eeprom[16] & 0x000F) as u32;
        let column_scale = ((eeprom[16] & 0x00F0) >> 4) as u32;
        let row_scale = ((eeprom[16] & 0x0F00) >> 8) as u32;
        let reference = sign_extend(eeprom[17], 16);

        let mut rows = [0i32; ROWS];
        let mut columns = [0i32; COLUMNS];
        unpack_nibbles(&eeprom[18..24], &mut rows);
        unpack_nibbles(&eeprom[24..32], &mut columns);

        for pixel in 0..PIXEL_COUNT {
            let remainder = sign_extend((eeprom[64 + pixel] & 0xFC00) >> 10, 6) << remainder_scale;
            self.offset[pixel] = reference
                + (rows[pixel / COLUMNS] << row_scale)
                + (columns[pixel % COLUMNS] << column_scale)
                + remainder;
        }
    }

    /// Per-pixel ambient-temperature coefficient of the offset.
    ///
    /// A base value per quadrant plus a 3-bit per-pixel remainder.
    fn extract_kta(&mut self, eeprom: &Eeprom) {
        let base = [
            sign_extend((eeprom[54] & 0xFF00) >> 8, 8),
            sign_extend((eeprom[55] & 0xFF00) >> 8, 8),
            sign_extend(eeprom[54] & 0x00FF, 8),
            sign_extend(eeprom[55] & 0x00FF, 8),
        ];
        let scale = exp2((((eeprom[56] & 0x00F0) >> 4) + 8) as u32);
        let remainder_scale = (eeprom[56] & 0x000F) as u32;

        for pixel in 0..PIXEL_COUNT {
            let remainder = sign_extend((eeprom[64 + pixel] & 0x000E) >> 1, 3) << remainder_scale;
            self.kta[pixel] = (base[quadrant(pixel)] + remainder) as f32 / scale;
        }
    }

    /// Supply-voltage coefficient of the offset, one value per quadrant.
    fn extract_kv(&mut self, eeprom: &Eeprom) {
        let scale = exp2(((eeprom[56] & 0x0F00) >> 8) as u32);
        let raw = [
            sign_extend((eeprom[52] & 0xF000) >> 12, 4),
            sign_extend((eeprom[52] & 0x00F0) >> 4, 4),
            sign_extend((eeprom[52] & 0x0F00) >> 8, 4),
            sign_extend(eeprom[52] & 0x000F, 4),
        ];
        for (kv, raw) in self.kv.iter_mut().zip(raw) {
            *kv = raw as f32 / scale;
        }
    }

    /// The three corrections that make an interleaved read-out agree with the
    /// chess-pattern calibration.
    fn extract_interleaved_corrections(&mut self, eeprom: &Eeprom) {
        self.il_chess_c[0] = sign_extend(eeprom[53] & 0x003F, 6) as f32 / 16.0;
        self.il_chess_c[1] = sign_extend((eeprom[53] & 0x07C0) >> 6, 5) as f32 / 2.0;
        self.il_chess_c[2] = sign_extend((eeprom[53] & 0xF800) >> 11, 5) as f32 / 8.0;
    }

    /// Collect the pixels the EEPROM flags as unusable.
    ///
    /// A pixel whose calibration word is zero is dead; bit 0 of the word marks
    /// a pixel that is outside the specified tolerance. More than
    /// [`MAX_DEVIATING_PIXELS`] of either kind means the part is faulty.
    fn extract_deviating_pixels(&mut self, eeprom: &Eeprom) -> Result<(), Error> {
        self.broken_count = 0;
        self.outlier_count = 0;
        let mut broken = 0usize;
        let mut outlier = 0usize;

        for pixel in 0..PIXEL_COUNT {
            let word = eeprom[64 + pixel];
            if word == 0 {
                if broken < MAX_DEVIATING_PIXELS {
                    self.broken[broken] = pixel as u16;
                    self.broken_count += 1;
                }
                broken += 1;
            } else if word & 0x0001 != 0 {
                if outlier < MAX_DEVIATING_PIXELS {
                    self.outlier[outlier] = pixel as u16;
                    self.outlier_count += 1;
                }
                outlier += 1;
            }
        }

        if broken > MAX_DEVIATING_PIXELS || outlier > MAX_DEVIATING_PIXELS {
            return Err(Error::TooManyDeviatingPixels(broken + outlier));
        }
        Ok(())
    }
}

impl Default for Parameters {
    fn default() -> Self {
        Parameters::zeroed()
    }
}

/// Unpack a run of signed 4-bit fields, four per EEPROM word, into `out`.
///
/// The per-row and per-column corrections of both the sensitivity and the
/// offset are stored this way, in ascending nibble order.
fn unpack_nibbles(words: &[u16], out: &mut [i32]) {
    for (index, value) in out.iter_mut().enumerate() {
        let word = words[index / 4];
        let nibble = (word >> (4 * (index % 4))) & 0x000F;
        *value = sign_extend(nibble, 4);
    }
}

/// One read-out of the measurement RAM: half the pixels plus the auxiliary
/// words the compensation needs.
///
/// About 1.7 kB, so it belongs in a `static` rather than on a task's stack.
pub struct Frame {
    ram: [u16; RAM_WORDS],
    /// Control register 1 as it was when the frame was read; the compensation
    /// needs the resolution and the pattern that produced these counts.
    control: u16,
    /// Which half of the array these counts belong to, 0 or 1.
    subpage: u8,
}

impl Frame {
    /// An empty frame, to be filled in by [`Mlx90640::read_frame`].
    pub const fn new() -> Self {
        Frame {
            ram: [0; RAM_WORDS],
            control: 0,
            subpage: 0,
        }
    }

    /// Which half of the array this read-out refreshed, 0 or 1.
    pub fn subpage(&self) -> u8 {
        self.subpage
    }

    /// The settings that were in force when the counts were taken.
    pub fn configuration(&self) -> Configuration {
        Configuration::from_control(self.control)
    }

    /// A raw pixel word, straight out of the device's RAM.
    ///
    /// Only the pixels of [`Frame::subpage`] were refreshed by this read-out.
    pub fn raw_pixel(&self, pixel: usize) -> i32 {
        sign_extend(self.ram[pixel], 16)
    }

    /// Supply voltage during the measurement, in volts.
    ///
    /// Reading at a resolution other than the calibrated one scales the counts
    /// by a power of two, which is corrected for here.
    pub fn supply_voltage(&self, parameters: &Parameters) -> f32 {
        let measured = Resolution::from_raw(
            (self.control & CONTROL1_RESOLUTION_MASK) >> CONTROL1_RESOLUTION_SHIFT,
        );
        let correction = exp2(parameters.resolution_ee as u32) / exp2(measured as u32);
        (correction * sign_extend(self.ram[RAM_INDEX_VDD], 16) as f32 - parameters.vdd25)
            / parameters.k_vdd
            + NOMINAL_VDD
    }

    /// Temperature of the sensor die itself, in degrees Celsius.
    ///
    /// This is the reference every pixel's offset is compensated against, and
    /// it runs warmer than the room the sensor sits in.
    pub fn ambient_temperature(&self, parameters: &Parameters) -> f32 {
        let vdd = self.supply_voltage(parameters);
        let ptat = sign_extend(self.ram[RAM_INDEX_PTAT], 16) as f32;
        let ptat_art = sign_extend(self.ram[RAM_INDEX_PTAT_ART], 16) as f32;

        // The two PTAT sensors are combined into a ratio so that a common gain
        // drift cancels out; 2^18 puts it back into the EEPROM's units.
        let combined = (ptat / (ptat * parameters.alpha_ptat + ptat_art)) * exp2(18);
        let compensated = combined / (1.0 + parameters.kv_ptat * (vdd - NOMINAL_VDD));
        (compensated - parameters.v_ptat25) / parameters.kt_ptat + NOMINAL_TA
    }

    /// Ratio between the calibrated and the measured gain.
    ///
    /// Every raw count is multiplied by this before anything else.
    pub fn gain(&self, parameters: &Parameters) -> f32 {
        parameters.gain_ee / sign_extend(self.ram[RAM_INDEX_GAIN], 16) as f32
    }

    /// Convert this read-out into object temperatures in degrees Celsius.
    ///
    /// Only the pixels belonging to [`Frame::subpage`] are written; the rest of
    /// `out` is left as it was, so alternating subpages accumulate into a
    /// complete image. `out` is indexed row-major, 32 pixels per row.
    ///
    /// `emissivity` is that of the observed surface (about 0.95 for most
    /// non-metals). `reflected_celsius` is the temperature of the surroundings
    /// whose radiation the surface reflects; with nothing better to go on, the
    /// datasheet suggests [`Frame::ambient_temperature`] minus 8 degrees.
    ///
    /// The pixel signal is first freed of gain, offset, ambient-temperature and
    /// supply-voltage drift and of the compensation pixel's share, then
    /// converted to a temperature by inverting the fourth-power radiation law.
    /// The result of that inversion selects one of four correction ranges,
    /// which is then applied to get the returned value.
    pub fn object_temperatures(
        &self,
        parameters: &Parameters,
        emissivity: f32,
        reflected_celsius: f32,
        out: &mut [f32; PIXEL_COUNT],
    ) {
        let vdd = self.supply_voltage(parameters);
        let ambient = self.ambient_temperature(parameters);
        let gain = self.gain(parameters);

        let ambient_kelvin4 = fourth_power(ambient + KELVIN_OFFSET);
        let reflected_kelvin4 = fourth_power(reflected_celsius + KELVIN_OFFSET);
        // The radiation the surface reflects rather than emits, removed from
        // the model up front so the per-pixel loop only adds it back.
        let background = reflected_kelvin4 - (reflected_kelvin4 - ambient_kelvin4) / emissivity;

        let ambient_drift = 1.0 + parameters.ks_ta * (ambient - NOMINAL_TA);
        let offset_ambient_drift = ambient - NOMINAL_TA;
        let offset_supply_drift = vdd - NOMINAL_VDD;

        // The sensitivity the final solution uses is the calibrated one carried
        // across the corner temperatures of the ranges below the pixel's own.
        // Range 1, which every room temperature falls in, is the range the part
        // was calibrated in and needs no carrying; the others accumulate the
        // slope of each range they cross.
        let range2_correction = 1.0 + parameters.ks_to[1] * parameters.ct[2] as f32;
        let range_correction = [
            1.0 / (1.0 + parameters.ks_to[0] * 40.0),
            1.0,
            range2_correction,
            range2_correction
                * (1.0 + parameters.ks_to[2] * (parameters.ct[3] - parameters.ct[2]) as f32),
        ];

        let chess = self.control & CONTROL1_CHESS_PATTERN_MASK != 0;
        // Reading in a pattern the part was not calibrated in leaves a
        // per-pattern residue that the ilChess constants remove.
        let needs_pattern_correction = chess != parameters.calibration_chess;

        // Signal of the compensation pixel of this subpage, compensated the
        // same way as an ordinary pixel.
        let compensation_index = if self.subpage == 0 {
            RAM_INDEX_CP_SUBPAGE0
        } else {
            RAM_INDEX_CP_SUBPAGE1
        };
        let mut compensation_offset = parameters.cp_offset[self.subpage as usize];
        if needs_pattern_correction && self.subpage == 1 {
            compensation_offset += parameters.il_chess_c[0];
        }
        let compensation = sign_extend(self.ram[compensation_index], 16) as f32 * gain
            - compensation_offset
                * (1.0 + parameters.cp_kta * offset_ambient_drift)
                * (1.0 + parameters.cp_kv * offset_supply_drift);

        for pixel in 0..PIXEL_COUNT {
            // Row parity selects the subpage in the interleaved pattern; row
            // parity exclusive-or column parity does it in the chess pattern.
            let row_parity = (pixel / COLUMNS) & 1;
            let pattern = if chess {
                row_parity ^ (pixel & 1)
            } else {
                row_parity
            };
            if pattern as u8 != self.subpage {
                continue;
            }

            let mut signal = sign_extend(self.ram[pixel], 16) as f32 * gain;
            signal -= parameters.offset[pixel] as f32
                * (1.0 + parameters.kta[pixel] * offset_ambient_drift)
                * (1.0 + parameters.kv[quadrant(pixel)] * offset_supply_drift);

            if needs_pattern_correction {
                // Alternates with the row parity, and with the position inside
                // each group of four pixels along the row.
                let index = pixel as i32;
                let group = (index + 2) / 4 - (index + 3) / 4 + (index + 1) / 4 - index / 4;
                let group = group * (1 - 2 * row_parity as i32);
                signal += parameters.il_chess_c[2] * (2.0 * row_parity as f32 - 1.0)
                    - parameters.il_chess_c[1] * group as f32;
            }

            signal -= parameters.tgc * compensation;
            signal /= emissivity;

            let alpha = parameters.alpha[pixel] * ambient_drift;

            // First solution of the radiation law, used only to pick the
            // correction range the pixel falls into.
            let slope = parameters.ks_to[1];
            let correction =
                fourth_root(alpha * alpha * alpha * (signal + alpha * background)) * slope;
            let estimate = fourth_root(
                signal / (alpha * (1.0 - slope * KELVIN_OFFSET) + correction) + background,
            ) - KELVIN_OFFSET;

            let range = if estimate < parameters.ct[1] as f32 {
                0
            } else if estimate < parameters.ct[2] as f32 {
                1
            } else if estimate < parameters.ct[3] as f32 {
                2
            } else {
                3
            };

            let slope = parameters.ks_to[range];
            let scaled = alpha
                * range_correction[range]
                * (1.0 + slope * (estimate - parameters.ct[range] as f32));
            out[pixel] = fourth_root(signal / scaled + background) - KELVIN_OFFSET;
        }
    }
}

impl Default for Frame {
    fn default() -> Self {
        Frame::new()
    }
}

/// `value^4`, the form the radiation law is used in here.
fn fourth_power(value: f32) -> f32 {
    let squared = value * value;
    squared * squared
}

/// MLX90640 attached to an esp-hal I2C bus.
///
/// The transfers are blocking. Nothing here waits for the device: it measures
/// on its own, so a caller polls [`Mlx90640::data_ready`] and can release the
/// shared bus in between.
pub struct Mlx90640<'a, 'd, T> {
    i2c: &'a mut I2C<'d, T, Blocking>,
    address: u8,
}

impl<'a, 'd, T: Instance> Mlx90640<'a, 'd, T> {
    /// Bind the driver to a bus, using the factory-default address.
    pub fn new(i2c: &'a mut I2C<'d, T, Blocking>) -> Self {
        Self::with_address(i2c, DEFAULT_ADDRESS)
    }

    /// Bind the driver to a bus using an explicit 7-bit address.
    pub fn with_address(i2c: &'a mut I2C<'d, T, Blocking>, address: u8) -> Self {
        Self { i2c, address }
    }

    /// Read consecutive 16-bit words starting at the 16-bit address `start`.
    ///
    /// The device auto-increments its address pointer, but the I2C peripheral
    /// refuses transfers longer than 254 bytes, so long blocks are split into
    /// chunks of [`MAX_WORDS_PER_TRANSFER`] words, each addressed on its own.
    pub async fn read_words(&mut self, start: u16, out: &mut [u16]) -> Result<(), Error> {
        let mut read = 0usize;

        while read < out.len() {
            let count = (out.len() - read).min(MAX_WORDS_PER_TRANSFER);
            let mut raw = [0u8; 2 * MAX_WORDS_PER_TRANSFER];
            let raw = &mut raw[..2 * count];
            let address = start + read as u16;

            self.i2c
                .write_read(self.address, &address.to_be_bytes(), raw)?;

            for (word, bytes) in out[read..read + count].iter_mut().zip(raw.chunks_exact(2)) {
                *word = u16::from_be_bytes([bytes[0], bytes[1]]);
            }
            read += count;

            // The blocking driver holds the CPU for the whole chunk. Pause
            // briefly between chunks of a multi-hundred-byte dump so blink,
            // Wi-Fi and the other sensor tasks can run.
            if read < out.len() {
                Timer::after_millis(1).await;
            }
        }

        Ok(())
    }

    /// Read a single 16-bit register.
    pub async fn read_word(&mut self, register: u16) -> Result<u16, Error> {
        let mut word = [0u16; 1];
        self.read_words(register, &mut word).await?;
        Ok(word[0])
    }

    /// Write a single 16-bit register.
    async fn write_word(&mut self, register: u16, value: u16) -> Result<(), Error> {
        let [register_high, register_low] = register.to_be_bytes();
        let [value_high, value_low] = value.to_be_bytes();
        self.i2c.write(
            self.address,
            &[register_high, register_low, value_high, value_low],
        )?;
        Ok(())
    }

    /// Write a register and confirm it took the value.
    ///
    /// Used for the control register, whose bits the device does not change on
    /// its own, so a mismatch is a real fault. The status register is not
    /// checked this way: the device sets its data-ready bit again as soon as it
    /// finishes a subpage, so a read-back there proves nothing.
    async fn write_word_verified(&mut self, register: u16, value: u16) -> Result<(), Error> {
        self.write_word(register, value).await?;
        let read = self.read_word(register).await?;
        if read != value {
            return Err(Error::WriteVerification {
                register,
                written: value,
                read,
            });
        }
        Ok(())
    }

    /// Read and decode the status register.
    pub async fn status(&mut self) -> Result<Status, Error> {
        Ok(Status::from_raw(self.read_word(REG_STATUS).await?))
    }

    /// Whether the device has finished a subpage since the flag was last
    /// cleared.
    ///
    /// One word on the bus, so this is what a task should poll while the frame
    /// is being measured; the shared bus can be released in between.
    pub async fn data_ready(&mut self) -> Result<bool, Error> {
        Ok(self.status().await?.data_ready)
    }

    /// Clear the data-ready flag, leaving overwriting enabled.
    pub async fn clear_data_ready(&mut self) -> Result<(), Error> {
        self.write_word(REG_STATUS, STATUS_ACKNOWLEDGE).await
    }

    /// Read the settings currently programmed into the device.
    pub async fn configuration(&mut self) -> Result<Configuration, Error> {
        Ok(Configuration::from_control(
            self.read_word(REG_CONTROL1).await?,
        ))
    }

    /// Program the refresh rate, ADC resolution and reading pattern.
    ///
    /// The other bits of the control register, which choose whether the
    /// subpages alternate, are left as they are; the power-on default has them
    /// alternating, which is what [`Frame::object_temperatures`] expects.
    pub async fn set_configuration(&mut self, configuration: &Configuration) -> Result<(), Error> {
        let current = self.read_word(REG_CONTROL1).await?;
        let mask =
            CONTROL1_REFRESH_RATE_MASK | CONTROL1_RESOLUTION_MASK | CONTROL1_CHESS_PATTERN_MASK;
        let updated = (current & !mask) | configuration.control_bits();
        self.write_word_verified(REG_CONTROL1, updated).await
    }

    /// Copy the whole calibration EEPROM into `eeprom`.
    ///
    /// The contents never change, so this is only needed once, at start-up;
    /// [`Parameters::extract`] turns the result into usable constants.
    pub async fn read_eeprom(&mut self, eeprom: &mut Eeprom) -> Result<(), Error> {
        self.read_words(EEPROM_BASE, eeprom).await
    }

    /// Read the subpage the device has just finished into `frame`.
    ///
    /// Returns [`Error::FrameNotReady`] if there is nothing new, so poll
    /// [`Mlx90640::data_ready`] first. The flag is cleared before the RAM is
    /// read and checked again afterwards: if the device finished another
    /// subpage in the meantime it has already started overwriting the RAM, and
    /// the words just read may hold parts of two different subpages, so the
    /// read is repeated. Failing that repeatedly means the refresh rate is too
    /// high for the bus to keep up with, which is
    /// [`Error::FrameOverwritten`].
    pub async fn read_frame(&mut self, frame: &mut Frame) -> Result<(), Error> {
        let mut status = self.status().await?;
        if !status.data_ready {
            return Err(Error::FrameNotReady);
        }

        for _ in 0..FRAME_READ_ATTEMPTS {
            self.clear_data_ready().await?;
            self.read_words(RAM_BASE, &mut frame.ram).await?;
            status = self.status().await?;

            if !status.data_ready {
                frame.control = self.read_word(REG_CONTROL1).await?;
                frame.subpage = status.subpage & 0x01;
                return Ok(());
            }
        }

        Err(Error::FrameOverwritten)
    }

    /// Bring the sensor into a known state and load its calibration.
    ///
    /// The EEPROM is dumped into `eeprom` and unpacked into `parameters`, then
    /// `configuration` is written. `eeprom` is only scratch space: it is a
    /// parameter so that its 1.7 kB can come from wherever the caller keeps
    /// large buffers instead of from the calling task's stack, and it can be
    /// dropped once this returns.
    ///
    /// Afterwards the device is measuring on its own; take images by polling
    /// [`Mlx90640::data_ready`] and calling [`Mlx90640::read_frame`] twice, once
    /// per subpage.
    pub async fn init(
        &mut self,
        configuration: &Configuration,
        eeprom: &mut Eeprom,
        parameters: &mut Parameters,
    ) -> Result<(), Error> {
        self.read_eeprom(eeprom).await?;
        parameters.extract(eeprom)?;
        self.set_configuration(configuration).await
    }
}
