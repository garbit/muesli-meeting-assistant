import FluidAudio
import Foundation

struct DiarSegment: Codable {
    let speaker: String
    let s: Double
    let e: Double
    let q: Float
}

struct DiarOutput: Codable {
    let engine: String
    let segments: [DiarSegment]
    /// Speaker id → 256-d L2-normalised embedding (pyannote community-1 embedding model).
    let speakers: [String: [Float]]
    let processingS: Double
}

/// pyannote speaker-diarization-community-1 (powerset segmentation + VBx) via FluidAudio.
final class CommunityDiarizer {
    static let id = ModelId.pyannoteCommunity1.rawValue
    private let store: ModelStore

    init(store: ModelStore) { self.store = store }

    func run(
        wav: URL, minSpeakers: Int?, maxSpeakers: Int?, numSpeakers: Int?,
        progress: @escaping @Sendable (Double) -> Void
    ) async throws -> DiarOutput {
        let st = store.status(.pyannoteCommunity1)
        guard st.installed else { throw RpcError.server("model not installed: \(ModelId.pyannoteCommunity1.rawValue) (missing \(st.missing))") }

        var config = OfflineDiarizerConfig()
        config.clustering.minSpeakers = minSpeakers
        config.clustering.maxSpeakers = maxSpeakers
        config.clustering.numSpeakers = numSpeakers

        let t0 = Date()
        let manager = OfflineDiarizerManager(config: config)
        // Load directly: `prepareModels` purges the installed repo on any first-load
        // failure and, in offline mode, cannot re-download it.
        let models = try await OfflineDiarizerModels.load(from: store.root)
        manager.initialize(models: models)
        log("diarizer models ready in \(String(format: "%.2f", Date().timeIntervalSince(t0)))s")

        let result = try await manager.process(
            wav,
            progressCallback: { done, total in
                if total > 0 { progress(Double(done) / Double(total)) }
            })
        let segments = result.segments.map {
            DiarSegment(speaker: $0.speakerId, s: Double($0.startTimeSeconds), e: Double($0.endTimeSeconds), q: $0.qualityScore)
        }
        return DiarOutput(
            engine: Self.id, segments: segments, speakers: result.speakerDatabase ?? [:],
            processingS: Date().timeIntervalSince(t0))
    }
}
