#!/usr/bin/env bash
# Install a LaunchAgent so Palmart Desktop starts at login (§12 optional).
#
# Usage:
#   ./desktop/scripts/install-macos-launch-agent.sh /Applications/Palmart.app

set -euo pipefail

APP="${1:?Pass path to Palmart.app}"
PLIST="$HOME/Library/LaunchAgents/com.palmart.desktop.plist"
EXEC="$APP/Contents/MacOS/kiosk-desktop"

if [[ ! -x "$EXEC" ]]; then
  echo "Executable not found: $EXEC" >&2
  exit 1
fi

mkdir -p "$(dirname "$PLIST")"
cat >"$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.palmart.desktop</string>
  <key>ProgramArguments</key>
  <array>
    <string>$EXEC</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <false/>
</dict>
</plist>
EOF

launchctl unload "$PLIST" 2>/dev/null || true
launchctl load "$PLIST"
echo "Installed $PLIST — Palmart will start at login."
