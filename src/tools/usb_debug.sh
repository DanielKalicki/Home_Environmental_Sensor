#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FIRMWARE="${ROOT_DIR}/target/xtensa-esp32s3-none-elf/release/xiao-esp32s3"

# Load the Espressif tools, including espflash, when available.
if [[ -f "${HOME}/export-esp.sh" ]]; then
    # shellcheck source=/dev/null
    source "${HOME}/export-esp.sh"
fi

if ! command -v espflash >/dev/null 2>&1; then
    echo "error: espflash was not found on PATH" >&2
    echo "Install espflash and ensure the Espressif environment is loaded." >&2
    exit 1
fi

# The XIAO ESP32-S3 exposes println! through its USB Serial/JTAG interface.
# Prefer an explicitly configured port, then the usual Linux USB serial path.
PORT="${ESPFLASH_PORT:-}"
if [[ -z "${PORT}" && -e /dev/ttyACM0 ]]; then
    PORT=/dev/ttyACM0
fi

if [[ -z "${PORT}" ]]; then
    echo "error: no USB serial device found" >&2
    echo "Connect the XIAO ESP32-S3, or set ESPFLASH_PORT=/dev/ttyACM0." >&2
    exit 1
fi

if [[ ! -e "${PORT}" ]]; then
    echo "error: serial device does not exist: ${PORT}" >&2
    exit 1
fi

if [[ ! -f "${FIRMWARE}" ]]; then
    echo "error: firmware ELF was not found: ${FIRMWARE}" >&2
    echo "Build the firmware first with ./tools/build.sh." >&2
    exit 1
fi

cd "${ROOT_DIR}"

# This is a monitor, not a flashing operation: leave the already-running
# firmware untouched. usb-reset enters the serial bootloader, which prevents
# the application from producing its println! messages.
exec espflash monitor \
    --elf "${FIRMWARE}" \
    --chip esp32s3 \
    --port "${PORT}" \
    --before no-reset-no-sync \
    --after no-reset \
    --log-format serial \
    "$@"
