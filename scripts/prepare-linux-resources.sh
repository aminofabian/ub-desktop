#!/usr/bin/env bash
# Stages desktop/src-tauri/Resources-linux/ for the Linux AppImage/deb bundle:
#
#   jre/      — Temurin 21 JRE for Linux x64 (no cross-jlink needed)
#   mariadb/  — MariaDB 10.11 Linux x86_64 (bin/lib/share, trimmed of dev files)
#   jar/      — kiosk.jar (platform-independent)
#   config/   — application-desktop.properties
#
# Downloads are cached under desktop/.cache/ so re-runs are cheap.
# Build afterwards (on a Linux host / CI — Linux bundles can't be
# cross-compiled from macOS):
#   cd desktop/src-tauri
#   cargo tauri build --bundles appimage,deb
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
DESKTOP="$ROOT/desktop"
RES="$DESKTOP/src-tauri/Resources-linux"
CACHE="$DESKTOP/.cache"
BACKEND="$ROOT/backend"

MARIADB_VERSION="10.11.11"   # keep in sync with backend/build.gradle
JRE_API="https://api.adoptium.net/v3/assets/latest/21/hotspot?os=linux&architecture=x64&image_type=jre"
MARIADB_URL="https://archive.mariadb.org/mariadb-${MARIADB_VERSION}/bintar-linux-systemd-x86_64/mariadb-${MARIADB_VERSION}-linux-systemd-x86_64.tar.gz"

mkdir -p "$CACHE"

# ── 0. Tauri splash (frontendDist) ──────────────────────────────────────
# Same as the Windows/Mac path: the shell window shows this stub while
# MariaDB + the JVM boot, so materialise it from the committed stub when
# absent.
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

echo "══ Prepare Linux bundle resources ══"

echo "── 1. kiosk.jar ──"
# Prefer the freshly built backend jar (the CI/release flow runs `clean
# bootJar` first). Fall back to the macOS-staged copy.
JAR="$(ls -1 "$BACKEND/build/libs"/kiosk-desktop-*.jar 2>/dev/null | sort | tail -1 || true)"
if [[ -z "$JAR" || ! -f "$JAR" ]] && [[ -f "$DESKTOP/src-tauri/Resources/jar/kiosk.jar" ]]; then
  JAR="$DESKTOP/src-tauri/Resources/jar/kiosk.jar"
fi
if [[ -z "$JAR" || ! -f "$JAR" ]]; then
  echo "error: kiosk.jar not found. Build it first:" >&2
  echo "  cd backend && ./gradlew clean bootJar -Pdesktop=true" >&2
  exit 1
fi
echo "   using $JAR"

echo "── 2. Temurin 21 JRE (Linux x64) ──"
JRE_TGZ="$CACHE/temurin-21-jre-linux-x64.tar.gz"
if [[ ! -s "$JRE_TGZ" ]]; then
  JRE_URL="$(curl -fsSL "$JRE_API" | python3 -c '
import json, sys
assets = json.load(sys.stdin)
print(assets[0]["binary"]["package"]["link"])
')"
  echo "   resolved: $JRE_URL"
fi
fetch "${JRE_URL:-}" "$JRE_TGZ"

echo "── 3. MariaDB ${MARIADB_VERSION} (linux-x86_64) ──"
MB_TGZ="$CACHE/mariadb-${MARIADB_VERSION}-linux-x86_64.tar.gz"
fetch "$MARIADB_URL" "$MB_TGZ"

echo "── 4. Stage into $RES ──"
rm -rf "$RES"
mkdir -p "$RES/jar" "$RES/config"

cp "$JAR" "$RES/jar/kiosk.jar"
cp "$BACKEND/src/main/resources/application-desktop.properties" "$RES/config/application.properties"

STAGE="$CACHE/.stage-linux"
rm -rf "$STAGE"
mkdir -p "$STAGE"

# JRE — tarball has a single top-level dir (jdk-21.x.y+z-jre/).
tar -xzf "$JRE_TGZ" -C "$STAGE/jre" 2>/dev/null || { mkdir -p "$STAGE/jre" && tar -xzf "$JRE_TGZ" -C "$STAGE/jre"; }
JRE_ROOT="$(find "$STAGE/jre" -maxdepth 1 -mindepth 1 -type d | head -1)"
[[ -f "$JRE_ROOT/bin/java" ]] || { echo "error: java missing in JRE tarball" >&2; exit 1; }
mv "$JRE_ROOT" "$RES/jre"

# MariaDB — single top-level dir (mariadb-<version>-linux-systemd-x86_64/).
# Extract straight into the destination (no staging copy) to keep the disk
# footprint to a single copy; keep the runtime pieces, drop dev/docs bits.
tar -xzf "$MB_TGZ" -C "$RES"
MB_ROOT="$(find "$RES" -maxdepth 1 -mindepth 1 -type d -name 'mariadb-*' | head -1)"
[[ -f "$MB_ROOT/bin/mariadbd" ]] || { echo "error: mariadbd missing in MariaDB tarball" >&2; exit 1; }
mv "$MB_ROOT" "$RES/mariadb"
rm -rf "$RES/mariadb/include" "$RES/mariadb/lib/debug" \
  "$RES/mariadb/docs" "$RES/mariadb/man" "$RES/mariadb/scripts" \
  "$RES/mariadb/mysql-test" "$RES/mariadb/sql-bench" 2>/dev/null || true
find "$RES/mariadb" \( -name '*.a' -o -name '*.la' \) -delete 2>/dev/null || true
# The bintar ships with debug symbols (mariadbd is ~300 MB alone). Strip on
# Linux hosts (CI); macOS `strip` only understands Mach-O so make it best-effort.
find "$RES/mariadb/bin" -type f -exec strip --strip-debug {} \; 2>/dev/null || true
rm -rf "$STAGE"

# tauri-build copies these trees into target/release/{jre,mariadb,jar} on the
# next cargo build preserving read-only permissions, which makes the NEXT
# build fail with EACCES (this bit the macOS path before the release script
# started clearing the staged copies). Make everything writable and drop the
# optional JRE AOT caches (regenerated lazily).
chmod -R u+w "$RES"
rm -f "$RES"/jre/lib/server/classes.jsa "$RES"/jre/lib/server/classes_nocoops.jsa

echo "   JRE:     $(du -sh "$RES/jre" | cut -f1)"
echo "   MariaDB: $(du -sh "$RES/mariadb" | cut -f1)"
echo "   JAR:     $(du -sh "$RES/jar/kiosk.jar" | cut -f1)"
echo "══ Resources-linux ready — build on a Linux host with: ══"
echo "   cd desktop/src-tauri && cargo tauri build --bundles appimage,deb"
