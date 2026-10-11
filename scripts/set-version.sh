#!/usr/bin/env bash
# Sets the app version everywhere it is written, refreshes Cargo.lock and verifies the result.
# Never commits or tags.
# Usage: scripts/set-version.sh X.Y.Z[-pre]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

v="${1:?usage: scripts/set-version.sh X.Y.Z[-pre]}"
v="${v#v}"
[[ "$v" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || { echo "not a semver version: $v" >&2; exit 2; }

# Cargo.toml: only the first `version = "..."` line, which is [workspace.package]. Both crates inherit it.
perl -pi -e 'if (!$done && s/^version = "[^"]+"$/version = "'"$v"'"/) { $done = 1 }' Cargo.toml

# JSON files: rewrite the key in place, keeping two-space indentation and the trailing newline.
set_json_version() {
  node -e '
    const fs = require("fs");
    const [path, version, onlyIfPresent] = process.argv.slice(1);
    const json = JSON.parse(fs.readFileSync(path, "utf8"));
    if (onlyIfPresent === "1" && !("version" in json)) process.exit(0);
    json.version = version;
    fs.writeFileSync(path, JSON.stringify(json, null, 2) + "\n");
  ' "$@"
}
set_json_version package.json "$v" 0
# tauri.conf.json normally inherits the version from Cargo.toml; only rewrite it if the key exists.
set_json_version src-tauri/tauri.conf.json "$v" 1

perl -pi -e 's/^let sidecarVersion = "[^"]+"/let sidecarVersion = "'"$v"'"/' sidecar/Sources/MuesliSidecar/Main.swift
# CFBundleVersion allows only digits and dots: drop any prerelease suffix.
perl -0pi -e 's{(<key>CFBundleVersion</key>\s*<string>)[^<]*}{${1}'"${v%%-*}"'}' sidecar/Sources/MuesliSidecar/Info.plist

# Refresh the two workspace entries in Cargo.lock without touching other dependencies.
cargo update --workspace --offline

scripts/check-version.sh "$v"

cat <<EOF

Next:
  git add -A && git commit -m "Release v$v"
  # merge into main, then tag the merged commit:
  git tag -a v$v -m "Muesli v$v" && git push origin v$v
EOF
