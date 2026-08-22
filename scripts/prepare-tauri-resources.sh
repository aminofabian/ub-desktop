#!/usr/bin/env bash
# Populates desktop/src-tauri/Resources/ with jlink JRE, MariaDB 10.11, bootJar,
# and application-desktop.properties before `cargo tauri build`.
#
# See DESKTOP_INSTALLATION.md §§7–8 and §12.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RES="$ROOT/desktop/src-tauri/Resources"
BACKEND="$ROOT/backend"
FRONTEND="$ROOT/frontend"

echo "══ Prepare Tauri bundle resources ══"

# The Tauri shell embeds desktop/dist/ into the exe/app at build time and
# shows it while MariaDB + the JVM boot. A missing page means a blank window,
# so materialise it from the committed stub when absent.
if [[ ! -f "$ROOT/desktop/dist/index.html" ]]; then
  mkdir -p "$ROOT/desktop/dist"
  cp "$SCRIPT_DIR/splash-stub.html" "$ROOT/desktop/dist/index.html"
  echo "   splash: copied splash-stub.html -> desktop/dist/index.html"
fi

echo "── 1. Frontend static export ──"
cd "$FRONTEND"
bun run build:desktop

echo "── 2. Backend bootJar + jlink JRE + MariaDB ──"
cd "$BACKEND"
./gradlew bootJar downloadMariaDb jlinkJre -Pdesktop=true --no-daemon

JAR="$(ls -1 "$BACKEND/build/libs"/kiosk-desktop-*.jar 2>/dev/null | head -1)"
if [[ -z "$JAR" || ! -f "$JAR" ]]; then
  echo "error: kiosk-desktop bootJar not found under $BACKEND/build/libs" >&2
  exit 1
fi
if [[ ! -d "$BACKEND/build/jre/bin" ]]; then
  echo "error: jlink JRE missing at $BACKEND/build/jre (check JDK 21+ has jlink)" >&2
  exit 1
fi
if [[ ! -f "$BACKEND/build/mariadb/bin/mariadbd" && ! -f "$BACKEND/build/mariadb/mariadbd" ]]; then
  echo "error: MariaDB binaries missing under $BACKEND/build/mariadb" >&2
  exit 1
fi

echo "── 3. Copy into $RES ──"
rm -rf "$RES/jre" "$RES/mariadb" "$RES/jar"
mkdir -p "$RES/config" "$RES/jar"

cp -Rp "$BACKEND/build/jre" "$RES/jre"
cp "$BACKEND/src/main/resources/application-desktop.properties" "$RES/config/application.properties"
cp "$JAR" "$RES/jar/kiosk.jar"

MB_SRC="$BACKEND/build/mariadb"
mkdir -p "$RES/mariadb"
if [[ -d "$MB_SRC/bin" ]]; then
  cp -Rp "$MB_SRC/bin" "$MB_SRC/lib" "$MB_SRC/share" "$RES/mariadb/"
else
  cp -Rp "$MB_SRC"/* "$RES/mariadb/"
fi

chmod +x "$RES/mariadb"/bin/* 2>/dev/null || chmod +x "$RES/mariadb"/* 2>/dev/null || true
chmod +x "$RES/jre/bin/java" 2>/dev/null || true

echo "   JRE:    $(du -sh "$RES/jre" | cut -f1)"
echo "   MariaDB: $(du -sh "$RES/mariadb" | cut -f1)"
echo "   JAR:    $(du -sh "$RES/jar/kiosk.jar" | cut -f1)"
echo "══ Resources ready for Tauri bundle ══"
