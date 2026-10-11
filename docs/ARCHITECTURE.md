# Muesli architecture

Muesli is a local, private meeting recorder and summariser for Apple Silicon Macs
(macOS 14.4+). Nothing leaves the machine. See `docs/MODEL-POLICY.md` for which
models are allowed and why.

```
┌──────────────── Muesli.app (Tauri 2) ────────────────────┐
│ React UI ⇄ Tauri IPC ⇄ Rust core (src-tauri)               │
│                        ├─ jobs: record / import / process  │
│                        ├─ llm: llama.cpp via crates/llm    │
│                        ├─ db: SQLite (rusqlite, WAL)       │
│                        ├─ models: manifest + GGUF download │
│                        └─ sidecar client (NDJSON JSON-RPC) │
│                                 │ stdio                     │
│   muesli-sidecar (Swift, sidecar/) — two roles, one binary  │
│   ├─ `capture`: Core Audio tap + mic → recording.m4a        │
│   └─ `process`: decode, Parakeet ASR, pyannote diarisation │
└─────────────────────────────────────────────────────────────┘
```

## Components

### Swift sidecar (`sidecar/`)

* SwiftPM package, Swift 5 language mode, macOS 14.4, single dependency:
  [FluidAudio](https://github.com/FluidInference/FluidAudio) 0.17.7 (Apache-2.0).
  Builds with Command Line Tools alone; no Xcode needed.
* Protocol: newline-delimited JSON-RPC 2.0 on stdin/stdout; logs on stderr.
  Long jobs emit `job.progress {jobId, phase, fraction}`; `job.cancel` cancels.
* `capture` role: one private aggregate device whose main sub-device is the
  default input and whose tap list holds a global process tap (everything except
  Muesli itself), with drift compensation. One IOProc delivers sample-aligned
  mic and system channels. Core Audio dispatches IOBlocks synchronously, so the
  IO handler only mixes into preallocated scratch space and pushes interleaved
  frames into a ring buffer; a writer queue drains it every 100 ms into raw
  Float32 PCM chunk files (`chunk-NNNNN-<rate>.pcm`, no header to finalise, so a
  crash loses at most 100 ms) and `stop` concatenates them into `recording.m4a`
  (AAC, L = mic, R = system). `capture.recover` rebuilds the file from leftover
  chunks, or reports an already finalised `recording.m4a` when an orphaned
  sidecar finished the job itself. The app runs it at startup for meetings whose
  recording never reached the database (state `recording` after a crash, or
  `failed` because `capture.stop` did not answer) and only when the meeting
  folder holds chunks, a recording or a lock file; a `recording` meeting with
  nothing on disk is marked `failed`. A busy error (code -32002, the lock holder
  is still alive) makes the app wait and retry for up to ten minutes. A
  `capture.stop` that fails triggers the same recovery immediately in the
  background, and a start that fails before any audio is written deletes the
  meeting again rather than leaving an empty "Failed" row.
  Graph lifecycle, device-change listeners (coalesced), the sample-rate listener
  and the watchdog run on a separate control queue. Device changes rebuild the
  graph and insert silence for the gap so the timeline stays wall-clock aligned.
  The watchdog rebuilds at most twice per recording when the system channel is
  bit-exact silent for 30 s while another process plays audio, then reports
  `systemAudio: "silent"`. If the parent dies, the sidecar ignores SIGPIPE,
  notices the failed write or stdin EOF, and finalises the recording first; the
  app never SIGKILLs a capture sidecar for that reason (a lingering one is
  finalising), and a normal quit while recording stops the recording before the
  app exits, refusing further quit requests until it is done. Between Stop and
  the finished file the UI shows "Finalising…" (status `state: "finalising"`). While
  recording, `capture.lock` holds the sidecar's pid so startup recovery (and a
  second app instance) refuse to touch a live recording; the app itself is
  single-instance.
* `process` role: `decode` (AVAssetReader → 16 kHz mono 16-bit WAV + 100 ms
  energy timeline for the mic/system channels), `asr.run` (Parakeet TDT 0.6B v3,
  word timings via `buildWordTimings`), `diarize.run` (pyannote community-1
  offline pipeline loaded with `OfflineDiarizerModels.load`, never the purging
  `prepareModels`; returns segments and one 256-d embedding per speaker),
  `models.status` (mirrors FluidAudio's load criteria: `coremldata.bin` present,
  no `.partial`, revision marker matches), `models.download`. Jobs run one at a
  time; `ModelHub.offlineMode` is on except during a download. The process exits
  when stdin closes, which is how CoreML/ANE memory is released before the LLM
  loads.
* Model files live under `~/Library/Application Support/Muesli/models/FluidInference/`
  in FluidAudio's cache layout (`parakeet-tdt-0.6b-v3/`, `speaker-diarization/`).
  Only the Community-1 artifact set of the diarisation repo is fetched (variant
  `offline`); legacy wespeaker bundles are never downloaded.

### Rust core (`src-tauri/`, `crates/llm/`)

* `sidecar.rs`: spawns `muesli-sidecar <role>` from the executable's directory
  (where Tauri places `externalBin`), routes responses to callers, fans out
  notifications, applies per-call timeouts, closes stdin on shutdown and
  SIGKILLs after 3 s.
* `jobs.rs`: one processing job at a time with cancellation; deletes the 16 kHz
  mix after use; marks interrupted jobs/summaries at startup. `summarise.rs`
  validates (model installed, transcript present) before any background work,
  tracks the single running summary for both UI- and pipeline-triggered runs,
  and unloads the LLM after 10 idle minutes.
* `db.rs`: rusqlite (bundled) with `rusqlite_migration`; schema in `migrations/`.
* `models.rs`: the embedded manifest (`models.manifest.json`) is the policy gate;
  GGUFs download from Hugging Face with range resume and optional SHA-256.
* `crates/llm`: llama.cpp through `llama-cpp-2` with Metal. ggml embeds its Metal
  library at build time, so no Metal compiler is required. The model's Jinja chat
  template is parsed once per load (minijinja with Python-compat helpers) and
  falls back to the Gemma 4 layout if it cannot be rendered.

### Live transcript and playback

* While recording, the capture sidecar also runs FluidAudio's sliding-window
  streaming recogniser on the mono mix with the same Parakeet v3 model (no extra
  download). Audio is buffered from the first second so the model's load time
  costs nothing; confirmed segments arrive in roughly ten-second windows and are
  labelled "me"/"them" from the mic/system energy balance. They are emitted as
  `capture.transcript` notifications, forwarded to the UI as
  `recording:transcript`, and saved as `live.json` when the recording stops so
  the meeting is readable before (or without) the full pass.
* The full transcription with speakers still runs after the meeting (setting
  `autoTranscribe`, default on) and replaces the live text; the summary follows
  (setting `autoSummarise`). A meeting left at `recorded` can be transcribed
  later with the re-run button.
* Playback uses Tauri's asset protocol, scoped to the meetings directory only.
  The transcript is the seek surface: clicking a segment jumps the player, and
  the segment under the playhead is highlighted and kept in view.
* The record/stop control lives in the sidebar so it is reachable from every
  screen; while recording it shows elapsed time and mic/call levels and links
  to the live meeting view (notes on the left, live transcript on the right).

### Diagnostics

* `diagnostics.rs` sets up logging to stderr and to a daily file
  `logs/muesli.log.<date>` (14 days kept). Sidecar stderr is forwarded into the
  same log and, per meeting, appended to `capture.log` / `process.log` in the
  meeting folder, so a failure can be read after the fact.
* Every job records its error in the `jobs` table; `Meeting.lastError` surfaces
  the latest one in the Library and the meeting header (with Retry), and the
  meeting's Diagnostics tab shows the job history, folder contents and logs.
* `diagnostics_report` (Settings → "Copy diagnostics report") and
  `muesli --doctor` produce the same text report: versions, machine, settings,
  model status, recent meetings/jobs/summaries and the log tail. It contains no
  transcript text.
* Contract tests in `align.rs` pin the JSON shapes the Swift sidecar emits
  (camelCase keys); a mismatch there is what broke live recordings once.

### Frontend (`src/`)

React 19 + TypeScript + Tailwind 4 (Vite plugin). `src/lib/ipc.ts` is the only
file that knows command names and payload shapes. WKWebView does not implement
`window.confirm`/`prompt`, so confirmations use the dialog plugin and text input
uses the inline `InlinePrompt` component.

## M0 spike results (M1 Max, 32 GB, macOS 26.5, 2026-10-11)

| Check | Result |
|---|---|
| llama.cpp Metal build with CLT only | OK (release build ~2 min) |
| Gemma 4 26B-A4B Q4_0 load | 8–17 s (page cache dependent) |
| Prompt eval, 20.8k tokens, n_ctx 32768, KV q8_0, flash-attn | 56 s = 370 tok/s |
| Generation | 25–27 tok/s |
| Peak RSS during the above | 12.5–13.7 GB |
| Parakeet v3 download / size | 23 files, 460 MiB |
| Parakeet first load (CoreML compile) / later loads | 28 s / ~2 s |
| Parakeet on 77 s two-voice fixture | verbatim, word timings, 23x realtime warm |
| pyannote community-1 download / size | 20 MiB (Community-1 set only) |
| Diarisation of the fixture | 2 speakers, 9 segments, all correct, 5 s |
| Capture: aggregate(mic + global tap) | 2 input streams at 48 kHz, both channels live, 0 gaps |
| Capture → decode → ASR round trip | transcript of the audio played through the speakers |
| Full pipeline on the 77 s fixture (Rust core, warm) | 9 turns, 2 speakers, ~6 s; re-import auto-links named voices in 2 s |
| Summary of the fixture, Gemma 4 26B-A4B via its own Jinja template | 642 prompt tokens, 325 output tokens, 11 s generation (+11 s model load) |

Implications:

* A 90-minute meeting (~18–20k tokens) summarises in one pass in roughly
  2 minutes on the Balanced preset; ASR and diarisation add well under a minute.
* Gemma 4 ships a large Jinja chat template that llama.cpp's built-in matcher
  does not recognise (`llama_chat_apply_template` returns -1). The format is
  `<|turn>role\n…<turn|>\n`, generation prompt `<|turn>model\n`, and an empty
  `<|channel>thought\n<channel|>` block disables thinking. The engine renders the
  model's own template with minijinja instead of hard-coding this.
* Sampling needs a repetition penalty (`penalties(64, 1.15)`) in addition to
  temperature; a degenerate (looping) prompt otherwise produces looping output.
* The capture process must be launched by something that holds the Microphone
  and System Audio Recording permissions; when launched from a terminal, the
  terminal is the responsible process.
* ggml's Metal backend asserts (`[rsets->data count] == 0`) during C++ static
  destruction if a model is still loaded at `exit()`. The engine is unloaded on
  Tauri's `RunEvent::Exit` and before the headless mode returns.
