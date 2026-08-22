#!/usr/bin/env bash
# Invoked by `cargo tauri build` — cwd may be `desktop/` or `desktop/src-tauri/`.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DESKTOP="$(cd "$HERE/../.." && pwd)"
exec bash "$DESKTOP/scripts/prepare-tauri-resources.sh"
