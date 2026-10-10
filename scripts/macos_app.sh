#!/usr/bin/env bash
# Wrap the server in "Go2 Ctrl.app", so macOS can grant it Location (Wi-Fi names, src/macos_wifi.rs).
# usage: scripts/macos_app.sh <dimos-app-server binary> <output dir>   (the flake's darwin backend runs this too)
set -euo pipefail
here="$(cd -- "$(dirname -- "$0")/.." && pwd)"
app="$2/Go2 Ctrl.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
install -m755 "$1" "$app/Contents/MacOS/dimos-app-server"
install -m644 "$here/backend/macos/Info.plist" "$app/Contents/Info.plist"
# ad-hoc, sealing the Info.plist in: Location remembers the app by its signature
codesign --force --sign - "$app"
