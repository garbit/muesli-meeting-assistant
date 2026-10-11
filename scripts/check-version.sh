#!/usr/bin/env bash
# Checks that every place the version is written agrees, and (optionally) that it equals a tag.
# Usage: scripts/check-version.sh [vX.Y.Z | X.Y.Z]
# Runs on macOS and Linux: bash, sed, awk, grep and node only.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

expected="${1:-}"
expected="${expected#v}"
semver='^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'

# [workspace.package] version is the first `version = "..."` line; dependency lines are inline tables.
cargo_v="$(sed -nE 's/^version = "([^"]+)"$/\1/p' Cargo.toml | head -n1)"
lock_app="$(awk '/^name = "muesli"$/ { getline; sub(/^version = "/, ""); sub(/"$/, ""); print; exit }' Cargo.lock)"
lock_llm="$(awk '/^name = "muesli-llm"$/ { getline; sub(/^version = "/, ""); sub(/"$/, ""); print; exit }' Cargo.lock)"
pkg_v="$(node -p 'require("./package.json").version')"
# tauri.conf.json normally omits `version` and inherits Cargo.toml; only check it when present.
tauri_v="$(node -p 'require("./src-tauri/tauri.conf.json").version ?? ""')"
swift_v="$(sed -nE 's/^let sidecarVersion = "([^"]+)"$/\1/p' sidecar/Sources/MuesliSidecar/Main.swift)"
plist_v="$(grep -A1 '<key>CFBundleVersion</key>' sidecar/Sources/MuesliSidecar/Info.plist \
  | sed -nE 's/.*<string>([^<]+)<\/string>.*/\1/p')"

status=0
fail() {
  echo "::error::$*" >&2
  status=1
}

printf '%-28s %s\n' "source" "version"
printf '%-28s %s\n' "Cargo.toml (workspace)" "${cargo_v:-?}"
printf '%-28s %s\n' "Cargo.lock muesli" "${lock_app:-?}"
printf '%-28s %s\n' "Cargo.lock muesli-llm" "${lock_llm:-?}"
printf '%-28s %s\n' "package.json" "${pkg_v:-?}"
printf '%-28s %s\n' "src-tauri/tauri.conf.json" "${tauri_v:-(inherits Cargo.toml)}"
printf '%-28s %s\n' "sidecar Main.swift" "${swift_v:-?}"
printf '%-28s %s\n' "sidecar Info.plist" "${plist_v:-?}"

[[ "$cargo_v" =~ $semver ]] || fail "Cargo.toml version '$cargo_v' is not a semver version"
[ "$lock_app" = "$cargo_v" ] || fail "Cargo.lock has muesli $lock_app, expected $cargo_v (run: cargo update --workspace --offline)"
[ "$lock_llm" = "$cargo_v" ] || fail "Cargo.lock has muesli-llm $lock_llm, expected $cargo_v (run: cargo update --workspace --offline)"
[ "$pkg_v" = "$cargo_v" ] || fail "package.json is $pkg_v, expected $cargo_v"
[ -z "$tauri_v" ] || [ "$tauri_v" = "$cargo_v" ] || fail "tauri.conf.json is $tauri_v, expected $cargo_v (or remove the key to inherit Cargo.toml)"
[ "$swift_v" = "$cargo_v" ] || fail "sidecar Main.swift is $swift_v, expected $cargo_v"
# CFBundleVersion must be plain digits and dots, so a prerelease suffix is dropped there.
[ "$plist_v" = "${cargo_v%%-*}" ] || fail "sidecar Info.plist CFBundleVersion is $plist_v, expected ${cargo_v%%-*}"
if [ -n "$expected" ]; then
  [ "$cargo_v" = "$expected" ] || fail "tag v$expected does not match the version $cargo_v in the sources"
fi

if [ "$status" -eq 0 ]; then
  echo "versions OK: $cargo_v${expected:+ (matches tag v$expected)}"
fi
exit "$status"
