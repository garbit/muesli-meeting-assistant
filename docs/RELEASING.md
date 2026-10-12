# Releasing Muesli

Releases are built by GitHub Actions on Apple Silicon `macos-26` runners. Pushing a `vX.Y.Z`
tag runs `.github/workflows/release.yml`, which validates the commit, builds the app with
updater artifacts and opens a **draft** GitHub release. Publishing the draft is a manual
step, and it is what makes the in-app updater offer the new version.

## What CI checks

`.github/workflows/ci.yml` runs on every pull request and push to `main`. It calls the
reusable `validate.yml`, whose jobs show up as:

| Check | Runner | What it does |
|---|---|---|
| `validate / versions` | ubuntu | `scripts/check-version.sh`: every version source agrees (and equals the tag on a release) |
| `validate / frontend` | ubuntu | `pnpm install --frozen-lockfile`, `pnpm build` (tsc + vite) |
| `validate / rust-check` | macos-26 | `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -D warnings`, `cargo test --workspace --lib` |
| `validate / bundle` | macos-26 | `pnpm tauri build` (Swift sidecar, frontend, Rust release profile, DMG), then `scripts/smoke-doctor.sh` on the bundle; the DMG is attached as the `muesli-dmg` artifact |

A cold run takes about 45 minutes (the bundle job dominates); with warm caches about 15.
To click-test a PR build, download `muesli-dmg` from the run, quit the installed Muesli first
(both share the bundle id and the app is single-instance), then right-click → Open.

## One-time setup

1. **Updater signing key.** The public key is embedded in `src-tauri/tauri.conf.json`; the
   private key is `~/.tauri/muesli.key` (no password). Store it as a repository secret and
   leave `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` unset:

   ```sh
   gh secret set TAURI_SIGNING_PRIVATE_KEY < ~/.tauri/muesli.key
   ```

   Confirm the embedded public key belongs to that private key (the `.pub` file is stored
   base64-encoded, exactly as it appears in the config):

   ```sh
   [ "$(node -p 'require("./src-tauri/tauri.conf.json").plugins.updater.pubkey')" = "$(cat ~/.tauri/muesli.key.pub)" ] \
     && echo "pubkey matches"
   ```

2. **Branch protection.** After the first CI run has reported its checks, require them on `main`:

   ```sh
   gh api -X PUT repos/garbit/muesli-meeting-assistant/branches/main/protection --input - <<'EOF'
   {
     "required_status_checks": {
       "strict": true,
       "contexts": ["validate / versions", "validate / frontend", "validate / rust-check", "validate / bundle"]
     },
     "enforce_admins": false,
     "required_pull_request_reviews": null,
     "restrictions": null,
     "allow_force_pushes": false
   }
   EOF
   ```

3. **Developer ID (later).** When an Apple Developer account exists, add the secrets
   `APPLE_CERTIFICATE` (base64 `.p12`), `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`,
   `APPLE_ID`, `APPLE_PASSWORD` (an app-specific password) and `APPLE_TEAM_ID`. The release job
   signs and notarises whenever `APPLE_CERTIFICATE` is set; nothing else changes. Until then
   builds are ad-hoc signed and macOS forgets the privacy grants on every update
   (see `docs/INSTALL.md`).

## Dry run

A dry run builds exactly what a release would, including the signed updater artifacts, but
creates no release:

```sh
gh workflow run release.yml --ref main
gh run watch
gh run download <run-id>        # muesli-dry-run-* artifacts: .dmg, .app.tar.gz, .sig
```

Do this at least once before the first real tag, and whenever the build or signing setup changes.

## Cutting a release

1. Set the version everywhere at once (Cargo workspace, `Cargo.lock`, `package.json`, the
   sidecar's `Main.swift` and `Info.plist`) and verify:

   ```sh
   scripts/set-version.sh 0.2.0
   git add -A && git commit -m "Release v0.2.0"
   ```

   Open a pull request and merge it. `tauri.conf.json` has no `version` key: Tauri reads it
   from `Cargo.toml`.

2. Tag the merged commit on `main` and push the tag:

   ```sh
   git switch main && git pull
   scripts/check-version.sh v0.2.0      # the tag must equal the version in the sources
   git tag -a v0.2.0 -m "Muesli v0.2.0"
   git push origin v0.2.0
   ```

   The `versions` job fails within seconds if the tag does not match or the commit is not on
   `main`, before any build starts. A tag with a prerelease suffix (`v0.2.0-beta.1`) is marked
   as a prerelease; the updater never offers those.

3. Wait for the workflow (about 25 minutes with warm caches). The draft release holds:
   `Muesli_0.2.0_aarch64.dmg`, `Muesli_0.2.0_aarch64.app.tar.gz`, its `.sig`, and
   `latest.json` for the updater. Release notes are generated from the merged pull requests,
   with the install blurb prepended.

## Publishing

1. Download the DMG from the draft, install it and run through a recording. Check that the
   `--doctor` smoke step in the run log reported the expected version.
2. Edit the generated notes if needed, then publish:

   ```sh
   gh release edit v0.2.0 --draft=false --latest
   curl -fsSL https://github.com/garbit/muesli-meeting-assistant/releases/latest/download/latest.json
   ```

   `releases/latest` ignores drafts and prereleases, so this is the moment installed apps start
   seeing the update (Settings → Check for updates).

## If something goes wrong

- **The workflow failed before publishing.** Fix the problem on `main`, then either re-run the
  failed run (the draft is reused and its assets replaced) or delete the draft and the tag and
  tag again.
- **A published release is bad.** Never re-run the release workflow for a published tag (the
  action refuses because the release is no longer a draft). Fix forward with a new patch
  version. If users must be stopped from updating to it, mark the release as a prerelease:
  `gh release edit v0.2.0 --prerelease`.
- **The link step is killed on the runner** (`signal: 9` while linking the release binary):
  the 7 GB runner ran out of memory under fat LTO. Switch CI to thin LTO without changing the
  committed profile: `gh variable set RELEASE_LTO -b thin`, then re-run.
- **Reproducing a release build locally**, including the signed updater archive:

  ```sh
  TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/muesli.key)" TAURI_SIGNING_PRIVATE_KEY_PASSWORD="" \
    pnpm tauri build --config src-tauri/tauri.release.conf.json
  scripts/smoke-doctor.sh target/release/bundle/macos/Muesli.app
  ```

## Dependencies

`.github/dependabot.yml` opens grouped monthly pull requests for GitHub Actions (which are
pinned by commit SHA), Cargo, npm and Swift packages. Each one runs the full CI.
