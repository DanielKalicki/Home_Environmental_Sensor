//! Build script: credential tracking and the BSEC static library.

use std::path::PathBuf;

/// Directory holding the ESP32-S3 build of the BSEC algorithm library.
///
/// BSEC is closed source and ships as one prebuilt archive per architecture.
/// This is the IAQ build, which is the one that provides the air-quality
/// outputs; the neighbouring `Sel_IAQ` build swaps them for the gas
/// classification and regression outputs instead.
const BSEC_LIBRARY_DIRECTORY: &str =
    "src/drivers/bsec/bsec_v3-3-0-0/release_bin/IAQ/bin/esp/esp32_s3";

fn main() {
    // `option_env!` bakes the credentials into the binary at compile time, but
    // Cargo does not track environment variables by default, so without these
    // lines a changed `.env` would silently keep the previously compiled
    // credentials.
    println!("cargo:rerun-if-env-changed=WIFI_SSID");
    println!("cargo:rerun-if-env-changed=WIFI_PASSWORD");

    // The linker needs an absolute path: it does not run from the manifest
    // directory.
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let library = manifest.join(BSEC_LIBRARY_DIRECTORY);

    assert!(
        library.join("libalgobsec.a").is_file(),
        "BSEC library not found at {}; the vendored archive under \
         src/drivers/bsec/ is missing or was extracted elsewhere",
        library.display()
    );

    println!("cargo:rerun-if-changed={}/libalgobsec.a", library.display());
    println!("cargo:rustc-link-search=native={}", library.display());
    println!("cargo:rustc-link-lib=static=algobsec");
}
