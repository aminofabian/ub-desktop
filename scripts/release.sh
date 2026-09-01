#!/usr/bin/env bash
# ── Kiosk Desktop — one-command release ────────────────────────────────────────
# Builds the Windows installer (and optionally macOS), uploads it to GitHub
# Releases, and republishes the site's download manifest so the homepage
# download button serves the new build.
#
# Usage (run from the repo root):
#   bash desktop/scripts/release.sh                 # Windows installer only
#   bash desktop/scripts/release.sh --macos         # Windows + macOS (host arch)
#   bash desktop/scripts/release.sh --no-upload     # build + stage, skip upload
#
# Env overrides:
#   GITHUB_REPO=owner/repo          default: detected from the frontend git remote
#   RELEASE_VERSION=0.0.2           default: desktop/src-tauri/tauri.conf.json
#   WEBVIEW2_INSTALLER_PATH=/path   pre-downloaded WebView2 installer
#
# Windows code signing (optional — removes the SmartScreen "Run anyway" step):
#   WINDOWS_SIGN_PFX_PATH=/path/cert.pfx   path to a code-signing certificate (PKCS#12)
#   WINDOWS_SIGN_PFX_PASSWORD=secret       password for that .pfx (omit if no password)
#   WINDOWS_SIGN_TIMESTAMP_URL=url         RFC3161 timestamp server (default: DigiCert)
#
# Prerequisites (one-time):
#   brew install nsis llvm lld bun
#   rustup target add x86_64-pc-windows-msvc
#   cargo install cargo-xwin --locked
#   gh auth login                    (only needed for the upload step)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
DESKTOP="$ROOT/desktop"
SRC="$DESKTOP/src-tauri"
FRONTEND="$ROOT/frontend"
BACKEND="$ROOT/backend"

BUILD_MACOS=0
DO_UPLOAD=1
for arg in "$@"; do
  case "$arg" in
    --macos) BUILD_MACOS=1 ;;
    --no-upload) DO_UPLOAD=0 ;;
    -h|--help)
      sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) echo "unknown argument: $arg" >&2; exit 1 ;;
  esac
done

CONF_VERSION="$(python3 -c "import json; print(json.load(open('$SRC/tauri.conf.json'))['version'])")"
VERSION="${RELEASE_VERSION:-$CONF_VERSION}"
if [ -n "${RELEASE_VERSION:-}" ] && [ "$RELEASE_VERSION" != "$CONF_VERSION" ]; then
  echo "  bumping tauri.conf.json version: $CONF_VERSION → $RELEASE_VERSION"
  python3 - "$SRC/tauri.conf.json" "$RELEASE_VERSION" <<'PY'
import json, sys
p, v = sys.argv[1], sys.argv[2]
cfg = json.load(open(p))
cfg["version"] = v
json.dump(cfg, open(p, "w"), indent=2, ensure_ascii=False)
PY
fi
RELEASE_TAG="desktop-v$VERSION"
GH_REPO="${GITHUB_REPO:-$(git -C "$FRONTEND" remote get-url origin 2>/dev/null | sed -nE 's#.*github.com[:/]([^/]+/[^/.]+)(\.git)?$#\1#p')}"

say() { printf "\n\033[1m── %s ──\033[0m\n" "$1"; }
die() { echo "error: $*" >&2; exit 1; }

echo "══ Kiosk Desktop release v$VERSION ══"
echo "  tag:   $RELEASE_TAG"
echo "  repo:  ${GH_REPO:-(not detected — set GITHUB_REPO for the upload step)}"
echo ""

# ── Preflight ──────────────────────────────────────────────────────────────
say "Preflight"
for tool in bun curl python3 unzip java cargo cargo-xwin; do
  command -v "$tool" >/dev/null 2>&1 || die "missing tool: $tool"
done
command -v gh >/dev/null 2>&1 || echo "  (gh CLI not found — upload + republish will be skipped)"
if [ -n "${WINDOWS_SIGN_PFX_PATH:-}" ]; then
  command -v osslsigncode >/dev/null 2>&1 || die "WINDOWS_SIGN_PFX_PATH is set but osslsigncode is missing — install with: brew install osslsigncode"
  echo "  code-signing: enabled ($WINDOWS_SIGN_PFX_PATH)"
else
  echo "  code-signing: disabled (set WINDOWS_SIGN_PFX_PATH to sign the installer)"
fi
[ -f "$BACKEND/gradlew" ] || die "backend/gradlew not found"
[ -d "$FRONTEND" ] || die "frontend/ not found"
# License key bake check — warn (not fail) so a dev/trial-only build can still ship.
if grep -Eq '^app\.desktop\.license\.public-key=\$\{APP_DESKTOP_LICENSE_PUBLIC_KEY:\}$' \
    "$BACKEND/src/main/resources/application-desktop.properties" 2>/dev/null; then
  echo "  ⚠ desktop license public key is NOT baked — shipped tills will be trial-only."
  echo "    Run once: bash backend/scripts/generate-license.sh bootstrap"
fi
echo "  ok."

# ── 1. Frontend static export ──────────────────────────────────────────────
# The JAR bundles this into its classpath, so it must be built BEFORE bootJar.
say "1/7  Frontend static export"
(cd "$FRONTEND" && bun run build:desktop)
[ -d "$FRONTEND/out" ] || die "frontend/out missing after build"

# ── 2. Backend bootJar ─────────────────────────────────────────────────────
# `clean` is deliberate: a stale jar (e.g. an old checkout or a cached
# bootJar) previously shipped in a Windows installer. Rebuild from scratch so
# the jar always matches the backend sources at HEAD.
say "2/7  Backend JAR (includes the UI + desktop profile)"
(cd "$BACKEND" && ./gradlew clean bootJar -Pdesktop=true --no-daemon)
echo "  backend HEAD: $(git -C "$BACKEND" log -1 --format='%h %cd' --date=short 2>/dev/null || echo 'n/a')"
JAR="$(ls -1t "$BACKEND/build/libs"/kiosk-desktop-*.jar 2>/dev/null | head -1)"
[ -n "$JAR" ] || die "kiosk-desktop bootJar not produced"
echo "  jar: $JAR ($(stat -f '%Sm' "$JAR" 2>/dev/null || echo 'unknown mtime'))"

# ── 3. WebView2 installer for offline installs ─────────────────────────────
say "3/7  WebView2 installer"
WB="$SRC/bundle/windows/MicrosoftEdgeWebview2Setup.exe"
if [ ! -f "$WB" ]; then
  mkdir -p "$(dirname "$WB")"
  if [ -n "${WEBVIEW2_INSTALLER_PATH:-}" ] && [ -f "$WEBVIEW2_INSTALLER_PATH" ]; then
    echo "  copying WEBVIEW2_INSTALLER_PATH → $WB"
    cp "$WEBVIEW2_INSTALLER_PATH" "$WB"
  else
    echo "  downloading the WebView2 bootstrapper (verified link, ~2 MB)…"
    curl -fL --retry 3 -o "$WB" "https://go.microsoft.com/fwlink/p/?LinkId=2124703" \
      || die "download failed. Download the Evergreen Standalone Installer from" \
             "https://developer.microsoft.com/microsoft-edge/webview2/ and save it as $WB"
  fi
fi
WB_SIZE="$(wc -c < "$WB")"
file "$WB" | grep -qi "PE32" || die "$WB is not a Windows PE executable"
if [ "$WB_SIZE" -lt 52428800 ]; then
  echo "  ⚠ WARNING: $WB is only $(du -h "$WB" | cut -f1) — that's the tiny online"
  echo "    bootstrapper, not the ~127 MB standalone installer. The installer will"
  echo "    still work on machines that already have WebView2 (Win10/11 do), but a"
  echo "    fully offline install needs the 'Evergreen Standalone Installer' from"
  echo "    https://developer.microsoft.com/microsoft-edge/webview2/ saved as $WB"
fi
echo "  ok ($(du -h "$WB" | cut -f1))."

# ── 4. Stage Windows resources (JRE + MariaDB + jar + config) ──────────────
say "4/7  Stage Windows resources"
bash "$DESKTOP/scripts/prepare-windows-resources.sh"

# ── 5. Build the Windows installer (cross-compile) ─────────────────────────
say "5/7  Windows installer (cargo-xwin cross-build)"
# tauri-build copies the resources (JRE/MariaDB/jar) into target/*/build/resources
# preserving their read-only permissions, so the NEXT build fails with EACCES
# trying to overwrite them. Clear the stale copies first — this bites every
# second build after the first.
rm -rf "$SRC"/target/*/debug/build/resources "$SRC"/target/*/release/build/resources 2>/dev/null || true
if [ "$(uname)" = "Darwin" ]; then
  LLVM_BIN="$(brew --prefix llvm 2>/dev/null)/bin"
  LLD_BIN="$(brew --prefix lld 2>/dev/null)/bin"
  export PATH="${LLVM_BIN:+$LLVM_BIN:}${LLD_BIN:+$LLD_BIN:}$PATH"
fi
(cd "$SRC" && cargo tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc)
WIN_EXE="$(ls -t "$SRC"/target/x86_64-pc-windows-msvc/release/bundle/nsis/*-setup.exe 2>/dev/null | head -1)"
[ -n "$WIN_EXE" ] && [ -f "$WIN_EXE" ] || die "Windows installer not produced"
echo "  → $WIN_EXE"

# Sign the installer when a PKCS#12 certificate is provided. This is what makes
# SmartScreen show the publisher instead of "More info → Run anyway". Runs after
# the NSIS bundle is produced so the normalized/uploaded artifact is signed.
if [ -n "${WINDOWS_SIGN_PFX_PATH:-}" ]; then
  TS_URL="${WINDOWS_SIGN_TIMESTAMP_URL:-http://timestamp.digicert.com}"
  say "Signing the Windows installer (osslsigncode)"
  SIGN_ARGS=(-pkcs12 "$WINDOWS_SIGN_PFX_PATH" -t "$TS_URL" -h sha256)
  if [ -n "${WINDOWS_SIGN_PFX_PASSWORD:-}" ]; then
    SIGN_ARGS+=(-pass "$WINDOWS_SIGN_PFX_PASSWORD")
  fi
  osslsigncode sign "${SIGN_ARGS[@]}" -in "$WIN_EXE" -out "$WIN_EXE.signed"
  mv "$WIN_EXE.signed" "$WIN_EXE"
  echo "  signed → $WIN_EXE"
else
  echo "  (unsigned — SmartScreen will prompt 'More info → Run anyway')"
fi

# ── 6. macOS installer (optional) ──────────────────────────────────────────
MAC_DMG=""
if [ "$BUILD_MACOS" = 1 ]; then
  say "6/7  macOS app + dmg (host arch — aarch64)"
  [ "$(uname)" = "Darwin" ] || die "--macos requires building on macOS"
  # Same EACCES trap as the Windows step, but tauri-build stages the macOS
  # copies directly under target/release/{jre,mariadb,jar} and copies them
  # (read-only) into the crate's build output dir — clear all of them so this
  # (and the next) build can overwrite the read-only files.
  rm -rf "$SRC"/target/release/jre "$SRC"/target/release/mariadb \
    "$SRC"/target/release/jar \
    "$SRC"/target/release/build/kiosk-desktop-*/out \
    "$SRC"/target/release/bundle/dmg 2>/dev/null || true
  # A previous failed DMG attempt can leave its rw.* temp image mounted
  # (hdiutil then can't unmount the next attempt: "Resource busy"). Detach
  # any strays before bundling.
  for v in /Volumes/dmg.*; do
    [ -d "$v" ] && hdiutil detach "$v" -force >/dev/null 2>&1 || true
  done
  (cd "$SRC" && cargo tauri build)
  MAC_DMG="$(ls -t "$SRC"/target/release/bundle/dmg/*.dmg 2>/dev/null | head -1)"
  [ -n "$MAC_DMG" ] && [ -f "$MAC_DMG" ] || die "macOS dmg not produced"
  echo "  → $MAC_DMG"
fi

# ── 7. Move stale loose installers aside ───────────────────────────────────
# The publish script also scans desktop/*.{exe,dmg,msi}; move old loose
# artifacts away so only fresh bundle output is published. (In particular the
# old hollow 2.6 MB Kiosk Desktop_0.0.1_aarch64.dmg must never be republished.)
say "7/7  Prepare publish"
STALE="$DESKTOP/.cache/stale-installers"
mkdir -p "$STALE"
for f in "$DESKTOP"/*.exe "$DESKTOP"/*.dmg "$DESKTOP"/*.msi; do
  [ -f "$f" ] && { mv "$f" "$STALE/"; echo "  moved stale $(basename "$f") → .cache/stale-installers/"; }
done

# Upload under the NORMALIZED names that the download manifest references
# (pack:desktop-downloads renames bundle output to
# kiosk-desktop-<version>-<os>-<arch>.<ext>). Uploading the raw bundle name
# (e.g. Kiosk_0.0.1_x64-setup.exe) made the manifest URL 404 before.
WIN_NORM="$(dirname "$WIN_EXE")/kiosk-desktop-$VERSION-windows-x86_64.exe"
cp "$WIN_EXE" "$WIN_NORM"
echo "  normalized → $(basename "$WIN_NORM")"
ASSETS=("$WIN_NORM")
if [ -n "$MAC_DMG" ]; then
  MAC_NORM="$(dirname "$MAC_DMG")/kiosk-desktop-$VERSION-macos-aarch64.dmg"
  cp "$MAC_DMG" "$MAC_NORM"
  echo "  normalized → $(basename "$MAC_NORM")"
  ASSETS+=("$MAC_NORM")
fi

if [ "$DO_UPLOAD" = 0 ]; then
  echo ""
  echo "── --no-upload: built + staged, not published ──"
  echo "  Upload manually with:"
  printf '    gh release create %s' "$RELEASE_TAG"
  for a in "${ASSETS[@]}"; do printf ' "%s"' "$a"; done
  echo ""
  echo "  then republish:"
  if [ -n "$GH_REPO" ]; then
    echo "    (cd frontend && DESKTOP_DOWNLOAD_BASE_URL=https://github.com/$GH_REPO/releases/download/$RELEASE_TAG bun run pack:desktop-downloads)"
  else
    echo "    (cd frontend && DESKTOP_DOWNLOAD_BASE_URL=https://github.com/<owner>/<repo>/releases/download/$RELEASE_TAG bun run pack:desktop-downloads)"
  fi
  echo ""
  echo "══ Done (no upload). ══"
  exit 0
fi

[ -n "$GH_REPO" ] || die "can't detect GITHUB_REPO — set it or use --no-upload"
command -v gh >/dev/null 2>&1 || die "gh CLI not installed (or use --no-upload)"
gh auth status >/dev/null 2>&1 || die "gh is not authenticated (or use --no-upload)"

# ── Upload to GitHub Releases ──────────────────────────────────────────────
say "Uploading to GitHub Releases ($GH_REPO @ $RELEASE_TAG)"
NOTES="Kiosk Desktop v$VERSION — the offline-first POS.

- One installer ships the POS, database, and Java runtime — nothing else to install.
- Fully offline: bundles MariaDB, a JRE, and the WebView2 runtime.
- ${#ASSETS[@]} platform installer(s): ${ASSETS[*]}"
if gh release view "$RELEASE_TAG" --repo "$GH_REPO" >/dev/null 2>&1; then
  gh release upload "$RELEASE_TAG" "${ASSETS[@]}" --repo "$GH_REPO" --clobber
  echo "  release existed — uploaded ${#ASSETS[@]} asset(s)."
else
  gh release create "$RELEASE_TAG" "${ASSETS[@]}" --repo "$GH_REPO" \
    --title "Kiosk Desktop v$VERSION" --notes "$NOTES"
  echo "  release created."
fi
RELEASE_URL="https://github.com/$GH_REPO/releases/download/$RELEASE_TAG"

# ── Republish the download manifest (homepage button) ──────────────────────
say "Republishing the site download manifest"
(cd "$FRONTEND" && DESKTOP_DOWNLOAD_BASE_URL="$RELEASE_URL" bun run pack:desktop-downloads)

echo ""
echo "══ Done! ══"
echo "  Release:   https://github.com/$GH_REPO/releases/tag/$RELEASE_TAG"
echo "  Manifest:  frontend/public/downloads/desktop/manifest.json (now points at the release)"
echo ""
echo "  Next steps:"
echo "   1. Commit the manifest (and any small installers) in the frontend repo and deploy."
echo "   2. The homepage button then downloads the new installer directly."
echo "   3. Sign the Windows installer for SmartScreen if you haven't (bundle > windows > signCommand)."
