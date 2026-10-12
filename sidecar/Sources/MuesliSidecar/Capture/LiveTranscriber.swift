import AVFoundation
import FluidAudio
import Foundation

/// A live transcript segment: confirmed segments are stable, the volatile one
/// is the current hypothesis and gets replaced on the next update.
struct LiveSegment: Codable {
    let text: String
    let confirmed: Bool
    let startMs: Int
    let endMs: Int
    /// "me" when the mic channel dominated during the segment, "them" when the
    /// system channel did, nil when unknown.
    let who: String?
}

/// What the live transcriber hands back when the recording stops.
struct LiveTranscriptResult {
    let text: String
    /// Every confirmed segment plus the final hypothesis, in time order.
    let segments: [LiveSegment]
}

/// Near-real-time transcription of the recording mix using the installed
/// Parakeet v3 model through FluidAudio's sliding-window streaming manager.
/// The recorder feeds it the mono mix every 100 ms; updates are delivered on a
/// callback and labelled "me"/"them" from the mic/system energy balance.
final class LiveTranscriber {
    private let store: ModelStore
    private let manager = SlidingWindowAsrManager(config: .streaming)
    private var updates: Task<Void, Never>?
    private let format: AVAudioFormat
    private let rate: Double
    /// Ordered hand-off from the writer queue to the actor (a Task per block
    /// could reorder audio).
    private let feedStream: AsyncStream<AVAudioPCMBuffer>
    private let feedContinuation: AsyncStream<AVAudioPCMBuffer>.Continuation
    private var feeder: Task<Void, Never>?

    /// Guards the state shared between the writer queue (`feed`), the updates
    /// task and the async start/stop paths.
    private let lock = NSLock()
    /// Accepting audio (from construction until stop); the recogniser may still be loading.
    private var started = true
    /// (timeMs, micRatio) samples, one per feed call, for me/them labelling.
    private var energy: [(Int, Float)] = []
    private var fedFrames: Int64 = 0
    private var lastConfirmedEnd = 0
    private var confirmedSegments: [LiveSegment] = []
    private var volatile: LiveSegment?

    /// Audio is accepted from construction so nothing recorded during the model
    /// load (up to ~30 s the first time) is lost; the backlog is transcribed
    /// faster than real time once the recogniser is ready.
    init(store: ModelStore, sampleRate: Double) {
        self.store = store
        self.rate = sampleRate
        self.format = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: sampleRate, channels: 1, interleaved: false)!
        (feedStream, feedContinuation) = AsyncStream<AVAudioPCMBuffer>.makeStream(bufferingPolicy: .unbounded)
    }

    var isRunning: Bool { lock.withLock { started } }

    /// Loads the ASR models (fast when warm) and starts the recogniser. On
    /// failure the transcriber stops accepting audio and drops its backlog.
    func start(onUpdate: @escaping @Sendable (LiveSegment) -> Void) async throws {
        guard store.status(.parakeetV3).installed else {
            abandon()
            throw RpcError.server("live transcript needs \(ModelId.parakeetV3.rawValue); it is not installed")
        }
        let t0 = Date()
        // Installed models load from disk only; online mode revalidates against
        // Hugging Face first, which is slow and pointless here.
        ModelHub.offlineMode = true
        do {
            let models = try await AsrModels.load(from: store.repoDirectory(.parakeetV3), version: .v3)
            try await manager.loadModels(models)
        } catch {
            abandon()
            throw error
        }
        log("live transcript models ready in \(String(format: "%.1f", Date().timeIntervalSince(t0)))s")
        let stream = await manager.transcriptionUpdates
        updates = Task { [weak self] in
            for await u in stream {
                guard let self else { return }
                onUpdate(self.record(u))
            }
        }
        do {
            try await manager.startStreaming(source: .system)
        } catch {
            abandon()
            updates?.cancel()
            await manager.cleanup()
            throw error
        }
        let mgr = manager
        let stream2 = feedStream
        feeder = Task {
            for await buf in stream2 { await mgr.streamAudio(buf) }
        }
        log("live transcript started (sliding window, rate \(rate))")
    }

    /// Stop accepting audio and discard what was buffered.
    private func abandon() {
        lock.withLock { started = false }
        feedContinuation.finish()
    }

    /// Feed a block of mixed mono audio at the recorder's rate (called from the writer queue).
    func feed(mic: UnsafePointer<Float>, sys: UnsafePointer<Float>, frames: Int, micEnergy: Float, sysEnergy: Float) {
        guard frames > 0 else { return }
        let total = micEnergy + sysEnergy
        let accepted: Bool = lock.withLock {
            guard started else { return false }
            let atMs = Int(Double(fedFrames) / rate * 1000)
            fedFrames += Int64(frames)
            if total > 1e-7 {
                energy.append((atMs, micEnergy / total))
                // Bounded history (~33 min at 100 ms); dropping the oldest half at
                // once keeps the trim amortised instead of a shift per call.
                if energy.count > 20_000 { energy.removeFirst(10_000) }
            }
            return true
        }
        guard accepted, let buf = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(frames)) else { return }
        let dst = buf.floatChannelData![0]
        for f in 0..<frames { dst[f] = 0.5 * (mic[f] + sys[f]) }
        buf.frameLength = AVAudioFrameCount(frames)
        feedContinuation.yield(buf)
    }

    /// Stop, flush the remaining audio through the recogniser and return the
    /// final text and segments.
    func stop() async -> LiveTranscriptResult {
        let wasStarted: Bool = lock.withLock {
            let s = started
            started = false
            return s
        }
        guard wasStarted else { return LiveTranscriptResult(text: "", segments: []) }
        feedContinuation.finish()
        await feeder?.value
        // finish() processes every remaining window and yields its updates before
        // returning; cleanup() then ends the update stream, and waiting for the
        // consumer guarantees those updates are in `confirmedSegments` below.
        let text = feeder == nil ? "" : ((try? await manager.finish()) ?? "")
        await manager.cleanup()
        await drainUpdates(timeout: 10)
        let segments: [LiveSegment] = lock.withLock {
            var all = confirmedSegments
            if let v = volatile, !v.text.isEmpty {
                all.append(LiveSegment(text: v.text, confirmed: true, startMs: v.startMs, endMs: v.endMs, who: v.who))
            }
            return all
        }
        return LiveTranscriptResult(text: text, segments: segments)
    }

    /// Wait for the updates task to see the end of the stream; cancel it if the
    /// stream somehow never ends so a stop can never hang on it.
    private func drainUpdates(timeout: Double) async {
        guard let updates else { return }
        await withTaskGroup(of: Bool.self) { group in
            group.addTask { await updates.value; return true }
            group.addTask {
                try? await Task.sleep(nanoseconds: UInt64(timeout * 1_000_000_000))
                return false
            }
            if await group.next() == false {
                log("live transcript updates did not drain in \(timeout)s; cancelling")
                updates.cancel()
            }
            group.cancelAll()
        }
    }

    /// Build the segment for an update and remember it: confirmed ones for good,
    /// the hypothesis until the next update replaces it.
    private func record(_ u: SlidingWindowTranscriptionUpdate) -> LiveSegment {
        let (startMs, endMs) = span(of: u)
        let seg = LiveSegment(text: u.text, confirmed: u.isConfirmed, startMs: startMs, endMs: endMs, who: who(startMs: startMs, endMs: endMs))
        lock.withLock {
            if u.isConfirmed {
                lastConfirmedEnd = endMs
                confirmedSegments.append(seg)
                volatile = nil
            } else {
                volatile = seg
            }
        }
        return seg
    }

    private func span(of u: SlidingWindowTranscriptionUpdate) -> (Int, Int) {
        if let first = u.tokenTimings.first, let last = u.tokenTimings.last {
            return (Int(first.startTime * 1000), Int(last.endTime * 1000))
        }
        // No timings: assume it follows the last confirmed segment up to now.
        return lock.withLock { (lastConfirmedEnd, Int(Double(fedFrames) / rate * 1000)) }
    }

    private func who(startMs: Int, endMs: Int) -> String? {
        lock.lock()
        defer { lock.unlock() }
        let window = energy.filter { $0.0 >= startMs - 100 && $0.0 <= endMs + 100 }
        guard !window.isEmpty else { return nil }
        let mean = window.map(\.1).reduce(0, +) / Float(window.count)
        if mean >= 0.65 { return "me" }
        if mean <= 0.35 { return "them" }
        return nil
    }
}
