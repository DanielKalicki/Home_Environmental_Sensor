//! Build script: make Cargo rebuild when the Wi-Fi credentials change.
//!
//! `option_env!` bakes the values into the binary at compile time, but Cargo
//! does not track environment variables by default, so without these lines a
//! changed `.env` would silently keep the previously compiled credentials.

fn main() {
    println!("cargo:rerun-if-env-changed=WIFI_SSID");
    println!("cargo:rerun-if-env-changed=WIFI_PASSWORD");
}
