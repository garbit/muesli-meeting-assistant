import FluidAudio
import Foundation

struct AsrWord: Codable {
    let w: String
    let s: Double
    let e: Double
}

struct AsrOutput: Codable {
    let engine: String
    let text: String
    let words: [AsrWord]
    let durationS: Double
    let processingS: Double
    let confidence: Float
}

/// ASR engine protocol so Whisper / SpeechAnalyzer can slot in later (M6).
protocol AsrEngine {
    static var id: String { get }
    func transcribe(wav: URL, progress: @escaping @Sendable (Double) -> Void) async throws -> AsrOutput
}

/// NVIDIA Parakeet TDT 0.6B v3 via FluidAudio (CoreML, Neural Engine).
final class ParakeetEngine: AsrEngine {
    static let id = ModelId.parakeetV3.rawValue
    private let store: ModelStore

    init(store: ModelStore) { self.store = store }

    func transcribe(wav: URL, progress: @escaping @Sendable (Double) -> Void) async throws -> AsrOutput {
        let st = store.status(.parakeetV3)
        guard st.installed else { throw RpcError.server("model not installed: \(ModelId.parakeetV3.rawValue) (missing \(st.missing))") }

        let t0 = Date()
        let models = try await AsrModels.load(from: store.repoDirectory(.parakeetV3), version: .v3)
        log("parakeet models loaded in \(String(format: "%.2f", Date().timeIntervalSince(t0)))s")
        let manager = AsrManager(config: .default, models: models)

        let progressTask = Task {
            let stream = await manager.transcriptionProgressStream
            do {
                for try await p in stream { progress(p) }
            } catch {}
        }
        defer { progressTask.cancel() }

        var state = TdtDecoderState.make(decoderLayers: 2)
        let result = try await manager.transcribe(wav, decoderState: &state)
        let words = buildWordTimings(from: result.tokenTimings ?? []).map {
            AsrWord(w: $0.word, s: $0.startTime, e: $0.endTime)
        }
        await manager.cleanup()
        return AsrOutput(
            engine: Self.id, text: result.text, words: words, durationS: result.duration,
            processingS: result.processingTime, confidence: result.confidence)
    }
}
