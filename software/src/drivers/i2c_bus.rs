//! I2C bus helpers: bit-banged bus recovery and a full 7-bit address scan.
//!
//! The SDA/SCL pins are fixed at compile time so that the bus-recovery routine
//! can take the concrete `GpioPin` types it needs to reconfigure them as
//! open-drain outputs.
//!
//! [`I2cBus`] owns the I2C0 peripheral together with both pins, so that a task
//! can recover the bus and rebuild the peripheral. Because the sensors share
//! one bus, it is handed to the tasks inside an `embassy_sync` mutex.

use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use esp_hal::{
    clock::Clocks,
    delay::Delay,
    gpio::{GpioPin, Level, OutputOpenDrain, Pull},
    i2c::I2C,
    peripherals::I2C0,
    prelude::*,
    Blocking,
};
use esp_println::{print, println};

/// GPIO number carrying I2C0 SDA.
pub const I2C_SDA_PIN: u8 = 5;
/// GPIO number carrying I2C0 SCL.
pub const I2C_SCL_PIN: u8 = 6;

/// Concrete pin types accepted by the helpers in this module.
pub type SdaPin = GpioPin<I2C_SDA_PIN>;
pub type SclPin = GpioPin<I2C_SCL_PIN>;

const I2C_FIRST_ADDRESS: u8 = 0x03;
const I2C_LAST_ADDRESS: u8 = 0x77;
const MAX_I2C_DEVICES: usize = (I2C_LAST_ADDRESS - I2C_FIRST_ADDRESS + 1) as usize;
const I2C_ADDRESS_COUNT: usize = MAX_I2C_DEVICES;
const SCAN_PROGRESS_BAR_WIDTH: usize = 30;
const I2C_SCAN_RETRIES: usize = 3;
const I2C_SCAN_RETRY_DELAY_MS: u32 = 10;
/// Half period of the bit-banged bus-recovery clock (~100 kHz).
const I2C_RECOVERY_HALF_PERIOD_US: u32 = 5;
/// Bus speed used for every sensor transfer.
const I2C_FREQUENCY_KHZ: u32 = 100;

/// Everything needed to drive I2C0, owned in one place.
///
/// The peripheral is deliberately *not* kept alive between transactions: a NACK
/// leaves the bus held, so it has to be bit-banged back to idle and rebuilt,
/// which is only possible while the pins are not owned by the peripheral.
pub struct I2cBus {
    i2c0: I2C0,
    sda: SdaPin,
    scl: SclPin,
    clocks: &'static Clocks<'static>,
    delay: Delay,
}

/// The single I2C bus, shared by every sensor task.
///
/// A task holds the lock for a whole transaction, including the awaited
/// datasheet delays, so the two sensors can never interleave transfers.
pub type SharedI2cBus = Mutex<CriticalSectionRawMutex, I2cBus>;

impl I2cBus {
    pub fn new(
        i2c0: I2C0,
        sda: SdaPin,
        scl: SclPin,
        clocks: &'static Clocks<'static>,
        delay: Delay,
    ) -> Self {
        Self {
            i2c0,
            sda,
            scl,
            clocks,
            delay,
        }
    }

    /// Recover the bus, then build a fresh I2C peripheral on it.
    ///
    /// The returned peripheral borrows the bus, so the caller keeps exclusive
    /// access for as long as it uses it.
    pub fn acquire(&mut self) -> I2C<'_, I2C0, Blocking> {
        recover_i2c_bus(&mut self.sda, &mut self.scl, &self.delay);
        I2C::new(
            &mut self.i2c0,
            &mut self.sda,
            &mut self.scl,
            I2C_FREQUENCY_KHZ.kHz(),
            self.clocks,
        )
    }

    /// Probe every valid 7-bit address. See [`scan_i2c`].
    pub fn scan(&mut self) -> I2cScanResult {
        scan_i2c(
            &mut self.i2c0,
            &mut self.sda,
            &mut self.scl,
            self.clocks,
            &self.delay,
        )
    }
}

/// The 7-bit addresses that acknowledged during an I2C bus scan.
pub struct I2cScanResult {
    addresses: [u8; MAX_I2C_DEVICES],
    count: usize,
}

impl I2cScanResult {
    pub fn addresses(&self) -> &[u8] {
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
///
/// The pulses are microseconds wide, so this deliberately busy-waits with a
/// blocking `Delay` instead of yielding to the Embassy executor.
pub fn recover_i2c_bus(sda: &mut SdaPin, scl: &mut SclPin, delay: &Delay) {
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
pub fn scan_i2c(
    i2c0: &mut I2C0,
    sda: &mut SdaPin,
    scl: &mut SclPin,
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

/// Print the outcome of [`scan_i2c`] to the serial monitor.
pub fn print_scan_result(result: &I2cScanResult) {
    if result.addresses().is_empty() {
        println!("No I2C devices acknowledged on I2C0");
    } else {
        for address in result.addresses() {
            println!("I2C device found at 0x{:02X}", address);
        }
    }
}
