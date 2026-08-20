#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_net::{Config as NetConfig, Stack};
use embassy_sync::mutex::Mutex;
use embassy_time::Timer;
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
use esp_wifi::{initialize, wifi::WifiStaDevice, EspWifiInitFor, EspWifiInitialization};
use static_cell::StaticCell;

mod drivers;
mod tasks;
mod utils;

use drivers::i2c_bus::{print_scan_result, I2cBus, SharedI2cBus, I2C_SCL_PIN, I2C_SDA_PIN};
use tasks::web_server_task::{WifiStack, WifiStackResources};
use tasks::{
    as7343_task, bme690_task, blink_task, bmp581_task, mlx90640_task, opt4048_task, scd41_task,
    sht41_task, sps30_task, web_server_task,
};
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
/// Wi-Fi driver initialisation token; must outlive the station interface.
static WIFI_INIT: StaticCell<EspWifiInitialization> = StaticCell::new();

/// Spawn a task and report failure on the console.
///
/// Spawn errors are almost always an exhausted task arena. Swallowing them with
/// `.ok()` leaves the board looking "stuck" with no LED blink and no Wi-Fi
/// progress, so every failure is printed with the task name.
fn spawn_task(_spawner: &Spawner, name: &str, result: Result<(), embassy_executor::SpawnError>) {
    if let Err(error) = result {
        println!("SPAWN FAILED: {} ({:?})", name, error);
    }
}

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
            "History: {} SCD41, {} SPS30, {} BME690, {} AS7343, {} BMP581, {} OPT4048 and {} SHT41 readings ({} h) in {} KiB of PSRAM, {} KiB free",
            shared_state::SCD41_CAPACITY,
            shared_state::SPS30_CAPACITY,
            shared_state::BME690_CAPACITY,
            shared_state::AS7343_CAPACITY,
            shared_state::BMP581_CAPACITY,
            shared_state::OPT4048_CAPACITY,
            shared_state::SHT41_CAPACITY,
            shared_state::HISTORY_WINDOW_MS / 3_600_000,
            bytes / 1024,
            psram.free_bytes() / 1024
        ),
        None => println!("History: PSRAM too small, readings will not be retained"),
    }

    // Hand the bus to the sensor tasks; each one locks it for a whole
    // transaction, so their transfers can never interleave.
    let bus: &'static SharedI2cBus = BUS.init(Mutex::new(bus));

    // Start the liveness LED before Wi-Fi and the sensors so a later hang still
    // leaves a visible "executor is alive" signal when the freeze is only in
    // one of those subsystems.
    spawn_task(
        &spawner,
        "blink",
        spawner.spawn(blink_task::blink_task(led)),
    );

    // Prove the executor schedules tasks and that USB-serial still works after
    // the first yield. If this line never appears, time or the executor is
    // already broken before Wi-Fi starts.
    Timer::after_millis(200).await;
    println!("Executor: blink task scheduled, continuing startup");

    // The Wi-Fi driver needs its own timer, plus entropy for the radio and
    // for the TCP initial sequence numbers.
    let timg1 = TimerGroup::new(peripherals.TIMG1, clocks);
    let mut rng = Rng::new(peripherals.RNG);
    let stack_seed = ((rng.random() as u64) << 32) | rng.random() as u64;

    let wifi_init = WIFI_INIT.init(
        initialize(
            EspWifiInitFor::Wifi,
            PeriodicTimer::new(ErasedTimer::from(timg1.timer0)),
            rng,
            peripherals.RADIO_CLK,
            clocks,
        )
        .expect("Wi-Fi initialisation failed"),
    );

    let (wifi_device, wifi_controller) =
        esp_wifi::wifi::new_with_mode(wifi_init, peripherals.WIFI, WifiStaDevice)
            .expect("could not create the Wi-Fi station interface");

    // The address is obtained from the network's DHCP server; the assigned
    // address is printed once the lease arrives.
    let stack: &'static WifiStack = NET_STACK.init(Stack::new(
        wifi_device,
        NetConfig::dhcpv4(Default::default()),
        NET_RESOURCES.init(WifiStackResources::new()),
        stack_seed,
    ));

    spawn_task(
        &spawner,
        "wifi_connection",
        spawner.spawn(web_server_task::wifi_connection_task(wifi_controller)),
    );
    spawn_task(
        &spawner,
        "net",
        spawner.spawn(web_server_task::net_task(stack)),
    );
    spawn_task(
        &spawner,
        "web_server",
        spawner.spawn(web_server_task::web_server_task(stack)),
    );

    // Bring the radio up before the sensor tasks. Long blocking I2C work (the
    // thermal camera's EEPROM dump in particular) runs without yielding; if it
    // starts in the same window as the first association attempt, the executor
    // cannot poll the Wi-Fi futures and the board looks dead. Sensors do not
    // need the network, they only need the executor to stay responsive while
    // the station associates.
    println!("Wi-Fi: waiting for link before starting sensors");
    let mut waited_ms = 0u64;
    while !stack.is_link_up() {
        Timer::after_millis(500).await;
        waited_ms += 500;
        if waited_ms % 5000 == 0 {
            println!("Wi-Fi: still waiting for link after {} s", waited_ms / 1000);
        }
    }
    println!("Wi-Fi: link is up, starting sensor tasks");

    spawn_task(
        &spawner,
        "sps30",
        spawner.spawn(sps30_task::measure_task(bus)),
    );
    spawn_task(
        &spawner,
        "scd41",
        spawner.spawn(scd41_task::measure_task(bus)),
    );
    spawn_task(
        &spawner,
        "bme690",
        spawner.spawn(bme690_task::measure_task(bus)),
    );
    spawn_task(
        &spawner,
        "as7343",
        spawner.spawn(as7343_task::measure_task(bus)),
    );
    spawn_task(
        &spawner,
        "bmp581",
        spawner.spawn(bmp581_task::measure_task(bus)),
    );
    spawn_task(
        &spawner,
        "opt4048",
        spawner.spawn(opt4048_task::measure_task(bus)),
    );
    spawn_task(
        &spawner,
        "sht41",
        spawner.spawn(sht41_task::measure_task(bus)),
    );
    spawn_task(
        &spawner,
        "mlx90640",
        spawner.spawn(mlx90640_task::capture_task(bus)),
    );

    // Keep main alive and print a slow heartbeat. If this stops while sensors
    // run, the freeze is in a sensor path that never yields.
    loop {
        Timer::after_millis(30_000).await;
        println!("Executor: heartbeat, uptime ok");
    }
}
