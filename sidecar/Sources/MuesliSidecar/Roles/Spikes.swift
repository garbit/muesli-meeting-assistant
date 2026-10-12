import FluidAudio
import Foundation

/// Developer spikes runnable without the Rust side (M0 acceptance checks).
enum Spikes {
    static func run(_ args: [String], store: ModelStore) async {
        do {
            switch args.first {
            case "models":
                AppLogger.minimumLevel = .debug
                let only = args.count >= 2 ? args[1] : nil
                for id in ModelId.allCases where only == nil || id.rawValue == only {
                    log("spike models: id=\(id.rawValue) repo=\(id.repo) folder=\(id.repo.folderName) variant=\(id.variant ?? "nil")")
                    let st = store.status(id)
                    if st.installed {
                        print("\(id.rawValue): installed at \(st.path) (\(st.bytes / 1_048_576) MiB)")
                        continue
                    }
                    print("\(id.rawValue): downloading to \(store.root.path)")
                    let last = Counter()
                    try await store.download(id) { f, phase in
                        let pct = Int(f * 100)
                        if last.bump(to: pct) { print("  \(pct)% \(phase)") }
                    }
                    print("\(id.rawValue): done, \(store.status(id).bytes / 1_048_576) MiB")
                }
            case "decode":
                guard args.count >= 3 else { throw RpcError.invalidParams("decode <in> <out.wav>") }
                let t0 = Date()
                let d = try await AudioDecoder.decode(input: URL(fileURLWithPath: args[1]), outputWav: URL(fileURLWithPath: args[2]), micSystemStereo: true)
                print("decoded \(d.durationMs) ms, \(d.sourceChannels) ch, energy frames: \(d.energy?.mic.count ?? 0), in \(String(format: "%.2f", Date().timeIntervalSince(t0)))s")
            case "asr":
                guard args.count >= 2 else { throw RpcError.invalidParams("asr <wav>") }
                let engine = ParakeetEngine(store: store)
                let t0 = Date()
                let r = try await engine.transcribe(wav: URL(fileURLWithPath: args[1])) { p in
                    FileHandle.standardError.write("  progress \(Int(p * 100))%\r".data(using: .utf8)!)
                }
                let wall = Date().timeIntervalSince(t0)
                print("\nASR: \(r.words.count) words, audio \(String(format: "%.1f", r.durationS))s, wall \(String(format: "%.1f", wall))s (\(String(format: "%.0f", r.durationS / max(wall, 0.001)))x realtime)")
                print(r.text.prefix(600))
                if let w = r.words.first, let l = r.words.last { print("first word: \(w.w)@\(w.s)s, last word: \(l.w)@\(l.e)s") }
            case "diarize":
                guard args.count >= 2 else { throw RpcError.invalidParams("diarize <wav>") }
                let d = CommunityDiarizer(store: store)
                let t0 = Date()
                let r = try await d.run(wav: URL(fileURLWithPath: args[1]), minSpeakers: nil, maxSpeakers: nil, numSpeakers: nil) { p in
                    FileHandle.standardError.write("  progress \(Int(p * 100))%\r".data(using: .utf8)!)
                }
                print("\nDiarize: \(r.segments.count) segments, \(r.speakers.count) speakers, wall \(String(format: "%.1f", Date().timeIntervalSince(t0)))s")
                for (id, emb) in r.speakers { print("  speaker \(id): embedding dim \(emb.count)") }
                for s in r.segments.prefix(12) { print(String(format: "  %7.2f - %7.2f  %@ (q=%.2f)", s.s, s.e, s.speaker, s.q)) }
            case "live":
                guard args.count >= 3, let secs = Double(args[1]) else { throw RpcError.invalidParams("live <seconds> <outDir>") }
                let dir = URL(fileURLWithPath: args[2], isDirectory: true)
                let r = MeetingRecorder(outputDir: dir)
                try r.start()
                let t = LiveTranscriber(store: store, sampleRate: r.currentSampleRate)
                r.attachLive(t)
                let t0 = Date()
                try await t.start { seg in
                    let who = seg.who.map { "[\($0)] " } ?? ""
                    let mark = seg.confirmed ? "✓" : "…"
                    print(String(format: "  %@ %6.1fs-%6.1fs %@%@", mark, Double(seg.startMs) / 1000, Double(seg.endMs) / 1000, who, seg.text))
                }
                print("live transcribing for \(secs)s (model ready after \(String(format: "%.1f", Date().timeIntervalSince(t0)))s) → \(dir.path)")
                try await Task.sleep(nanoseconds: UInt64(secs * 1_000_000_000))
                let live = await t.stop()
                r.attachLive(nil)
                let res = try r.stop()
                print("final live text (\(live.segments.count) segments): \(live.text)")
                print("saved \(res.recordingPath): \(res.durationMs) ms, dropped \(res.droppedFrames)")
            case "recover":
                guard args.count >= 2 else { throw RpcError.invalidParams("recover <outDir>") }
                let r = try MeetingRecorder.recover(outputDir: URL(fileURLWithPath: args[1], isDirectory: true))
                print("recovered \(r.recordingPath): \(r.durationMs) ms @ \(r.sampleRate) Hz")
            case "capture":
                guard args.count >= 3, let secs = Double(args[1]) else { throw RpcError.invalidParams("capture <seconds> <outDir>") }
                let dir = URL(fileURLWithPath: args[2], isDirectory: true)
                let r = MeetingRecorder(outputDir: dir)
                try r.start()
                print("recording for \(secs)s → \(dir.path) (play something in Zoom/Chrome and talk)")
                let end = Date().addingTimeInterval(secs)
                while Date() < end {
                    try await Task.sleep(nanoseconds: 1_000_000_000)
                    let s = r.status()
                    let bufs = r.bufferDiagnostics().map { String(format: "%.4f", $0) }.joined(separator: ",")
                    print(String(format: "  t=%5.1fs rec=%5.1fs mic=%.4f sys=%.4f buffers=[%@] in=%@ out=%@ gaps=%d dropped=%lld sysAudio=%@", Double(s.elapsedMs) / 1000, Double(s.recordedMs) / 1000, s.micRms, s.sysRms, bufs, s.inputDevice, s.outputDevice, s.gaps, s.droppedFrames, s.systemAudio))
                }
                let res = try r.stop()
                print("saved \(res.recordingPath): \(res.durationMs) ms @ \(res.sampleRate) Hz, gaps: \(res.gaps.count), dropped frames: \(res.droppedFrames)")
            default:
                throw RpcError.invalidParams("spike <models|decode|asr|diarize|capture>")
            }
        } catch {
            FileHandle.standardError.write("spike failed: \(error)\n".data(using: .utf8)!)
            exit(1)
        }
    }
}

/// Tiny thread-safe counter for progress printing.
final class Counter: @unchecked Sendable {
    private var value = -1
    private let lock = NSLock()
    /// Returns true when the 5 % bucket changed.
    func bump(to v: Int) -> Bool {
        lock.lock(); defer { lock.unlock() }
        let changed = v / 5 != value / 5
        value = v
        return changed
    }
}
