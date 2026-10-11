#!/usr/bin/env bash
# Builds the Swift sidecar and places it where Tauri's externalBin expects it.
# Usage: scripts/build-sidecar.sh [debug|release]   (default: release)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CONFIG="${1:-release}"
TRIPLE="$(rustc --print host-tuple 2>/dev/null || echo aarch64-apple-darwin)"
cd "$ROOT/sidecar"
swift build -c "$CONFIG" --product muesli-sidecar
mkdir -p "$ROOT/src-tauri/binaries"
cp -f ".build/$CONFIG/muesli-sidecar" "$ROOT/src-tauri/binaries/muesli-sidecar-$TRIPLE"
echo "sidecar → src-tauri/binaries/muesli-sidecar-$TRIPLE ($CONFIG)"
