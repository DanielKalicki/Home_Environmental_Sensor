#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_SCRIPT="${ROOT_DIR}/tools/build.sh"
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

"${BUILD_SCRIPT}"

FLASH_ARGS=(flash --chip esp32s3)
if [[ -n "${ESPFLASH_PORT:-}" ]]; then
    FLASH_ARGS+=(--port "${ESPFLASH_PORT}")
fi

cd "${ROOT_DIR}"
espflash "${FLASH_ARGS[@]}" "${FIRMWARE}" "$@"
