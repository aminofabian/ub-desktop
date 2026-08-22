#!/usr/bin/env bash
# Regenerate Tauri bundle icons from the canonical platform logomark SVG.
# Run after changing frontend/public/kiosk-mark.svg or lib/platform-logo-mark.ts.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SRC="$ROOT/frontend/public/kiosk-mark.svg"
OUT="$ROOT/desktop/src-tauri/icons"

if [[ ! -f "$SRC" ]]; then
  echo "error: source SVG missing at $SRC" >&2
  exit 1
fi

echo "Generating desktop app icons from $SRC"
cd "$ROOT/desktop/src-tauri"
cargo tauri icon "$SRC" -o icons

echo "Icons written to $OUT"
