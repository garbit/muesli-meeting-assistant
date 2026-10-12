# Muesli

A local, private meeting recorder and summariser for Apple Silicon Macs. Muesli
records your meeting (the other participants through a Core Audio tap, you
through the microphone), transcribes it, works out who said what, and writes a
summary with a TL;DR, the main points per speaker, and the actions. Everything
runs on your Mac; nothing is uploaded.

Models are restricted to US/EU-origin weights and runtimes. See
[docs/MODEL-POLICY.md](docs/MODEL-POLICY.md).

## Status

Early development. The M0 spikes (capture, ASR, diarisation, local LLM) pass;
see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for numbers.

## Requirements

* Apple Silicon Mac, macOS 14.4 or newer; 16 GB RAM for the Fast preset,
  24 GB+ for Balanced.
* About 1 GB of disk for speech models plus 8–15 GB for the summarisation model.

## Development

```sh
rustup update stable          # Tauri 2.12 needs Rust 1.90+
pnpm install
scripts/build-sidecar.sh      # Swift sidecar (needs only Command Line Tools)
pnpm tauri dev                # or: pnpm tauri build --debug for a real bundle
```

Every pull request runs `cargo fmt --check`, clippy, the unit tests, the frontend
build and a full `pnpm tauri build`; the DMG is attached to the run as the
`muesli-dmg` artifact. Tagged releases are built by CI and opened as drafts; see
[docs/RELEASING.md](docs/RELEASING.md). Local builds never produce updater
artifacts (`createUpdaterArtifacts` is enabled only by the release workflow).

Microphone and System Audio Recording permissions are granted to the process
that launches the recorder. When running from `pnpm tauri dev`, that is your
terminal; test capture from a built bundle (`pnpm tauri build --debug`) to see
the prompts attributed to Muesli.

Useful sidecar spikes (no app needed):

```sh
sidecar/.build/release/muesli-sidecar spike models             # download ASR + diarisation models
sidecar/.build/release/muesli-sidecar spike capture 15 /tmp/cap # record 15 s of mic + system audio
sidecar/.build/release/muesli-sidecar spike asr file.wav
sidecar/.build/release/muesli-sidecar spike diarize file.wav
sidecar/.build/release/muesli-sidecar spike recover /tmp/cap        # rebuild recording.m4a from chunks after a crash
sidecar/.build/release/muesli-sidecar spike live 30 /tmp/cap        # record 30 s and print the live transcript as it arrives
cargo run --release -p muesli-llm --example bench -- path/to/model.gguf
```

Headless modes of the app binary (integration checks, no UI):

```sh
target/debug/muesli --import recording.m4a          # decode → transcribe → diarise → summarise, prints everything
MUESLI_NAME_SPEAKERS="S1=Alice,S2=Bob" target/debug/muesli --import call.wav   # name clusters so later imports auto-link them
# headless runs use a scratch library under $TMPDIR/muesli-headless unless MUESLI_HOME is set
open "$PWD/target/debug/bundle/macos/Muesli.app" --args --capture-test 10 /tmp/cap  # record 10 s as the bundle (prompts attributed to Muesli; needs the absolute path)
scripts/tcc-reset.sh                                # forget Muesli's privacy grants during development
```

## Troubleshooting

* **A meeting shows "Failed"**: open it; the banner shows the error, and the
  Diagnostics tab has the job history, the files in the meeting folder and the
  sidecar logs for that meeting. Retry re-runs processing.
* **Logs**: `~/Library/Application Support/Muesli/logs/muesli.log.<date>` (14
  days kept); Settings → Diagnostics → "Show log file". Each meeting folder also
  has `capture.log` and `process.log`.
* **Bug reports**: Settings → Diagnostics → "Copy diagnostics report" (or run
  `muesli --doctor` against the bundle's binary:
  `/Applications/Muesli.app/Contents/MacOS/muesli --doctor`). The report lists
  versions, model status, recent meetings, jobs with errors and the log tail;
  it never includes transcript text.
* **Recording does not start**: the error appears under the Record button and
  no meeting is kept. The usual cause is a missing Microphone or System Audio
  Recording grant (Settings → Permissions).
* **Quitting while recording** finalises the recording first (the sidebar shows
  "Finalising…" while the file is encoded); the next launch resumes any meeting
  that was recorded but not yet transcribed. If the app is force-quit or
  crashes, the capture process still finishes the file on its own and the next
  launch picks it up. Only one Muesli can run at a time; launching it again just
  focuses the existing window.
* **Reinstalling a local build**: `scripts/install-local.sh` builds the debug
  bundle, replaces `/Applications/Muesli.app` and relaunches it.

## Layout

* `src/` React + TypeScript UI
* `src-tauri/` Rust core (jobs, SQLite, model manifest, llama.cpp)
* `crates/llm/` llama.cpp wrapper
* `sidecar/` Swift sidecar (audio capture, Parakeet ASR, pyannote diarisation)
* `docs/` architecture and model policy

## Licence

MIT. Third-party models keep their own licences (see the model policy).
