#!/usr/bin/env bash
# Sign and notarize a macOS desktop build produced by `cargo tauri build`.
#
# The previous version only ran codesign on the .dmg, which does not produce a
# notarizable app: Gatekeeper assesses the .app inside, so the .app (and its
# nested helpers) must be signed with a Developer ID and hardened runtime
# *before* the .dmg is notarized and stapled. This does that.
#
# Prerequisites:
#   - Apple "Developer ID Application" certificate in the login keychain
#   - a stored notarytool profile:  xcrun notarytool store-credentials \
#         "kiosk-notary" --apple-id … --team-id … --password …
#
# Usage:
#   export APPLE_SIGNING_IDENTITY="Developer ID Application: Your Co (TEAMID)"
#   export APPLE_NOTARIZE_PROFILE="kiosk-notary"
#   ./desktop/scripts/sign-and-notarize-macos.sh path/to/Kiosk_0.0.1_aarch64.dmg
#
# The matching .app is found next to the .dmg (…/bundle/macos/*.app); pass it as
# the second argument to override.

set -euo pipefail

DMG="${1:?Pass path to the .dmg}"
IDENTITY="${APPLE_SIGNING_IDENTITY:?Set APPLE_SIGNING_IDENTITY}"
NOTARY_PROFILE="${APPLE_NOTARIZE_PROFILE:-kiosk-notary}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ENTITLEMENTS="$SCRIPT_DIR/../entitlements/macos/entitlements.plist"

# Locate the .app that lives inside this .dmg's bundle directory.
APP="${2:-}"
if [[ -z "$APP" ]]; then
  APP="$(ls -d "$(dirname "$DMG")"/../macos/*.app 2>/dev/null | head -1 || true)"
fi

if [[ -n "$APP" && -d "$APP" ]]; then
  echo "==> Signing the app (hardened runtime): $APP"
  # Sign nested code first, then the bundle. --options runtime + entitlements is
  # what notarization requires; --timestamp gives a secure timestamp.
  codesign --force --timestamp --options runtime \
    --entitlements "$ENTITLEMENTS" --sign "$IDENTITY" \
    --deep "$APP"
  echo "==> Verifying the app signature"
  codesign --verify --deep --strict --verbose=2 "$APP"
else
  echo "!! No .app found next to $DMG — notarization will likely fail." >&2
  echo "   Re-run with the .app path as the second argument." >&2
fi

# The .dmg itself is signed too, so the disk image is trusted before it is
# mounted; this is separate from the app signature.
echo "==> Signing the disk image"
codesign --force --timestamp --sign "$IDENTITY" "$DMG"

echo "==> Submitting the disk image for notarization (this can take a few minutes)"
xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" --wait

echo "==> Stapling the notarization ticket"
xcrun stapler staple "$DMG"
if [[ -n "$APP" && -d "$APP" ]]; then
  xcrun stapler staple "$APP"
fi

echo "==> Gatekeeper assessment"
spctl --assess --type install --verbose=2 "$DMG" || true

echo "Done: $DMG"
