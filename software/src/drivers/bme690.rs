#![allow(dead_code)]

use embassy_time::Timer;
use esp_hal::{
    i2c::{Error as I2cError, Instance, I2C},
    Blocking,
};

/// Factory-default 7-bit address of the BME690 (`SDO` pulled high).
pub const DEFAULT_ADDRESS: u8 = 0x77;

/// First byte of the `field_0` measurement block (`meas_status_0`).
const REG_FIELD0: u8 = 0x1D;
/// Heater resistance target for profile 0; profiles 1..=9 follow consecutively.
const REG_RES_HEAT0: u8 = 0x5A;
/// Heater-on duration for profile 0; profiles 1..=9 follow consecutively.
const REG_GAS_WAIT0: u8 = 0x64;
const REG_CTRL_GAS_0: u8 = 0x70; // heater on/off
const REG_CTRL_GAS_1: u8 = 0x71; // gas conversion enable & heater profile index
const REG_CTRL_HUM: u8 = 0x72; // humidity oversampling settings
const REG_CTRL_MEAS: u8 = 0x74; // temperature & pressure oversampling settings, mode
const REG_CONFIG: u8 = 0x75; // IIR filter & SPI settings
const REG_CHIP_ID: u8 = 0xD0;
const REG_RESET: u8 = 0xE0;

// The factory calibration coefficients are split across three non-contiguous
// register blocks. They are read into one 42-byte array whose indices the
// `IDX_*` constants below refer to.
const REG_COEFF1: u8 = 0x8A;
const COEFF1_LENGTH: usize = 23;
const REG_COEFF2: u8 = 0xE1;
const COEFF2_LENGTH: usize = 14;
const REG_COEFF3: u8 = 0x00;
const COEFF3_LENGTH: usize = 5;
const COEFF_TOTAL_LENGTH: usize = COEFF1_LENGTH + COEFF2_LENGTH + COEFF3_LENGTH;

// Temperature coefficients.
const IDX_PAR_T1_LSB: usize = 31;
const IDX_PAR_T1_MSB: usize = 32;
const IDX_PAR_T2_LSB: usize = 0;
const IDX_PAR_T2_MSB: usize = 1;
const IDX_PAR_T3: usize = 2;

// Pressure coefficients.
const IDX_PAR_P1_LSB: usize = 10;
const IDX_PAR_P1_MSB: usize = 11;
const IDX_PAR_P2_LSB: usize = 12;
const IDX_PAR_P2_MSB: usize = 13;
const IDX_PAR_P3: usize = 14;
const IDX_PAR_P4: usize = 15;
const IDX_PAR_P5_LSB: usize = 4;
const IDX_PAR_P5_MSB: usize = 5;
const IDX_PAR_P6_LSB: usize = 6;
const IDX_PAR_P6_MSB: usize = 7;
const IDX_PAR_P7: usize = 8;
const IDX_PAR_P8: usize = 9;
const IDX_PAR_P9_LSB: usize = 18;
const IDX_PAR_P9_MSB: usize = 19;
const IDX_PAR_P10: usize = 20;
const IDX_PAR_P11: usize = 21;

// Humidity coefficients. `par_h1` and `par_h5` are 12-bit values that share
// byte 24: `par_h5` takes its high nibble, `par_h1` its low nibble.
const IDX_PAR_H1_MSB: usize = 25;
const IDX_PAR_H1_H5_SHARED: usize = 24;
const IDX_PAR_H5_MSB: usize = 23;
const IDX_PAR_H2: usize = 26;
const IDX_PAR_H3: usize = 28;
const IDX_PAR_H4: usize = 27;
const IDX_PAR_H6: usize = 29;

// Gas-heater coefficients.
const IDX_PAR_G1: usize = 35;
const IDX_PAR_G2_LSB: usize = 33;
const IDX_PAR_G2_MSB: usize = 34;
const IDX_PAR_G3: usize = 36;
const IDX_RES_HEAT_VAL: usize = 37;
const IDX_RES_HEAT_RANGE: usize = 39;

/// `res_heat_range[1:0]` occupies bits 5:4 of its coefficient byte.
const RES_HEAT_RANGE_MASK: u8 = 0x30;
const RES_HEAT_RANGE_SHIFT: u8 = 4;

/// Largest value a 12-bit two's-complement coefficient can hold before it
/// represents a negative number.
const COEFF_12BIT_SIGN_THRESHOLD: i16 = 2047;
/// Amount subtracted to sign-extend a 12-bit two's-complement coefficient.
const COEFF_12BIT_RANGE: i16 = 4096;

// `mode[1:0]` field of `REG_CTRL_MEAS`, occupying bits 1:0 of the register.
const CTRL_MEAS_MODE_MASK: u8 = 0b0000_0011;
// `heat_off` bit of `REG_CTRL_GAS_0`, occupying bit 3 of the register.
const CTRL_GAS_0_HEAT_OFF_MASK: u8 = 0b0000_1000;
// `run_gas` bit of `REG_CTRL_GAS_1`, occupying bit 5 of the register.
//
// The older BME68x family uses a two-bit field here; on the BME690 it is a
// single enable bit.
const CTRL_GAS_1_RUN_GAS_MASK: u8 = 0b0010_0000;
// `nb_conv[3:0]` field of `REG_CTRL_GAS_1`, occupying bits 3:0 of the register.
const CTRL_GAS_1_NB_CONV_MASK: u8 = 0b0000_1111;

/// Highest selectable heater profile index (`nb_conv` is 4 bits but only
/// profiles 0..=9 have `res_heat`/`gas_wait` registers).
const MAX_HEATER_PROFILE: u8 = 9;
/// The heater target temperature is capped at 400 °C by the datasheet.
const MAX_HEATER_TEMPERATURE_CELSIUS: u16 = 400;
/// Heater durations at or above this value select the maximum `gas_wait`
/// encoding (`0xFF`).
const MAX_HEATER_DURATION_MS: u16 = 0xFC0;

// `osrs_h[2:0]` field of `REG_CTRL_HUM`, occupying bits 2:0 of the register.
const CTRL_HUM_OSRS_H_SHIFT: u8 = 0;
// `osrs_t[2:0]` field of `REG_CTRL_MEAS`, occupying bits 7:5 of the register.
const CTRL_MEAS_OSRS_T_SHIFT: u8 = 5;
// `osrs_p[2:0]` field of `REG_CTRL_MEAS`, occupying bits 4:2 of the register.
const CTRL_MEAS_OSRS_P_SHIFT: u8 = 2;

/// Oversampling setting applied to a humidity, temperature, or pressure
/// measurement channel.
///
/// Higher oversampling reduces noise at the cost of measurement time. It is
/// independent of the IIR filter (`IirFilterCoefficient`), which instead
/// smooths consecutive temperature/pressure readings together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Oversampling {
    /// Measurement skipped; the channel is disabled and returns no data.
    Skipped,
    X1,
    X2,
    X4,
    X8,
    X16,
}

impl Oversampling {
    /// The 3-bit `osrs[2:0]` field value for this oversampling setting.
    fn raw(self) -> u8 {
        match self {
            Oversampling::Skipped => 0b000,
            Oversampling::X1 => 0b001,
            Oversampling::X2 => 0b010,
            Oversampling::X4 => 0b011,
            Oversampling::X8 => 0b100,
            Oversampling::X16 => 0b101,
        }
    }

    /// Decode a 3-bit `osrs[2:0]` field value.
    ///
    /// Needed because BSEC hands its oversampling choices back as those raw
    /// codes. Returns `None` for the two encodings the datasheet leaves
    /// undefined, rather than guessing at a setting whose measurement time
    /// would then be wrong.
    pub fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0b000 => Some(Oversampling::Skipped),
            0b001 => Some(Oversampling::X1),
            0b010 => Some(Oversampling::X2),
            0b011 => Some(Oversampling::X4),
            0b100 => Some(Oversampling::X8),
            0b101 => Some(Oversampling::X16),
            _ => None,
        }
    }

    /// Number of ADC conversion cycles this setting costs.
    ///
    /// A skipped channel costs nothing; otherwise the count equals the
    /// oversampling multiplier. Used to work out how long a forced-mode
    /// measurement takes.
    fn measurement_cycles(self) -> u32 {
        match self {
            Oversampling::Skipped => 0,
            Oversampling::X1 => 1,
            Oversampling::X2 => 2,
            Oversampling::X4 => 4,
            Oversampling::X8 => 8,
            Oversampling::X16 => 16,
        }
    }
}

// `filter[2:0]` field of `REG_CONFIG`, occupying bits 4:2 of the register.
const CONFIG_FILTER_SHIFT: u8 = 2;

/// IIR filter coefficient applied to the temperature and pressure ADC output.
///
/// The filter smooths out short-term perturbations (e.g. draughts, door/window
/// slams) in the temperature and pressure signals; it has no effect on
/// humidity or gas resistance. Higher coefficients respond more slowly but
/// with less noise. See `REG_CONFIG` (0x75), bits 4:2, in the datasheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IirFilterCoefficient {
    /// Filter disabled; the raw ADC output is used directly.
    Off,
    Coefficient1,
    Coefficient3,
    Coefficient7,
    Coefficient15,
    Coefficient31,
    Coefficient63,
    Coefficient127,
}

impl IirFilterCoefficient {
    /// The 3-bit `filter[2:0]` field value for this coefficient.
    fn raw(self) -> u8 {
        match self {
            IirFilterCoefficient::Off => 0b000,
            IirFilterCoefficient::Coefficient1 => 0b001,
            IirFilterCoefficient::Coefficient3 => 0b010,
            IirFilterCoefficient::Coefficient7 => 0b011,
            IirFilterCoefficient::Coefficient15 => 0b100,
            IirFilterCoefficient::Coefficient31 => 0b101,
            IirFilterCoefficient::Coefficient63 => 0b110,
            IirFilterCoefficient::Coefficient127 => 0b111,
        }
    }
}

const RESET_COMMAND: u8 = 0xB6;
const RESET_DELAY_MS: u64 = 2;
const EXPECTED_CHIP_ID: u8 = 0x61;

/// Largest number of register/value pairs a single burst write may carry.
const MAX_REGISTER_WRITES: usize = 8;

/// Size of the `field_0` result block, from `meas_status_0` (0x1D) through
/// `gas_r_lsb_0` (0x2D).
const FIELD_LENGTH: usize = 17;
// Byte offsets within that block.
const FIELD_STATUS: usize = 0; // meas_status_0
const FIELD_MEASUREMENT_INDEX: usize = 1; // sub_meas_index_0
const FIELD_PRESSURE: usize = 2; // press_msb_0 / _lsb_0 / _xlsb_0
const FIELD_TEMPERATURE: usize = 5; // temp_msb_0 / _lsb_0 / _xlsb_0
const FIELD_HUMIDITY: usize = 8; // hum_msb_0 / _lsb_0
const FIELD_GAS_MSB: usize = 15; // gas_r_msb_0
const FIELD_GAS_LSB: usize = 16; // gas_r_lsb_0

// `meas_status_0` bit fields.
const FIELD_STATUS_NEW_DATA_MASK: u8 = 0b1000_0000;
const FIELD_STATUS_GAS_INDEX_MASK: u8 = 0b0000_1111;

// `gas_r_lsb_0` bit fields: the low two bits of the gas ADC result sit in bits
// 7:6, above the validity flags and the range index.
const GAS_LSB_RESISTANCE_SHIFT: u8 = 6;
const GAS_LSB_VALID_MASK: u8 = 0b0010_0000;
const GAS_LSB_HEAT_STABLE_MASK: u8 = 0b0001_0000;
const GAS_LSB_RANGE_MASK: u8 = 0b0000_1111;

// `gas_wait_x` splits into a 6-bit count and a 2-bit multiplier exponent.
const GAS_WAIT_COUNT_MAX: u16 = 0x3F;
const GAS_WAIT_MULTIPLIER_SHIFT: u8 = 6;

/// Duration of one ADC conversion cycle, in microseconds.
const MEASUREMENT_CYCLE_US: u32 = 1963;
/// Fixed overhead for switching between the temperature, pressure and
/// humidity channels, in microseconds.
const TPH_SWITCHING_US: u32 = 477 * 4;
/// Fixed overhead of the gas conversion that follows the T/P/H channels, in
/// microseconds.
const GAS_MEASUREMENT_US: u32 = 477 * 5;
/// Time the sensor needs to leave sleep mode, in microseconds.
const WAKE_UP_US: u32 = 1000;

/// Delay between polls while waiting for the sensor to fall back to sleep.
const MODE_POLL_INTERVAL_MS: u64 = 10;
/// Number of polls before giving up on a mode change.
const MODE_POLL_ATTEMPTS: u8 = 10;
/// Delay between polls while waiting for `new_data_0` to be set.
const FIELD_POLL_INTERVAL_MS: u64 = 10;
/// Number of polls before giving up on a measurement result.
const FIELD_POLL_ATTEMPTS: u8 = 5;

/// Operating mode written to the `mode[1:0]` field of `REG_CTRL_MEAS` (0x74).
///
/// The field also encodes parallel (0b10) and sequential (0b11) mode, neither
/// of which this driver implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// No measurements are taken; all registers stay accessible. The sensor
    /// returns here on its own after a forced-mode measurement completes.
    Sleep,
    /// Perform exactly one T/P/H (+ gas) measurement, then return to sleep.
    Forced,
}

impl Mode {
    /// The 2-bit `mode[1:0]` field value for this mode.
    fn raw(self) -> u8 {
        match self {
            Mode::Sleep => 0b00,
            Mode::Forced => 0b01,
        }
    }
}

/// Oversampling and IIR filter settings applied to every measurement.
///
/// Held by the caller rather than the driver because the driver is rebound to
/// the shared bus for each transaction and keeps no state of its own. The
/// settings are also what determines how long a forced-mode measurement takes,
/// which the caller needs in order to wait for the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Configuration {
    pub humidity_oversampling: Oversampling,
    pub temperature_oversampling: Oversampling,
    pub pressure_oversampling: Oversampling,
    pub iir_filter: IirFilterCoefficient,
}

impl Configuration {
    /// How long one forced-mode T/P/H + gas conversion takes, in microseconds,
    /// excluding the heater-on time.
    ///
    /// This is the sum of one conversion cycle per oversampled sample across
    /// all three channels, the fixed channel-switching and gas-conversion
    /// overheads, and the wake-up time out of sleep mode.
    pub fn measurement_duration_us(&self) -> u32 {
        let cycles = self.temperature_oversampling.measurement_cycles()
            + self.pressure_oversampling.measurement_cycles()
            + self.humidity_oversampling.measurement_cycles();

        cycles * MEASUREMENT_CYCLE_US + TPH_SWITCHING_US + GAS_MEASUREMENT_US + WAKE_UP_US
    }
}

/// Per-chip factory trimming values.
///
/// Burned into the sensor during production and unaffected by a soft reset, so
/// they only have to be read once per power cycle. The driver is rebound to
/// the shared bus for every transaction and keeps no state, so the caller owns
/// this and passes it back in when compensating a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Calibration {
    par_t1: u16,
    par_t2: u16,
    par_t3: i8,
    par_p1: u16,
    par_p2: u16,
    par_p3: i8,
    par_p4: i8,
    par_p5: i16,
    par_p6: i16,
    par_p7: i8,
    par_p8: i8,
    par_p9: i16,
    par_p10: i8,
    par_p11: i8,
    par_h1: i16,
    par_h2: i8,
    par_h3: u8,
    par_h4: i8,
    par_h5: i16,
    par_h6: u8,
    par_g1: i8,
    par_g2: i16,
    par_g3: i8,
    res_heat_range: u8,
    res_heat_val: i8,
}

impl Calibration {
    /// Decode the 42-byte coefficient block.
    fn from_bytes(raw: &[u8; COEFF_TOTAL_LENGTH]) -> Self {
        // `par_h5` takes the high nibble of the shared byte, `par_h1` the low
        // nibble; both are 12-bit two's-complement values.
        let par_h5 = sign_extend_12bit(
            ((raw[IDX_PAR_H5_MSB] as i16) << 4) | (raw[IDX_PAR_H1_H5_SHARED] >> 4) as i16,
        );
        let par_h1 = sign_extend_12bit(
            ((raw[IDX_PAR_H1_MSB] as i16) << 4) | (raw[IDX_PAR_H1_H5_SHARED] & 0x0F) as i16,
        );

        Self {
            par_t1: u16::from_le_bytes([raw[IDX_PAR_T1_LSB], raw[IDX_PAR_T1_MSB]]),
            par_t2: u16::from_le_bytes([raw[IDX_PAR_T2_LSB], raw[IDX_PAR_T2_MSB]]),
            par_t3: raw[IDX_PAR_T3] as i8,
            par_p1: u16::from_le_bytes([raw[IDX_PAR_P1_LSB], raw[IDX_PAR_P1_MSB]]),
            par_p2: u16::from_le_bytes([raw[IDX_PAR_P2_LSB], raw[IDX_PAR_P2_MSB]]),
            par_p3: raw[IDX_PAR_P3] as i8,
            par_p4: raw[IDX_PAR_P4] as i8,
            par_p5: i16::from_le_bytes([raw[IDX_PAR_P5_LSB], raw[IDX_PAR_P5_MSB]]),
            par_p6: i16::from_le_bytes([raw[IDX_PAR_P6_LSB], raw[IDX_PAR_P6_MSB]]),
            par_p7: raw[IDX_PAR_P7] as i8,
            par_p8: raw[IDX_PAR_P8] as i8,
            par_p9: i16::from_le_bytes([raw[IDX_PAR_P9_LSB], raw[IDX_PAR_P9_MSB]]),
            par_p10: raw[IDX_PAR_P10] as i8,
            par_p11: raw[IDX_PAR_P11] as i8,
            par_h1,
            par_h2: raw[IDX_PAR_H2] as i8,
            par_h3: raw[IDX_PAR_H3],
            par_h4: raw[IDX_PAR_H4] as i8,
            par_h5,
            par_h6: raw[IDX_PAR_H6],
            par_g1: raw[IDX_PAR_G1] as i8,
            par_g2: i16::from_le_bytes([raw[IDX_PAR_G2_LSB], raw[IDX_PAR_G2_MSB]]),
            par_g3: raw[IDX_PAR_G3] as i8,
            res_heat_range: (raw[IDX_RES_HEAT_RANGE] & RES_HEAT_RANGE_MASK) >> RES_HEAT_RANGE_SHIFT,
            res_heat_val: raw[IDX_RES_HEAT_VAL] as i8,
        }
    }

    /// Convert a heater target temperature into a `res_heat` register value.
    ///
    /// `ambient_celsius` is the temperature the sensor is sitting in; the
    /// heater is driven to `target_celsius` *relative* to it, so a wrong
    /// ambient value biases the resulting heater temperature. Targets above
    /// 400 °C are capped.
    pub fn resistance_register(&self, target_celsius: u16, ambient_celsius: i8) -> u8 {
        let target = target_celsius.min(MAX_HEATER_TEMPERATURE_CELSIUS) as f32;

        let var1 = (self.par_g1 as f32 / 16.0) + 49.0;
        let var2 = ((self.par_g2 as f32 / 32768.0) * 0.0005) + 0.00235;
        let var3 = self.par_g3 as f32 / 1024.0;
        let var4 = var1 * (1.0 + (var2 * target));
        let var5 = var4 + (var3 * ambient_celsius as f32);

        let resistance = 3.4
            * ((var5
                * (4.0 / (4.0 + self.res_heat_range as f32))
                * (1.0 / (1.0 + (self.res_heat_val as f32 * 0.002))))
                - 25.0);

        // `as` saturates in Rust, so an out-of-range result clamps instead of
        // wrapping around.
        resistance as u8
    }

    /// Turn raw ADC counts into physical units.
    ///
    /// Temperature is compensated first because the pressure and humidity
    /// formulas both take the compensated temperature as an input.
    pub fn compensate(&self, measurement: &Measurement) -> CompensatedMeasurement {
        let temperature_celsius = self.temperature_celsius(measurement.temperature_raw);

        CompensatedMeasurement {
            temperature_celsius: temperature_celsius as f32,
            pressure_pascals: self.pressure_pascals(measurement.pressure_raw, temperature_celsius),
            relative_humidity_percent: self
                .relative_humidity_percent(measurement.relative_humidity_raw, temperature_celsius),
            gas_resistance_ohms: gas_resistance_ohms(
                measurement.gas_resistance_raw,
                measurement.gas_range,
            ),
        }
    }

    /// Compensated temperature in °C.
    ///
    /// A quadratic in the ADC offset from the per-chip zero point `par_t1`.
    /// Computed in `f64` because the squared term reaches 2^48, well beyond
    /// what an `f32` mantissa can represent exactly.
    fn temperature_celsius(&self, temperature_raw: u32) -> f64 {
        let zero_offset = ((self.par_t1 as i32) << 8) as f64;
        let linear = self.par_t2 as f64 / (1u64 << 30) as f64;
        let quadratic = self.par_t3 as f64 / (1u64 << 48) as f64;

        let offset = temperature_raw as f64 - zero_offset;

        (offset * linear) + (offset * offset * quadratic)
    }

    /// Compensated pressure in pascals.
    ///
    /// A cubic in the pressure ADC output whose offset, sensitivity and
    /// non-linearity terms are each themselves cubics in temperature. The
    /// cubic term reaches 2^72, so this must be evaluated in `f64`.
    fn pressure_pascals(&self, pressure_raw: u32, temperature_celsius: f64) -> f32 {
        let offset = (self.par_p1 as u32 * 8) as f64;
        let offset_tk1 = self.par_p2 as f64 / (1u64 << 6) as f64;
        let offset_tk2 = self.par_p3 as f64 / (1u64 << 8) as f64;
        let offset_tk3 = self.par_p4 as f64 / (1u64 << 15) as f64;

        let sensitivity = (self.par_p5 as f64 - (1u64 << 14) as f64) / (1u64 << 20) as f64;
        let sensitivity_tk1 = (self.par_p6 as f64 - (1u64 << 14) as f64) / (1u64 << 29) as f64;
        let sensitivity_tk2 = self.par_p7 as f64 / (1u64 << 32) as f64;
        let sensitivity_tk3 = self.par_p8 as f64 / (1u64 << 37) as f64;

        let nonlinear = self.par_p9 as f64 / (1u64 << 48) as f64;
        let nonlinear_tk = self.par_p10 as f64 / (1u64 << 48) as f64;
        // The divisor is 2^65, which overflows `u64`, so it is split into two
        // factors: 2^(35+30) == 2^35 * 2^30.
        let nonlinear_cubic = self.par_p11 as f64 / ((1u64 << 35) as f64 * (1u64 << 30) as f64);

        let t = temperature_celsius;
        let raw = pressure_raw as f64;

        let offset_term =
            offset + (offset_tk1 * t) + (offset_tk2 * t * t) + (offset_tk3 * t * t * t);
        let linear_term = raw
            * (sensitivity
                + (sensitivity_tk1 * t)
                + (sensitivity_tk2 * t * t)
                + (sensitivity_tk3 * t * t * t));
        let quadratic_term = raw * raw * (nonlinear + (nonlinear_tk * t));
        let cubic_term = raw * raw * raw * nonlinear_cubic;

        (offset_term + linear_term + quadratic_term + cubic_term) as f32
    }

    /// Compensated relative humidity in percent, clamped to 0..=100.
    ///
    /// The humidity ADC output is corrected for its temperature dependence and
    /// then for the sensor's own non-linearity.
    fn relative_humidity_percent(&self, humidity_raw: u16, temperature_celsius: f64) -> f32 {
        // The formula works in the same fixed-point temperature scale the
        // integer implementation uses, not in °C.
        let scaled_temperature = (temperature_celsius * 5120.0) - 76800.0;

        let offset = self.par_h1 as f64 * (1u64 << 6) as f64;
        let sensitivity = self.par_h5 as f64 / (1u64 << 16) as f64;
        let offset_tk = self.par_h2 as f64 / (1u64 << 14) as f64;
        let sensitivity_tk1 = self.par_h4 as f64 / (1u64 << 26) as f64;
        let sensitivity_tk2 = self.par_h3 as f64 / (1u64 << 26) as f64;
        let nonlinear = self.par_h6 as f64 / (1u64 << 19) as f64;

        let corrected = humidity_raw as f64 - (offset + offset_tk * scaled_temperature);
        let scaled = corrected
            * sensitivity
            * (1.0
                + (sensitivity_tk1 * scaled_temperature)
                + (sensitivity_tk1 * sensitivity_tk2 * scaled_temperature * scaled_temperature));
        let humidity = scaled * (1.0 - nonlinear * scaled);

        humidity.clamp(0.0, 100.0) as f32
    }
}

/// Sign-extend a 12-bit two's-complement coefficient held in an `i16`.
fn sign_extend_12bit(value: i16) -> i16 {
    if value > COEFF_12BIT_SIGN_THRESHOLD {
        value - COEFF_12BIT_RANGE
    } else {
        value
    }
}

/// Convert the gas ADC output and its range index into ohms.
///
/// Needs no per-chip calibration: the range index selects a fixed reference
/// that the ADC reading is divided into.
fn gas_resistance_ohms(gas_resistance_raw: u16, gas_range: u8) -> f32 {
    let reference = (262144u32 >> gas_range) as f32;
    let scaled = 4096.0 + ((gas_resistance_raw as i32 - 512) * 3) as f32;

    1_000_000.0 * reference / scaled
}

/// One measurement converted into physical units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompensatedMeasurement {
    pub temperature_celsius: f32,
    pub pressure_pascals: f32,
    pub relative_humidity_percent: f32,
    /// Resistance of the gas-sensing film in ohms.
    ///
    /// This is a raw physical resistance, not an air-quality figure. It drifts
    /// with the film's age and moves with humidity as well as with gas, so it
    /// is only meaningful relative to a baseline tracked over time.
    pub gas_resistance_ohms: f32,
}

/// One entry of the heater profile table (`res_heat_x` / `gas_wait_x`).
///
/// Forced mode runs a single profile, selected by `GasConfig::heater_profile`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaterProfile {
    /// Profile slot, 0..=9.
    pub index: u8,
    /// Heater target temperature in °C, capped at 400 °C.
    pub target_temperature_celsius: u16,
    /// How long the heater stays on before the gas ADC is sampled, in
    /// milliseconds. Encoded with a decreasing resolution, so the value
    /// actually applied is only approximately this long.
    pub duration_ms: u16,
}

/// Gas-sensor and heater enables, spanning `REG_CTRL_GAS_0` (0x70) and
/// `REG_CTRL_GAS_1` (0x71).
///
/// Both registers are written together because the heater profile index lives
/// in the same register as the gas-conversion enable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GasConfig {
    /// Drives the `heat_off` bit (0x70 bit 3) inverted: `true` powers the
    /// heater during a measurement.
    pub heater_enabled: bool,
    /// `run_gas` (0x71 bit 5): append a gas conversion to each T/P/H
    /// measurement. Without it the gas ADC output stays stale.
    pub gas_measurement_enabled: bool,
    /// `nb_conv[3:0]` (0x71 bits 3:0): which heater profile forced mode runs.
    pub heater_profile: u8,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Error {
    Bus(I2cError),
    InvalidChipId(u8),
    WriteTooLong,
    /// A heater profile index above 9 was requested.
    InvalidHeaterProfile(u8),
    /// An `osrs[2:0]` code outside the six the datasheet defines was requested.
    InvalidOversampling(u8),
    /// An operating mode this driver does not implement was requested. Only
    /// sleep and forced mode are supported; parallel and sequential mode are
    /// not.
    UnsupportedOperatingMode(u8),
    /// The sensor did not return to sleep mode, so a new mode could not be
    /// selected.
    ModeChangeTimeout,
    /// The measurement did not finish within the expected time; `new_data_0`
    /// was still clear after the final poll.
    NoNewData,
}

impl From<I2cError> for Error {
    fn from(error: I2cError) -> Self {
        Error::Bus(error)
    }
}

/// One uncompensated measurement, exactly as the ADCs reported it.
///
/// Turning these into °C, Pa, %RH and ohms requires the per-chip compensation
/// coefficients, which this driver does not read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    /// 24-bit pressure ADC output.
    pub pressure_raw: u32,
    /// 24-bit temperature ADC output.
    pub temperature_raw: u32,
    /// 16-bit humidity ADC output.
    pub relative_humidity_raw: u16,
    /// 10-bit gas resistance ADC output.
    pub gas_resistance_raw: u16,
    /// `gas_range_r[3:0]`, the switched range the gas ADC used. Required to
    /// interpret `gas_resistance_raw`.
    pub gas_range: u8,
    /// `gas_valid_r`: the gas conversion actually ran and produced a result.
    pub gas_measurement_valid: bool,
    /// `heat_stab_r`: the heater reached its target temperature in time.
    pub heater_stable: bool,
    /// `gas_meas_index_0`: the heater profile this result came from.
    pub gas_measurement_index: u8,
    /// `sub_meas_index_0`: increments with every measurement, used to order
    /// results in the multi-field modes.
    pub measurement_index: u8,
}

pub struct Bme690<'a, 'd, T> {
    i2c: &'a mut I2C<'d, T, Blocking>,
    address: u8,
}

impl<'a, 'd, T> Bme690<'a, 'd, T>
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

    /// Read one or more consecutive registers starting at `register`.
    ///
    /// The register pointer auto-increments on reads, so a single transfer can
    /// pull a whole block such as the 17-byte `field_0`.
    async fn read_registers(&mut self, register: u8, raw: &mut [u8]) -> Result<(), Error> {
        self.i2c.write_read(self.address, &[register], raw)?;
        Ok(())
    }

    /// Write a single register.
    async fn write_register(&mut self, register: u8, value: u8) -> Result<(), Error> {
        self.i2c.write(self.address, &[register, value])?;
        Ok(())
    }

    /// Write several registers in one I2C transaction.
    ///
    /// Unlike reads, the register pointer does *not* auto-increment on writes,
    /// so the address has to be repeated before every data byte:
    /// `addr0, data0, addr1, data1, ...`. Sending a start address followed by
    /// a run of data bytes would write every byte to that one register.
    async fn write_registers(&mut self, pairs: &[(u8, u8)]) -> Result<(), Error> {
        if pairs.len() > MAX_REGISTER_WRITES {
            return Err(Error::WriteTooLong);
        }

        let mut buffer = [0u8; MAX_REGISTER_WRITES * 2];
        for (index, (register, value)) in pairs.iter().enumerate() {
            buffer[index * 2] = *register;
            buffer[index * 2 + 1] = *value;
        }
        self.i2c.write(self.address, &buffer[..pairs.len() * 2])?;
        Ok(())
    }

    /// Read a register, replace the bits selected by `mask`, and write it back.
    ///
    /// Used wherever a register mixes fields owned by different settings, so
    /// that changing one does not clear the others.
    async fn update_register(&mut self, register: u8, mask: u8, value: u8) -> Result<(), Error> {
        let mut raw = [0u8; 1];
        self.read_registers(register, &mut raw).await?;
        self.write_register(register, (raw[0] & !mask) | (value & mask))
            .await
    }

    /// Read the chip identification register (0xD0), always 0x61 on the BME690.
    pub async fn chip_id(&mut self) -> Result<u8, Error> {
        let mut raw = [0u8; 1];
        self.read_registers(REG_CHIP_ID, &mut raw).await?;
        Ok(raw[0])
    }

    pub async fn reset(&mut self) -> Result<(), Error> {
        self.write_register(REG_RESET, RESET_COMMAND).await?;
        Timer::after_millis(RESET_DELAY_MS).await;
        Ok(())
    }

    /// Set the humidity, temperature, and pressure oversampling settings.
    ///
    /// Per the datasheet, `REG_CTRL_HUM` (0x72) only takes effect once
    /// `REG_CTRL_MEAS` (0x74) is written afterwards, so the humidity write
    /// happens first. `REG_CTRL_MEAS` also carries `mode[1:0]`, which is
    /// preserved.
    pub async fn set_oversampling(
        &mut self,
        humidity_oversampling: Oversampling,
        temperature_oversampling: Oversampling,
        pressure_oversampling: Oversampling,
    ) -> Result<(), Error> {
        self.write_register(
            REG_CTRL_HUM,
            humidity_oversampling.raw() << CTRL_HUM_OSRS_H_SHIFT,
        )
        .await?;

        self.update_register(
            REG_CTRL_MEAS,
            !CTRL_MEAS_MODE_MASK,
            temperature_oversampling.raw() << CTRL_MEAS_OSRS_T_SHIFT
                | pressure_oversampling.raw() << CTRL_MEAS_OSRS_P_SHIFT,
        )
        .await?;

        Ok(())
    }

    /// Set the IIR filter coefficient applied to the temperature (and
    /// pressure) ADC output.
    ///
    /// Only `filter[2:0]` is touched; the `odr[2:0]` field sharing this
    /// register, and `spi_3w_en` (bit 0), are left alone. The driver only
    /// speaks I2C, so `spi_3w_en` stays at its reset value of 0 anyway.
    pub async fn set_iir_filter(&mut self, filter: IirFilterCoefficient) -> Result<(), Error> {
        self.update_register(
            REG_CONFIG,
            0b111 << CONFIG_FILTER_SHIFT,
            filter.raw() << CONFIG_FILTER_SHIFT,
        )
        .await
    }

    /// Select the operating mode.
    ///
    /// The datasheet requires the sensor to be in sleep mode before a new mode
    /// is written, so any measurement still in flight is first stopped and
    /// waited out. Selecting [`Mode::Forced`] starts one measurement; the
    /// sensor clears the field back to sleep by itself once it finishes.
    pub async fn set_mode(&mut self, mode: Mode) -> Result<(), Error> {
        let mut raw = [0u8; 1];

        for _ in 0..MODE_POLL_ATTEMPTS {
            self.read_registers(REG_CTRL_MEAS, &mut raw).await?;
            if raw[0] & CTRL_MEAS_MODE_MASK == Mode::Sleep.raw() {
                break;
            }

            self.write_register(REG_CTRL_MEAS, raw[0] & !CTRL_MEAS_MODE_MASK)
                .await?;
            Timer::after_millis(MODE_POLL_INTERVAL_MS).await;
        }

        if raw[0] & CTRL_MEAS_MODE_MASK != Mode::Sleep.raw() {
            return Err(Error::ModeChangeTimeout);
        }

        if mode != Mode::Sleep {
            self.write_register(REG_CTRL_MEAS, (raw[0] & !CTRL_MEAS_MODE_MASK) | mode.raw())
                .await?;
        }

        Ok(())
    }

    /// Read the factory calibration coefficients.
    ///
    /// They live in three non-contiguous register blocks which are stitched
    /// into one array. The values never change, so the result should be read
    /// once and kept for as long as the sensor is powered.
    pub async fn read_calibration(&mut self) -> Result<Calibration, Error> {
        let mut raw = [0u8; COEFF_TOTAL_LENGTH];

        self.read_registers(REG_COEFF1, &mut raw[..COEFF1_LENGTH])
            .await?;
        self.read_registers(
            REG_COEFF2,
            &mut raw[COEFF1_LENGTH..COEFF1_LENGTH + COEFF2_LENGTH],
        )
        .await?;
        self.read_registers(REG_COEFF3, &mut raw[COEFF1_LENGTH + COEFF2_LENGTH..])
            .await?;

        Ok(Calibration::from_bytes(&raw))
    }

    /// Program one entry of the heater profile table.
    ///
    /// Writes the target temperature to `res_heat_<index>` and the heater-on
    /// time to `gas_wait_<index>`. Both registers keep their value until the
    /// sensor is reset, so this only has to be redone after a reset.
    pub async fn set_heater_profile(
        &mut self,
        profile: &HeaterProfile,
        calibration: &Calibration,
        ambient_celsius: i8,
    ) -> Result<(), Error> {
        if profile.index > MAX_HEATER_PROFILE {
            return Err(Error::InvalidHeaterProfile(profile.index));
        }

        let resistance =
            calibration.resistance_register(profile.target_temperature_celsius, ambient_celsius);

        self.write_registers(&[
            (REG_RES_HEAT0 + profile.index, resistance),
            (
                REG_GAS_WAIT0 + profile.index,
                gas_wait_register(profile.duration_ms),
            ),
        ])
        .await
    }

    /// Set the heater and gas-conversion enables and select the heater profile.
    ///
    /// Covers both `REG_CTRL_GAS_0` (0x70) and `REG_CTRL_GAS_1` (0x71) in one
    /// call so that changing the enables cannot silently reset the profile
    /// index, which shares a register with `run_gas`.
    pub async fn set_gas_config(&mut self, config: &GasConfig) -> Result<(), Error> {
        if config.heater_profile > MAX_HEATER_PROFILE {
            return Err(Error::InvalidHeaterProfile(config.heater_profile));
        }

        let mut raw = [0u8; 2];
        self.read_registers(REG_CTRL_GAS_0, &mut raw).await?;

        // `heat_off` is active-low with respect to the heater being enabled.
        let mut ctrl_gas_0 = raw[0] & !CTRL_GAS_0_HEAT_OFF_MASK;
        if !config.heater_enabled {
            ctrl_gas_0 |= CTRL_GAS_0_HEAT_OFF_MASK;
        }

        let mut ctrl_gas_1 = raw[1] & !(CTRL_GAS_1_RUN_GAS_MASK | CTRL_GAS_1_NB_CONV_MASK);
        if config.gas_measurement_enabled {
            ctrl_gas_1 |= CTRL_GAS_1_RUN_GAS_MASK;
        }
        ctrl_gas_1 |= config.heater_profile & CTRL_GAS_1_NB_CONV_MASK;

        self.write_registers(&[(REG_CTRL_GAS_0, ctrl_gas_0), (REG_CTRL_GAS_1, ctrl_gas_1)])
            .await
    }

    /// Read the `field_0` result block.
    ///
    /// Returns `None` while `new_data_0` is clear, meaning the measurement has
    /// not finished yet or the previous result was already consumed. Reading
    /// the block clears the flag.
    pub async fn read_field(&mut self) -> Result<Option<Measurement>, Error> {
        let mut raw = [0u8; FIELD_LENGTH];
        self.read_registers(REG_FIELD0, &mut raw).await?;

        if raw[FIELD_STATUS] & FIELD_STATUS_NEW_DATA_MASK == 0 {
            return Ok(None);
        }

        Ok(Some(Measurement {
            pressure_raw: u32::from_be_bytes([
                0,
                raw[FIELD_PRESSURE],
                raw[FIELD_PRESSURE + 1],
                raw[FIELD_PRESSURE + 2],
            ]),
            temperature_raw: u32::from_be_bytes([
                0,
                raw[FIELD_TEMPERATURE],
                raw[FIELD_TEMPERATURE + 1],
                raw[FIELD_TEMPERATURE + 2],
            ]),
            relative_humidity_raw: u16::from_be_bytes([
                raw[FIELD_HUMIDITY],
                raw[FIELD_HUMIDITY + 1],
            ]),
            // 10-bit result split across a whole MSB byte and the top two bits
            // of the LSB byte.
            gas_resistance_raw: ((raw[FIELD_GAS_MSB] as u16) << 2)
                | (raw[FIELD_GAS_LSB] >> GAS_LSB_RESISTANCE_SHIFT) as u16,
            gas_range: raw[FIELD_GAS_LSB] & GAS_LSB_RANGE_MASK,
            gas_measurement_valid: raw[FIELD_GAS_LSB] & GAS_LSB_VALID_MASK != 0,
            heater_stable: raw[FIELD_GAS_LSB] & GAS_LSB_HEAT_STABLE_MASK != 0,
            gas_measurement_index: raw[FIELD_STATUS] & FIELD_STATUS_GAS_INDEX_MASK,
            measurement_index: raw[FIELD_MEASUREMENT_INDEX],
        }))
    }

    /// Run one forced-mode measurement and return the raw ADC results.
    ///
    /// Triggers the measurement, waits for the conversion time implied by
    /// `configuration` plus the heater-on time of `heater_duration_ms`, then
    /// polls `field_0` until `new_data_0` is set. Pass a heater duration of 0
    /// when the gas conversion is disabled.
    pub async fn measure_forced(
        &mut self,
        configuration: &Configuration,
        heater_duration_ms: u16,
    ) -> Result<Measurement, Error> {
        self.set_mode(Mode::Forced).await?;

        let wait_us = configuration.measurement_duration_us() + heater_duration_ms as u32 * 1000;
        Timer::after_micros(wait_us as u64).await;

        for _ in 0..FIELD_POLL_ATTEMPTS {
            if let Some(measurement) = self.read_field().await? {
                return Ok(measurement);
            }
            Timer::after_millis(FIELD_POLL_INTERVAL_MS).await;
        }

        Err(Error::NoNewData)
    }

    /// Reset the sensor, confirm its identity, and apply `configuration`.
    ///
    /// Leaves the sensor in sleep mode with the gas conversion disabled; call
    /// [`Bme690::set_heater_profile`] and [`Bme690::set_gas_config`] afterwards
    /// to enable it.
    pub async fn init(&mut self, configuration: &Configuration) -> Result<(), Error> {
        self.reset().await?;
        let chip_id = self.chip_id().await?;
        if chip_id != EXPECTED_CHIP_ID {
            return Err(Error::InvalidChipId(chip_id));
        }

        self.set_oversampling(
            configuration.humidity_oversampling,
            configuration.temperature_oversampling,
            configuration.pressure_oversampling,
        )
        .await?;

        self.set_iir_filter(configuration.iir_filter).await?;

        Ok(())
    }
}

/// Encode a heater-on time in milliseconds into a `gas_wait_x` register value.
///
/// The register holds a 6-bit count in bits 5:0 and a 2-bit multiplier
/// exponent in bits 7:6, so the count is repeatedly divided by four until it
/// fits. Resolution therefore drops as the duration grows, and durations at or
/// above 4032 ms all map to the maximum encoding.
fn gas_wait_register(duration_ms: u16) -> u8 {
    if duration_ms >= MAX_HEATER_DURATION_MS {
        return 0xFF;
    }

    let mut duration = duration_ms;
    let mut multiplier: u8 = 0;
    while duration > GAS_WAIT_COUNT_MAX {
        duration /= 4;
        multiplier += 1;
    }

    duration as u8 | (multiplier << GAS_WAIT_MULTIPLIER_SHIFT)
}
