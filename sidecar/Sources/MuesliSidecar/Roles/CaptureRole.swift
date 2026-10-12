import AVFoundation
import Foundation

struct StopResult: Codable {
    let recordingPath: String
    let durationMs: Int
    let sampleRate: Double
    let gaps: [GapRecord]
    let droppedFrames: Int64
    /// Final text of the live transcript (empty when live transcription was off).
    let liveText: String
    /// Live transcript segments, confirmed plus the final hypothesis, in time
    /// order. Authoritative: the last `capture.transcript` notifications can
    /// land after this response.
    let liveSegments: [LiveSegment]
}

/// Owns the single active recorder (and its live transcriber) for the capture process.
actor RecorderHolder {
    private var recorder: MeetingRecorder?
    private var live: LiveTranscriber?

    func start(
        dir: URL, chunkSeconds: Double, liveTranscript: Bool, store: ModelStore,
        onStatus: @escaping (CaptureStatus) -> Void, onTranscript: @escaping @Sendable (LiveSegment) -> Void
    ) async throws -> CaptureStatus {
        guard recorder == nil else { throw RpcError.server("already recording") }
        let r = MeetingRecorder(outputDir: dir, chunkSeconds: chunkSeconds)
        r.onStatus = onStatus
        try r.start()
        recorder = r
        if liveTranscript {
            // Attach before loading so the first seconds are buffered, not lost.
            let t = LiveTranscriber(store: store, sampleRate: r.currentSampleRate)
            r.attachLive(t)
            do {
                try await t.start(onUpdate: onTranscript)
                live = t
            } catch {
                r.attachLive(nil)
                log("live transcript unavailable: \(error)")
            }
        }
        return r.status()
    }

    func status() -> CaptureStatus? { recorder?.status() }

    var isRecording: Bool { recorder != nil }

    func stop() async throws -> StopResult {
        guard let r = recorder else { throw RpcError.server("not recording") }
        recorder = nil
        var transcript = LiveTranscriptResult(text: "", segments: [])
        if let t = live {
            live = nil
            transcript = await t.stop()
            r.attachLive(nil)
        }
        let res = try r.stop()
        return StopResult(
            recordingPath: res.recordingPath, durationMs: res.durationMs, sampleRate: res.sampleRate, gaps: res.gaps,
            droppedFrames: res.droppedFrames, liveText: transcript.text, liveSegments: transcript.segments)
    }
}

/// `muesli-sidecar capture`: lives for the duration of one meeting.
enum CaptureRole {
    private static let holder = RecorderHolder()

    /// Called when the parent disappears (stdin EOF or stdout write failure):
    /// finalise any recording so nothing is lost.
    static func shutdown() async {
        guard await holder.isRecording else { return }
        log("parent gone while recording; finalising")
        do {
            let r = try await holder.stop()
            log("finalised \(r.recordingPath) (\(r.durationMs) ms)")
        } catch {
            log("finalise failed: \(error); raw chunks kept for capture.recover")
        }
    }

    static func register(on server: RpcServer, store: ModelStore) {
        server.register("health") { _, _ in
            ["ok": true, "role": "capture", "version": .string(sidecarVersion), "pid": .number(Double(getpid()))]
        }

        server.register("capture.devices") { _, _ in
            let inp = (try? AudioObjectID.defaultInputDevice()) ?? kAudioObjectUnknown
            let out = (try? AudioObjectID.defaultOutputDevice()) ?? kAudioObjectUnknown
            return [
                "input": .string(inp.isValid ? inp.name : ""),
                "output": .string(out.isValid ? out.name : ""),
            ]
        }

        server.register("permissions.probe") { _, _ in
            let mic: String
            switch AVCaptureDevice.authorizationStatus(for: .audio) {
            case .authorized: mic = "authorized"
            case .denied: mic = "denied"
            case .restricted: mic = "restricted"
            default: mic = "notDetermined"
            }
            return ["microphone": .string(mic)]
        }

        server.register("permissions.request") { _, _ in
            let granted = await AVCaptureDevice.requestAccess(for: .audio)
            return ["microphone": .bool(granted)]
        }

        /// Exercise a process tap (and so trigger the System Audio Recording prompt
        /// if macOS has not decided yet) and report what it delivered; see
        /// `SystemAudioProbe` for why a plain create is not enough.
        server.register("permissions.systemAudioProbe") { params, _ in
            let timeout = params["timeoutMs"]?.int ?? 3000
            return try jsonValue(await SystemAudioProbe.run(timeoutMs: timeout))
        }

        server.register("capture.start") { params, server in
            guard let dir = params["outputDir"]?.string else { throw RpcError.invalidParams("outputDir required") }
            let status = try await holder.start(
                dir: URL(fileURLWithPath: dir, isDirectory: true), chunkSeconds: params["chunkSeconds"]?.double ?? 10,
                liveTranscript: params["liveTranscript"]?.bool ?? false, store: store,
                onStatus: { status in
                    if let v = try? jsonValue(status) { Task { await server.output.notify("capture.status", v) } }
                },
                onTranscript: { seg in
                    if let v = try? jsonValue(seg) { Task { await server.output.notify("capture.transcript", v) } }
                })
            return try jsonValue(status)
        }

        server.register("capture.status") { _, _ in
            guard let s = await holder.status() else { return ["state": "idle"] }
            return try jsonValue(s)
        }

        server.register("capture.stop") { _, _ in
            try jsonValue(try await holder.stop())
        }

        /// Rebuild recording.m4a from chunk files a crashed session left behind.
        server.register("capture.recover") { params, _ in
            guard let dir = params["outputDir"]?.string else { throw RpcError.invalidParams("outputDir required") }
            return try jsonValue(try MeetingRecorder.recover(outputDir: URL(fileURLWithPath: dir, isDirectory: true)))
        }
    }
}
