import FluidAudio
import Foundation

/// Runs async operations one at a time (FIFO). Used so decode/ASR/diarise jobs
/// never overlap: they share CoreML/ANE memory and the process-global
/// `ModelHub.offlineMode`.
actor AsyncMutex {
    private var locked = false
    private var waiters: [CheckedContinuation<Void, Never>] = []

    func run<T: Sendable>(_ op: @Sendable () async throws -> T) async throws -> T {
        if locked {
            await withCheckedContinuation { waiters.append($0) }
        }
        locked = true
        defer {
            if waiters.isEmpty {
                locked = false
            } else {
                waiters.removeFirst().resume()
            }
        }
        return try await op()
    }
}

/// `muesli-sidecar process`: one-shot ML worker. Rust spawns one per job and
/// the process exits when stdin closes, which releases all CoreML/ANE memory
/// before the LLM is loaded.
enum ProcessRole {
    private static let gate = AsyncMutex()

    static func register(on server: RpcServer, store: ModelStore) {
        // Offline by default: a model that is "installed" must never trigger a
        // purge-and-redownload from Hugging Face. Only `models.download` goes online.
        ModelHub.offlineMode = true

        server.register("health") { _, _ in
            [
                "ok": true, "role": "process", "version": .string(sidecarVersion),
                "modelsRoot": .string(store.root.path), "pid": .number(Double(getpid())),
            ]
        }

        server.register("models.status") { _, _ in
            try jsonValue(ModelId.allCases.map { store.status($0) })
        }

        server.register("models.download") { params, server in
            guard let ids = params["ids"]?.array?.compactMap({ $0.string }), !ids.isEmpty else {
                throw RpcError.invalidParams("ids: [string] required")
            }
            let jobId = params["jobId"]?.string ?? "models"
            return try await gate.run {
                ModelHub.offlineMode = false
                defer { ModelHub.offlineMode = true }
                var results: [ModelStatus] = []
                for raw in ids {
                    guard let id = ModelId(rawValue: raw) else { throw RpcError.invalidParams("unknown model id: \(raw)") }
                    try await store.download(id) { fraction, phase in
                        Task { await server.progress(jobId: jobId, phase: "\(id.rawValue): \(phase)", fraction: fraction) }
                    }
                    results.append(store.status(id))
                    try Task.checkCancellation()
                }
                return try jsonValue(results)
            }
        }

        server.register("decode") { params, server in
            guard let input = params["input"]?.string, let output = params["output"]?.string else {
                throw RpcError.invalidParams("input, output required")
            }
            let jobId = params["jobId"]?.string ?? "decode"
            let stereo = params["micSystemStereo"]?.bool ?? false
            return try await gate.run {
                await server.progress(jobId: jobId, phase: "decode", fraction: 0)
                let decoded = try await AudioDecoder.decode(
                    input: URL(fileURLWithPath: input), outputWav: URL(fileURLWithPath: output), micSystemStereo: stereo)
                await server.progress(jobId: jobId, phase: "decode", fraction: 1)
                return try jsonValue(decoded)
            }
        }

        server.register("asr.run") { params, server in
            guard let audio = params["audio"]?.string, let output = params["output"]?.string else {
                throw RpcError.invalidParams("audio, output required")
            }
            let engineId = params["engine"]?.string ?? ParakeetEngine.id
            guard engineId == ParakeetEngine.id else { throw RpcError.invalidParams("unsupported engine: \(engineId)") }
            let jobId = params["jobId"]?.string ?? "asr"
            return try await gate.run {
                let engine = ParakeetEngine(store: store)
                let result = try await engine.transcribe(wav: URL(fileURLWithPath: audio)) { p in
                    Task { await server.progress(jobId: jobId, phase: "transcribe", fraction: p) }
                }
                try JSONEncoder().encode(result).write(to: URL(fileURLWithPath: output), options: .atomic)
                return [
                    "output": .string(output), "words": .number(Double(result.words.count)),
                    "durationS": .number(result.durationS), "processingS": .number(result.processingS),
                ]
            }
        }

        server.register("diarize.run") { params, server in
            guard let audio = params["audio"]?.string, let output = params["output"]?.string else {
                throw RpcError.invalidParams("audio, output required")
            }
            let jobId = params["jobId"]?.string ?? "diarize"
            let minSpeakers = params["minSpeakers"]?.int
            let maxSpeakers = params["maxSpeakers"]?.int
            let numSpeakers = params["numSpeakers"]?.int
            return try await gate.run {
                let diarizer = CommunityDiarizer(store: store)
                let result = try await diarizer.run(
                    wav: URL(fileURLWithPath: audio), minSpeakers: minSpeakers, maxSpeakers: maxSpeakers, numSpeakers: numSpeakers
                ) { p in
                    Task { await server.progress(jobId: jobId, phase: "diarize", fraction: p) }
                }
                try JSONEncoder().encode(result).write(to: URL(fileURLWithPath: output), options: .atomic)
                return [
                    "output": .string(output), "segments": .number(Double(result.segments.count)),
                    "speakers": .number(Double(result.speakers.count)), "processingS": .number(result.processingS),
                ]
            }
        }

        server.register("job.cancel") { params, server in
            guard let jobId = params["jobId"]?.string else { throw RpcError.invalidParams("jobId required") }
            return ["cancelled": .bool(server.jobs.cancel(jobId))]
        }
    }
}
