#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="${ROOT_DIR}/.env"

# Load the Wi-Fi credentials, if they were configured. `set -a` exports every
# variable the file assigns, so they reach the compiler as build-time
# environment variables. See .env.example for the expected contents.
if [[ -f "${ENV_FILE}" ]]; then
    set -a
    # shellcheck source=/dev/null
    source "${ENV_FILE}"
    set +a
else
    echo "note: ${ENV_FILE} not found; building without Wi-Fi credentials." >&2
    echo "      Copy .env.example to .env to enable the web server." >&2
fi

# Load the Espressif compiler environment when it is available. If it is
# already loaded, this is harmless; otherwise fail with a useful message.
if [[ -f "${HOME}/export-esp.sh" ]]; then
    # shellcheck source=/dev/null
    source "${HOME}/export-esp.sh"
fi

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo was not found on PATH" >&2
    echo "Install the Espressif Rust toolchain with espup first." >&2
    exit 1
fi

cd "${ROOT_DIR}"

# Never use more than two parallel compiler processes.
cargo build --release --jobs 2 --target xtensa-esp32s3-none-elf
