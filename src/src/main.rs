#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::{
    clock::{ClockControl, Clocks},
    delay::Delay,
    gpio::{GpioPin, Io, Level, Output, OutputOpenDrain, Pull},
    i2c::I2C,
    peripherals::{Peripherals, I2C0},
    prelude::*,
    system::SystemControl,
};
use esp_println::{print, println};

// mod scd41;
mod sps30;

// use scd41::Scd41;
use sps30::Sps30;

const I2C_SDA_PIN: u8 = 5;
const I2C_SCL_PIN: u8 = 6;
const I2C_FIRST_ADDRESS: u8 = 0x03;
const I2C_LAST_ADDRESS: u8 = 0x77;
const MAX_I2C_DEVICES: usize = (I2C_LAST_ADDRESS - I2C_FIRST_ADDRESS + 1) as usize;
const I2C_ADDRESS_COUNT: usize = MAX_I2C_DEVICES;
const SCAN_PROGRESS_BAR_WIDTH: usize = 30;
const I2C_SCAN_RETRIES: usize = 3;
const I2C_SCAN_RETRY_DELAY_MS: u32 = 10;
/// Half period of the bit-banged bus-recovery clock (~100 kHz).
const I2C_RECOVERY_HALF_PERIOD_US: u32 = 5;
/// Idle time between two measurement read-outs.
const MEASUREMENT_INTERVAL_MS: u32 = 5000;
/// Idle time before retrying after a bus or sensor error.
const ERROR_RETRY_DELAY_MS: u32 = 1000;
/// How long to wait for the SPS30 to flag a fresh result.
const DATA_READY_POLL_ATTEMPTS: usize = 30;
const DATA_READY_POLL_DELAY_MS: u32 = 100;

/// The 7-bit addresses that acknowledged during an I2C bus scan.
struct I2cScanResult {
    addresses: [u8; MAX_I2C_DEVICES],
    count: usize,
}

impl I2cScanResult {
    fn addresses(&self) -> &[u8] {
        &self.addresses[..self.count]
    }
}

fn print_scan_progress(address: u8, completed: usize) {
    let mut bar = [b'.'; SCAN_PROGRESS_BAR_WIDTH];
    let filled = completed * SCAN_PROGRESS_BAR_WIDTH / I2C_ADDRESS_COUNT;

    for segment in &mut bar[..filled] {
        *segment = b'#';
    }

    let bar = core::str::from_utf8(&bar).unwrap_or("??????????????????????????????");
    print!(
        "\rScanning I2C0: [{}] 0x{:02X} ({}/{})",
        bar, address, completed, I2C_ADDRESS_COUNT
    );
}

/// Return the bus to a known idle state.
///
/// `esp-hal` aborts a transfer as soon as a NACK is seen and resets the I2C
/// state machine *without* emitting a STOP condition, so every non-answering
/// address leaves SCL/SDA in an undefined state. Those aborts accumulate over a
/// long scan until the bus is effectively locked and even a device that is
/// present stops answering. Clocking SCL nine times (so any slave that is
/// holding SDA can finish its byte) followed by a manual STOP releases it.
fn recover_i2c_bus(
    sda: &mut GpioPin<I2C_SDA_PIN>,
    scl: &mut GpioPin<I2C_SCL_PIN>,
    delay: &Delay,
) {
    let mut sda = OutputOpenDrain::new(sda, Level::High, Pull::Up);
    let mut scl = OutputOpenDrain::new(scl, Level::High, Pull::Up);

    for _ in 0..9 {
        scl.set_low();
        delay.delay_micros(I2C_RECOVERY_HALF_PERIOD_US);
        scl.set_high();
        delay.delay_micros(I2C_RECOVERY_HALF_PERIOD_US);
    }

    // STOP condition: SDA goes low->high while SCL is high.
    sda.set_low();
    delay.delay_micros(I2C_RECOVERY_HALF_PERIOD_US);
    scl.set_high();
    delay.delay_micros(I2C_RECOVERY_HALF_PERIOD_US);
    sda.set_high();
    delay.delay_micros(I2C_RECOVERY_HALF_PERIOD_US);
}

/// Probe every valid 7-bit I2C address and return the ones that acknowledge.
///
/// Each probe is an address-only write, so no device registers are modified.
/// The bus is recovered and the I2C peripheral is rebuilt before every probe so
/// that a NACK at one address cannot hide a device at a later address.
fn scan_i2c(
    i2c0: &mut I2C0,
    sda: &mut GpioPin<I2C_SDA_PIN>,
    scl: &mut GpioPin<I2C_SCL_PIN>,
    clocks: &Clocks<'_>,
    delay: &Delay,
) -> I2cScanResult {
    let mut result = I2cScanResult {
        addresses: [0; MAX_I2C_DEVICES],
        count: 0,
    };

    for address in I2C_FIRST_ADDRESS..=I2C_LAST_ADDRESS {
        let completed = (address - I2C_FIRST_ADDRESS) as usize;
        print_scan_progress(address, completed);

        let mut acknowledged = false;
        for attempt in 0..I2C_SCAN_RETRIES {
            recover_i2c_bus(sda, scl, delay);

            {
                let mut i2c = I2C::new(&mut *i2c0, &mut *sda, &mut *scl, 100.kHz(), clocks);
                if i2c.write(address, &[]).is_ok() {
                    acknowledged = true;
                }
            }

            if acknowledged {
                break;
            }

            if attempt + 1 < I2C_SCAN_RETRIES {
                delay.delay_millis(I2C_SCAN_RETRY_DELAY_MS);
            }
        }

        if acknowledged {
            result.addresses[result.count] = address;
            result.count += 1;
        }
    }

    recover_i2c_bus(sda, scl, delay);
    print_scan_progress(I2C_LAST_ADDRESS, I2C_ADDRESS_COUNT);
    println!();

    result
}

fn print_scan_result(result: &I2cScanResult) {
    if result.addresses().is_empty() {
        println!("No I2C devices acknowledged on I2C0");
    } else {
        for address in result.addresses() {
            println!("I2C device found at 0x{:02X}", address);
        }
    }
}

/// Wait for the SPS30 to flag a fresh result, then read it.
///
/// If no flag appears within the poll window the last values are read anyway;
/// a genuine bus problem still surfaces as an error from the read itself.
fn read_when_ready<T: esp_hal::i2c::Instance>(
    sensor: &mut Sps30<'_, '_, T>,
    delay: &Delay,
) -> Result<sps30::Measurement, sps30::Error> {
    for _ in 0..DATA_READY_POLL_ATTEMPTS {
        if sensor.is_data_ready()? {
            break;
        }
        delay.delay_millis(DATA_READY_POLL_DELAY_MS);
    }

    sensor.read_measured_values()
}

/// Blink the XIAO ESP32-S3's built-in user LED (GPIO21).
///
/// On boards where the LED is wired active-low, the visible on/off states are
/// reversed, but the one-second blink still confirms that the firmware runs.
#[entry]
fn main() -> ! {
    let peripherals = Peripherals::take();
    let system = SystemControl::new(peripherals.SYSTEM);
    let clocks = ClockControl::max(system.clock_control).freeze();

    let io = Io::new(peripherals.GPIO, peripherals.IO_MUX);
    let mut led = Output::new(io.pins.gpio21, Level::Low);
    let delay = Delay::new(&clocks);
    let mut i2c0 = peripherals.I2C0;
    let mut sda = io.pins.gpio5;
    let mut scl = io.pins.gpio6;

    println!("XIAO ESP32-S3 firmware started; blinking GPIO21");
    println!(
        "Scanning I2C0 at 100 kHz (SDA GPIO{}, SCL GPIO{})...",
        I2C_SDA_PIN, I2C_SCL_PIN
    );

    let devices = scan_i2c(&mut i2c0, &mut sda, &mut scl, &clocks, &delay);
    print_scan_result(&devices);

    // Set on every bus error so the next cycle re-runs the sensor's init
    // sequence instead of assuming it is still in the idle state.
    let mut needs_init = true;

    loop {
        led.set_high();

        // Recover the bus and rebuild the I2C peripheral for every cycle. A
        // NACK makes esp-hal reset the state machine without emitting a STOP,
        // which leaves the bus held and makes every later transfer fail with
        // AckCheckFailed; only bit-banging the bus back to idle clears that.
        recover_i2c_bus(&mut sda, &mut scl, &delay);
        let result = {
            let mut i2c = I2C::new(&mut i2c0, &mut sda, &mut scl, 100.kHz(), &clocks);
            let mut sensor = Sps30::new(&mut i2c, delay);

            let initialized = if needs_init {
                match sensor.serial_number() {
                    Ok((serial, len)) => {
                        let serial = core::str::from_utf8(&serial[..len]).unwrap_or("<non-ascii>");
                        println!("SPS30 found, serial number: {}", serial);

                        // The sensor rejects start_measurement while it is
                        // already measuring, so return it to idle first.
                        let _ = sensor.stop_measurement();

                        match sensor.start_measurement() {
                            Ok(()) => {
                                println!("SPS30: measurement started");
                                true
                            }
                            Err(error) => {
                                println!("SPS30: start_measurement failed: {:?}", error);
                                false
                            }
                        }
                    }
                    Err(error) => {
                        println!("SPS30: initialisation failed: {:?}", error);
                        false
                    }
                }
            } else {
                true
            };

            if initialized {
                Some(read_when_ready(&mut sensor, &delay))
            } else {
                None
            }
        };

        match result {
            Some(Ok(measurement)) => {
                needs_init = false;
                println!(
                    "PM1.0: {} ug/m3, PM2.5: {} ug/m3, PM4.0: {} ug/m3, PM10: {} ug/m3",
                    measurement.pm1_0, measurement.pm2_5, measurement.pm4_0, measurement.pm10
                );
                println!(
                    "NC0.5: {} #/cm3, NC1.0: {} #/cm3, NC2.5: {} #/cm3, NC4.0: {} #/cm3, NC10: {} #/cm3",
                    measurement.nc0_5,
                    measurement.nc1_0,
                    measurement.nc2_5,
                    measurement.nc4_0,
                    measurement.nc10
                );
                println!(
                    "Typical particle size: {} um",
                    measurement.typical_particle_size
                );
            }
            Some(Err(error)) => {
                needs_init = true;
                println!("SPS30: measurement failed: {:?}, recovering bus", error);
            }
            None => needs_init = true,
        }

        // ---------------------------------------------------------------
        // SCD41 CO2 measurement, disabled while the SPS30 is under test.
        // ---------------------------------------------------------------
        // let result = {
        //     let mut i2c = I2C::new(&mut i2c0, &mut sda, &mut scl, 100.kHz(), &clocks);
        //     let mut sensor = Scd41::new(&mut i2c, delay);
        //
        //     let initialized = if needs_init {
        //         match sensor
        //             .stop_periodic_measurement()
        //             .and_then(|()| sensor.serial_number())
        //         {
        //             Ok(serial) => {
        //                 println!("SCD41 ready, serial number: 0x{:012X}", serial);
        //                 true
        //             }
        //             Err(error) => {
        //                 println!("SCD41: initialisation failed: {:?}", error);
        //                 false
        //             }
        //         }
        //     } else {
        //         true
        //     };
        //
        //     if initialized {
        //         Some(sensor.measure_single_shot())
        //     } else {
        //         None
        //     }
        // };
        //
        // match result {
        //     Some(Ok(measurement)) => {
        //         needs_init = false;
        //         println!(
        //             "CO2: {} ppm, temperature: {} C, humidity: {} %",
        //             measurement.co2_ppm,
        //             measurement.temperature_celsius(),
        //             measurement.humidity_percent()
        //         );
        //     }
        //     Some(Err(error)) => {
        //         needs_init = true;
        //         println!("SCD41: measurement failed: {:?}, recovering bus", error);
        //     }
        //     None => needs_init = true,
        // }

        led.set_low();

        if needs_init {
            delay.delay_millis(ERROR_RETRY_DELAY_MS);
        } else {
            delay.delay_millis(MEASUREMENT_INTERVAL_MS);
        }
    }
}
