#!/usr/bin/env bash
# beforeBuildCommand for Windows bundles (tauri.windows.conf.json).
# Verifies Resources-windows/ has been staged; unlike the macOS flow we do
# NOT rebuild everything on each `tauri build` because the Windows resources
# come from downloads (JRE, MariaDB) that rarely change.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DESKTOP="$(cd "$HERE/.." && pwd)"
RES="$HERE/Resources-windows"

missing=()
[[ -f "$RES/jar/kiosk.jar" ]] || missing+=("jar/kiosk.jar")
[[ -f "$RES/jre/bin/java.exe" ]] || missing+=("jre/bin/java.exe")
[[ -f "$RES/mariadb/bin/mariadbd.exe" ]] || missing+=("mariadb/bin/mariadbd.exe")
[[ -f "$RES/config/application.properties" ]] || missing+=("config/application.properties")

# The Tauri splash (frontendDist) is embedded into the EXE at build time from
# desktop/dist/. If it is missing the build succeeds but the window renders
# blank ("nothing is showing"), so fail the build instead.
if [[ ! -f "$DESKTOP/dist/index.html" ]]; then
  echo "error: $DESKTOP/dist/index.html is missing — the Tauri splash page is" >&2
  echo "embedded into the exe and a missing page means a blank window." >&2
  echo "Run: bash desktop/scripts/prepare-windows-resources.sh" >&2
  exit 1
fi

if (( ${#missing[@]} > 0 )); then
  echo "error: Resources-windows/ is not staged (missing: ${missing[*]})." >&2
  echo "Run: bash desktop/scripts/prepare-windows-resources.sh" >&2
  exit 1
fi
echo "Resources-windows/ OK."
