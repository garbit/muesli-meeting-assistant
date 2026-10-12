#!/usr/bin/env bash
# Reset the privacy grants macOS keeps for Muesli. Useful in development because
# every ad-hoc signed rebuild looks like a new app to TCC and the prompts stop
# appearing once a stale grant exists.
set -euo pipefail
ID="${1:-dev.garbett.muesli}"
for svc in Microphone AudioCapture ScreenCapture; do
  tccutil reset "$svc" "$ID" 2>/dev/null && echo "reset $svc for $ID" || echo "could not reset $svc for $ID (may not exist)"
done
