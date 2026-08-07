use embassy_time::Timer;
use esp_hal::gpio::{AnyPin, Output};

/// Half period of the liveness blink.
const LED_BLINK_INTERVAL_MS: u64 = 500;

/// Blink the configured user LED.
///
/// On boards where the LED is wired active-low, the visible on/off states are
/// reversed, but the blink still confirms that the executor keeps running.
#[embassy_executor::task]
pub async fn blink_task(mut led: Output<'static, AnyPin<'static>>) {
    loop {
        led.set_high();
        Timer::after_millis(LED_BLINK_INTERVAL_MS).await;
        led.set_low();
        Timer::after_millis(LED_BLINK_INTERVAL_MS).await;
    }
}
