#!/usr/bin/env bash
# Build a debug bundle and (re)install it into /Applications, replacing the running copy.
# Usage: scripts/install-local.sh [--no-build] [--reset-tcc]
#   --reset-tcc  forget the Microphone / System Audio grants first (scripts/tcc-reset.sh),
#                so the new build gets clean prompts instead of a stale grant that
#                macOS reports as denied.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
BUILD=1
RESET_TCC=0
for arg in "$@"; do
  case "$arg" in
    --no-build) BUILD=0 ;;
    --reset-tcc) RESET_TCC=1 ;;
    *) echo "usage: $0 [--no-build] [--reset-tcc]" >&2; exit 2 ;;
  esac
done
if [ "$BUILD" = 1 ]; then
  pnpm tauri build --debug
fi
SRC="$ROOT/target/debug/bundle/macos/Muesli.app"
[ -d "$SRC" ] || { echo "no bundle at $SRC"; exit 1; }
osascript -e 'tell application "Muesli" to quit' >/dev/null 2>&1 || true
sleep 1
rm -rf "/Applications/Muesli.app"
ditto "$SRC" "/Applications/Muesli.app"
echo "installed /Applications/Muesli.app ($(du -sh /Applications/Muesli.app | cut -f1))"
if [ "$RESET_TCC" = 1 ]; then
  "$ROOT/scripts/tcc-reset.sh"
fi
echo "note: an ad-hoc signed rebuild is a new app to macOS; Muesli asks for Microphone and System Audio Recording again on first launch."
open -a "/Applications/Muesli.app"
