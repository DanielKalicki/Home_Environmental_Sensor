#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_sync::mutex::Mutex;
use esp_backtrace as _;
use esp_hal::{
    clock::{ClockControl, Clocks},
    delay::Delay,
    gpio::{AnyPin, Io, Level, Output},
    peripherals::Peripherals,
    system::SystemControl,
    timer::{timg::TimerGroup, ErasedTimer, OneShotTimer},
};
use esp_println::println;
use static_cell::StaticCell;

mod drivers;
mod tasks;

use drivers::i2c_bus::{print_scan_result, I2cBus, SharedI2cBus, I2C_SCL_PIN, I2C_SDA_PIN};
use tasks::{blink_task, scd41_task, sps30_task};

/// Embassy needs one hardware timer to drive `embassy_time`.
static TIMERS: StaticCell<[OneShotTimer<ErasedTimer>; 1]> = StaticCell::new();
/// The tasks need the clock configuration for the whole program run.
static CLOCKS: StaticCell<Clocks<'static>> = StaticCell::new();
/// The I2C bus shared by the sensor tasks.
static BUS: StaticCell<SharedI2cBus> = StaticCell::new();

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    let peripherals = Peripherals::take();
    let system = SystemControl::new(peripherals.SYSTEM);
    let clocks = CLOCKS.init(ClockControl::max(system.clock_control).freeze());

    // Drive `embassy_time` from TIMG0's first timer.
    let timg0 = TimerGroup::new(peripherals.TIMG0, clocks);
    let timer0: ErasedTimer = timg0.timer0.into();
    let timers = TIMERS.init([OneShotTimer::new(timer0)]);
    esp_hal_embassy::init(clocks, timers);

    let io = Io::new(peripherals.GPIO, peripherals.IO_MUX);
    // Board-specific wiring belongs here; the blink task only needs an output.
    let led = Output::new(AnyPin::new(io.pins.gpio21), Level::Low);

    let mut bus = I2cBus::new(
        peripherals.I2C0,
        io.pins.gpio5,
        io.pins.gpio6,
        clocks,
        Delay::new(clocks),
    );

    println!("XIAO ESP32-S3 Embassy firmware started; blinking GPIO21");
    println!(
        "Scanning I2C0 at 100 kHz (SDA GPIO{}, SCL GPIO{})...",
        I2C_SDA_PIN, I2C_SCL_PIN
    );

    let devices = bus.scan();
    print_scan_result(&devices);

    // Hand the bus to the sensor tasks; each one locks it for a whole
    // transaction, so their transfers can never interleave.
    let bus: &'static SharedI2cBus = BUS.init(Mutex::new(bus));

    spawner.spawn(blink_task::blink_task(led)).ok();
    spawner.spawn(sps30_task::measure_task(bus)).ok();
    spawner.spawn(scd41_task::measure_task(bus)).ok();
}
