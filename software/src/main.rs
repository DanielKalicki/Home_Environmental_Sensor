#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_net::{Config as NetConfig, Stack};
use embassy_sync::mutex::Mutex;
use esp_backtrace as _;
use esp_hal::{
    clock::{ClockControl, Clocks},
    delay::Delay,
    gpio::{AnyPin, Io, Level, Output},
    peripherals::Peripherals,
    rng::Rng,
    system::SystemControl,
    timer::{timg::TimerGroup, ErasedTimer, OneShotTimer, PeriodicTimer},
};
use esp_println::println;
use esp_wifi::{initialize, wifi::WifiStaDevice, EspWifiInitFor};
use static_cell::StaticCell;

mod drivers;
mod tasks;
mod utils;

use drivers::i2c_bus::{print_scan_result, I2cBus, SharedI2cBus, I2C_SCL_PIN, I2C_SDA_PIN};
use tasks::web_server_task::{WifiStack, WifiStackResources};
use tasks::{as7343_task, bme690_task, blink_task, scd41_task, sps30_task, web_server_task};
use utils::psram::Psram;
use utils::shared_state;

/// Embassy needs one hardware timer to drive `embassy_time`.
static TIMERS: StaticCell<[OneShotTimer<ErasedTimer>; 1]> = StaticCell::new();
/// The tasks need the clock configuration for the whole program run.
static CLOCKS: StaticCell<Clocks<'static>> = StaticCell::new();
/// The I2C bus shared by the sensor tasks.
static BUS: StaticCell<SharedI2cBus> = StaticCell::new();
/// Socket and buffer bookkeeping owned by the network stack.
static NET_RESOURCES: StaticCell<WifiStackResources> = StaticCell::new();
/// The network stack itself, shared by the runner and the web server.
static NET_STACK: StaticCell<WifiStack> = StaticCell::new();

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    let peripherals = Peripherals::take();

    // Map the external PSRAM before anything else: the routine retunes the
    // memory-SPI timing and reconfigures the data cache, so it has to run
    // before the clocks are raised and before any other peripheral is set up.
    let mut psram = Psram::init(peripherals.PSRAM);

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

    // Reserve the measurement histories before any reading can be published;
    // readings taken before this would be dropped.
    match shared_state::init(&mut psram).await {
        Some(bytes) => println!(
            "History: {} SCD41, {} SPS30, {} BME690 and {} AS7343 readings ({} h) in {} KiB of PSRAM, {} KiB free",
            shared_state::SCD41_CAPACITY,
            shared_state::SPS30_CAPACITY,
            shared_state::BME690_CAPACITY,
            shared_state::AS7343_CAPACITY,
            shared_state::HISTORY_WINDOW_MS / 3_600_000,
            bytes / 1024,
            psram.free_bytes() / 1024
        ),
        None => println!("History: PSRAM too small, readings will not be retained"),
    }

    // Hand the bus to the sensor tasks; each one locks it for a whole
    // transaction, so their transfers can never interleave.
    let bus: &'static SharedI2cBus = BUS.init(Mutex::new(bus));

    spawner.spawn(blink_task::blink_task(led)).ok();
    spawner.spawn(sps30_task::measure_task(bus)).ok();
    spawner.spawn(scd41_task::measure_task(bus)).ok();
    spawner.spawn(bme690_task::measure_task(bus)).ok();
    spawner.spawn(as7343_task::measure_task(bus)).ok();

    // The Wi-Fi driver needs its own timer, plus entropy for the radio and
    // for the TCP initial sequence numbers.
    let timg1 = TimerGroup::new(peripherals.TIMG1, clocks);
    let mut rng = Rng::new(peripherals.RNG);
    let stack_seed = ((rng.random() as u64) << 32) | rng.random() as u64;

    let wifi_init = initialize(
        EspWifiInitFor::Wifi,
        PeriodicTimer::new(ErasedTimer::from(timg1.timer0)),
        rng,
        peripherals.RADIO_CLK,
        clocks,
    )
    .expect("Wi-Fi initialisation failed");

    let (wifi_device, wifi_controller) =
        esp_wifi::wifi::new_with_mode(&wifi_init, peripherals.WIFI, WifiStaDevice)
            .expect("could not create the Wi-Fi station interface");

    // The address is obtained from the network's DHCP server; the assigned
    // address is printed once the lease arrives.
    let stack: &'static WifiStack = NET_STACK.init(Stack::new(
        wifi_device,
        NetConfig::dhcpv4(Default::default()),
        NET_RESOURCES.init(WifiStackResources::new()),
        stack_seed,
    ));

    spawner
        .spawn(web_server_task::wifi_connection_task(wifi_controller))
        .ok();
    spawner.spawn(web_server_task::net_task(stack)).ok();
    spawner.spawn(web_server_task::web_server_task(stack)).ok();
}
