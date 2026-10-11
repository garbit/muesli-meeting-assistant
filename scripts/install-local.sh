#!/usr/bin/env bash
# Build a debug bundle and (re)install it into /Applications, replacing the running copy.
# Usage: scripts/install-local.sh [--no-build]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
if [ "${1:-}" != "--no-build" ]; then
  pnpm tauri build --debug
fi
SRC="$ROOT/target/debug/bundle/macos/Muesli.app"
[ -d "$SRC" ] || { echo "no bundle at $SRC"; exit 1; }
osascript -e 'tell application "Muesli" to quit' >/dev/null 2>&1 || true
sleep 1
rm -rf "/Applications/Muesli.app"
ditto "$SRC" "/Applications/Muesli.app"
echo "installed /Applications/Muesli.app ($(du -sh /Applications/Muesli.app | cut -f1))"
echo "note: an ad-hoc signed rebuild is a new app to macOS; re-grant Microphone and System Audio Recording if prompted."
open -a "/Applications/Muesli.app"
