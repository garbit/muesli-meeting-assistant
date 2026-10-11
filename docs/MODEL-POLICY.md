# Model policy

Muesli only runs models whose **weights and inference runtimes** come from
organisations in the United States or the European Union, plus Apple's own
on-device frameworks. The list is enforced, not advisory: the app's model
manager can only install ids present in `src-tauri/models.manifest.json`, the
Swift sidecar only knows how to fetch the repos in its `ModelId` enum, and the
LLM loader refuses paths that were not produced by the manifest.

## Approved for v1

| Id | Role | Weights | Runtime | Licence |
|---|---|---|---|---|
| `parakeet-tdt-0.6b-v3` | speech to text | NVIDIA Parakeet TDT 0.6B v3 (US) | FluidAudio CoreML port (FluidInference, US) on Apple CoreML/ANE | CC-BY-4.0 |
| `pyannote-community-1` | speaker diarisation | pyannote `speaker-diarization-community-1` (FR). Segmentation and embedding weights trained by pyannote; the embedding architecture is credited to WeSpeaker (a Chinese academic project) | FluidAudio CoreML port, Community-1 artifact set only | CC-BY-4.0 |
| `gemma-4-12b-it-q4_0` | summarisation (Fast preset) | Google Gemma 4 12B (US) | llama.cpp (ggml-org, EU) with Metal | Apache-2.0 |
| `gemma-4-26b-a4b-it-q4_0` | summarisation (Balanced preset, default) | Google Gemma 4 26B-A4B (US) | llama.cpp | Apache-2.0 |
| `gemma-4-31b-it-q4_0` | summarisation (optional, slow) | Google Gemma 4 31B (US) | llama.cpp | Apache-2.0 |
| `gpt-oss-20b-mxfp4` | summarisation (optional) | OpenAI gpt-oss-20b (US) | llama.cpp | Apache-2.0 |
| `embeddinggemma-300m-q8_0` | search embeddings | Google EmbeddingGemma 300M (US) | llama.cpp | Gemma Terms of Use |

All GGUFs are the `ggml-org` conversions on Hugging Face. CoreML bundles come
from the `FluidInference` organisation on Hugging Face; the diarisation repo is
pinned to a commit by FluidAudio.

EmbeddingGemma is the first entry under the **Gemma Terms of Use** rather than
Apache-2.0 or CC-BY. Commercial use is permitted; the Gemma prohibited-use
policy applies to what users do with the model; and the weights are downloaded
by the user from Hugging Face at runtime, never redistributed with the app. The
`ggml-org` GGUF includes the model's dense output layers, so its vectors match
the published model; Muesli keeps the first 256 of 768 dimensions (Matryoshka).

## Considered and rejected

* **DiariZen** (BUT Brno, CZ): best open diarisation accuracy in the ETH Zurich
  benchmark, but its weights are CC-BY-NC, which rules out work meetings.
* **NVIDIA Sortformer v2**: excellent up to four speakers, hard cap at four.
* **Swapping pyannote's embedding model** for ECAPA-TDNN (SpeechBrain, EU) or
  TitaNet (NVIDIA): would move away from the pipeline the accuracy numbers were
  measured on; pyannote moved from ECAPA to the WeSpeaker-style model because
  it was more accurate. Decision: keep community-1, record the provenance here.
* **sherpa-onnx** runtime (k2-fsa, CN), **Qwen**, **SenseVoice/Paraformer**,
  **CAM++/WeSpeaker pretrained checkpoints**: excluded by origin.
* **Legacy `wespeaker_v2.mlmodelc`** in the FluidInference diarisation repo:
  original WeSpeaker-trained weights; never downloaded (variant `offline` only).
* **nomic-embed-text-v1.5** (Nomic, US; Apache-2.0) for search embeddings:
  acceptable origin and licence but weaker retrieval than EmbeddingGemma. The
  embedder still accepts the `nomic-bert` architecture as a fallback.
* **Apple NLContextualEmbedding** for search embeddings: nothing to download,
  but the weakest retrieval quality of the three.

## Deferred (M6)

* **Whisper large-v3-turbo** (OpenAI, US) via WhisperKit (Argmax, US), MIT.
* **Apple SpeechAnalyzer** (macOS 26+): system-managed on-device model; no
  speaker labels, so still paired with pyannote.

## How to add a model

1. Confirm the organisation and licence of both weights and runtime.
2. Add an entry to `src-tauri/models.manifest.json` (and, for CoreML models,
   a `ModelId` case in `sidecar/Sources/MuesliSidecar/Models/ModelStore.swift`).
3. Record the decision in this file.
