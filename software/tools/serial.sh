#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

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

cd "${ROOT_DIR}"

MONITOR_ARGS=(monitor --chip esp32s3)
if [[ -n "${ESPFLASH_PORT:-}" ]]; then
    MONITOR_ARGS+=(--port "${ESPFLASH_PORT}")
fi

# Additional arguments can be passed through, for example:
#   ./tools/serial.sh --baud 115200
exec espflash "${MONITOR_ARGS[@]}" "$@"
