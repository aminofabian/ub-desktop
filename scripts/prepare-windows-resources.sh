#!/usr/bin/env bash
# Stages desktop/src-tauri/Resources-windows/ for the Windows NSIS bundle:
#
#   jre/      — prebuilt Temurin 21 JRE for Windows x64 (no cross-jlink needed)
#   mariadb/  — MariaDB 10.11 winx64 (bin/share/lib, trimmed of dev files)
#   jar/      — kiosk.jar (platform-independent; reused from the macOS staging
#               or backend/build/libs)
#   config/   — application-desktop.properties
#
# Downloads are cached under desktop/.cache/ so re-runs are cheap.
# Cross-build afterwards with:
#   cd desktop/src-tauri
#   cargo tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
DESKTOP="$ROOT/desktop"
RES="$DESKTOP/src-tauri/Resources-windows"
CACHE="$DESKTOP/.cache"
BACKEND="$ROOT/backend"

MARIADB_VERSION="10.11.11"   # keep in sync with backend/build.gradle
JRE_API="https://api.adoptium.net/v3/assets/latest/21/hotspot?os=windows&architecture=x64&image_type=jre"
MARIADB_URL="https://archive.mariadb.org/mariadb-${MARIADB_VERSION}/winx64-packages/mariadb-${MARIADB_VERSION}-winx64.zip"

mkdir -p "$CACHE"

# ── 0. Tauri splash (frontendDist) ──────────────────────────────────────
# The Tauri shell embeds desktop/dist/ into the EXE at build time and shows
# it while MariaDB + the JVM boot. A missing dist means the window renders
# blank on Windows, so materialise it from the committed stub when absent.
if [[ ! -f "$DESKTOP/dist/index.html" ]]; then
  mkdir -p "$DESKTOP/dist"
  cp "$SCRIPT_DIR/splash-stub.html" "$DESKTOP/dist/index.html"
  echo "   splash: copied splash-stub.html -> dist/index.html"
fi

fetch() { # fetch <url> <dest-file>
  local url="$1" dest="$2"
  if [[ -s "$dest" ]]; then
    echo "   cached: $(basename "$dest")"
    return
  fi
  echo "   downloading $(basename "$dest") …"
  curl -fL --retry 3 -o "$dest.part" "$url"
  mv "$dest.part" "$dest"
}

echo "══ Prepare Windows bundle resources ══"

echo "── 1. kiosk.jar ──"
# Prefer the freshly built backend jar (release.sh runs `clean bootJar` right
# before this). The macOS-staged Resources/jar/kiosk.jar is only a fallback:
# it can be stale (e.g. from a previous release), and a stale jar is exactly
# how an outdated backend shipped inside a Windows installer before.
JAR="$(ls -1 "$BACKEND/build/libs"/kiosk-desktop-*.jar 2>/dev/null | sort | tail -1 || true)"
if [[ -z "$JAR" || ! -f "$JAR" ]] && [[ -f "$DESKTOP/src-tauri/Resources/jar/kiosk.jar" ]]; then
  JAR="$DESKTOP/src-tauri/Resources/jar/kiosk.jar"
fi
if [[ -z "$JAR" || ! -f "$JAR" ]]; then
  echo "error: kiosk.jar not found. Build it first:" >&2
  echo "  cd backend && ./gradlew clean bootJar -Pdesktop=true" >&2
  echo "  (or run desktop/scripts/prepare-tauri-resources.sh)" >&2
  exit 1
fi
echo "   using $JAR"

echo "── 2. Temurin 21 JRE (Windows x64) ──"
JRE_ZIP="$CACHE/temurin-21-jre-windows-x64.zip"
if [[ ! -s "$JRE_ZIP" ]]; then
  JRE_URL="$(curl -fsSL "$JRE_API" | python3 -c '
import json, sys
assets = json.load(sys.stdin)
print(assets[0]["binary"]["package"]["link"])
')"
  echo "   resolved: $JRE_URL"
fi
fetch "${JRE_URL:-}" "$JRE_ZIP"

echo "── 3. MariaDB ${MARIADB_VERSION} (winx64) ──"
MB_ZIP="$CACHE/mariadb-${MARIADB_VERSION}-winx64.zip"
fetch "$MARIADB_URL" "$MB_ZIP"

echo "── 4. Stage into $RES ──"
rm -rf "$RES"
mkdir -p "$RES/jar" "$RES/config"

cp "$JAR" "$RES/jar/kiosk.jar"
cp "$BACKEND/src/main/resources/application-desktop.properties" "$RES/config/application.properties"

STAGE="$CACHE/.stage"
rm -rf "$STAGE"
mkdir -p "$STAGE"

# JRE — the zip contains a single top-level dir (jdk-21.x.y+z-jre/).
unzip -q "$JRE_ZIP" -d "$STAGE/jre"
JRE_ROOT="$(find "$STAGE/jre" -maxdepth 1 -mindepth 1 -type d | head -1)"
[[ -f "$JRE_ROOT/bin/java.exe" ]] || { echo "error: java.exe missing in JRE zip" >&2; exit 1; }
mv "$JRE_ROOT" "$RES/jre"

# MariaDB — single top-level dir (mariadb-<version>-winx64/). Keep the
# runtime pieces; drop headers, import libs, debug symbols, and docs.
unzip -q "$MB_ZIP" -d "$STAGE/mariadb"
MB_ROOT="$(find "$STAGE/mariadb" -maxdepth 1 -mindepth 1 -type d | head -1)"
[[ -f "$MB_ROOT/bin/mariadbd.exe" ]] || { echo "error: mariadbd.exe missing in MariaDB zip" >&2; exit 1; }
mkdir -p "$RES/mariadb"
cp -Rp "$MB_ROOT/bin" "$RES/mariadb/bin"
cp -Rp "$MB_ROOT/share" "$RES/mariadb/share"
if [[ -d "$MB_ROOT/lib" ]]; then
  cp -Rp "$MB_ROOT/lib" "$RES/mariadb/lib"
fi
find "$RES/mariadb" \( -name '*.pdb' -o -name '*.lib' -o -name '*.exp' \) -delete
rm -rf "$RES/mariadb/lib/debug" "$RES/mariadb/include" 2>/dev/null || true

# tauri-build copies these trees into target/*/build/resources on the next
# cargo build. Files extracted from the Windows zips arrive read-only on
# macOS, which makes that copy fail with EACCES on every fresh stage — make
# the whole tree writable. The JRE's AOT cache files (classes*.jsa) are
# optional (the JVM regenerates them lazily) and only bloat the installer.
chmod -R u+w "$RES"
rm -f "$RES"/jre/bin/server/classes.jsa "$RES"/jre/bin/server/classes_nocoops.jsa

rm -rf "$STAGE"

echo "   JRE:     $(du -sh "$RES/jre" | cut -f1)"
echo "   MariaDB: $(du -sh "$RES/mariadb" | cut -f1)"
echo "   JAR:     $(du -sh "$RES/jar/kiosk.jar" | cut -f1)"
echo "══ Resources-windows ready — build with: ══"
echo "   cd desktop/src-tauri && cargo tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc"
