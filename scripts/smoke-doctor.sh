#!/usr/bin/env bash
# Smoke-tests a built Muesli.app without launching the UI: runs `muesli --doctor` against a
# throwaway library and checks that the app and the bundled sidecar start and report the
# expected version. Needs no models and no privacy permissions, so it runs on CI.
# Usage: scripts/smoke-doctor.sh <path/to/Muesli.app> [expected-version]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

APP="${1:?usage: scripts/smoke-doctor.sh <Muesli.app> [expected-version]}"
EXPECTED="${2:-$(sed -nE 's/^version = "([^"]+)"$/\1/p' "$ROOT/Cargo.toml" | head -n1)}"
BIN="$APP/Contents/MacOS/muesli"
SIDECAR="$APP/Contents/MacOS/muesli-sidecar"
[ -x "$BIN" ] || { echo "not a Muesli bundle: missing $BIN" >&2; exit 1; }
[ -x "$SIDECAR" ] || { echo "sidecar missing from the bundle: $SIDECAR" >&2; exit 1; }

# A fresh library so the real ~/Library/Application Support/Muesli is never touched.
base="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
SCRATCH="$(mktemp -d "${base%/}/muesli-doctor.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT
export MUESLI_HOME="$SCRATCH"
# Models default to $MUESLI_HOME/models (empty); the sidecar must be the one inside the bundle.
unset MUESLI_MODELS_DIR MUESLI_SIDECAR

set +e
report="$("$BIN" --doctor 2>&1)"
code=$?
set -e
printf '%s\n' "$report"
echo "----"
[ "$code" -eq 0 ] || { echo "::error::smoke: doctor exited with status $code" >&2; exit 1; }

fail() {
  echo "::error::smoke: $*" >&2
  exit 1
}
# doctor exits 0 even when the sidecar fails, so the report itself is what we assert on.
grep -q "^version: $EXPECTED " <<<"$report" || fail "app version line is not $EXPECTED"
grep -qF "data: $SCRATCH " <<<"$report" || fail "doctor did not use the scratch library at $SCRATCH"
grep -q '^sidecar: {.*"ok":true' <<<"$report" || fail "no healthy sidecar line in the report"
grep -q "^sidecar: {.*\"version\":\"$EXPECTED\"" <<<"$report" || fail "sidecar version is not $EXPECTED"
if grep -Eq 'sidecar (unavailable|health failed)|models\.status failed' <<<"$report"; then
  fail "sidecar error in the report"
fi
echo "smoke OK: muesli $EXPECTED and the bundled sidecar are healthy"
