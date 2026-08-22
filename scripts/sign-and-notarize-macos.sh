#!/usr/bin/env bash
# Sign and notarize a macOS .dmg built by Tauri (DESKTOP_INSTALLATION.md §12).
#
# Prerequisites:
#   - Apple Developer ID Application certificate in Keychain
#   - notarytool profile: xcrun notarytool store-credentials "palmart-notary" ...
#
# Usage:
#   export APPLE_SIGNING_IDENTITY="Developer ID Application: Your Co (TEAMID)"
#   export APPLE_NOTARIZE_PROFILE="palmart-notary"
#   ./desktop/scripts/sign-and-notarize-macos.sh path/to/Palmart_0.0.1_aarch64.dmg

set -euo pipefail

DMG="${1:?Pass path to .dmg}"
IDENTITY="${APPLE_SIGNING_IDENTITY:?Set APPLE_SIGNING_IDENTITY}"
NOTARY_PROFILE="${APPLE_NOTARIZE_PROFILE:-palmart-notary}"
ENTITLEMENTS="$(cd "$(dirname "$0")/.." && pwd)/entitlements/macos/entitlements.plist"

echo "==> Signing $DMG"
codesign --force --options runtime --entitlements "$ENTITLEMENTS" --sign "$IDENTITY" "$DMG"

echo "==> Submitting for notarization"
xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" --wait

echo "==> Stapling ticket"
xcrun stapler staple "$DMG"

echo "Done: $DMG"
