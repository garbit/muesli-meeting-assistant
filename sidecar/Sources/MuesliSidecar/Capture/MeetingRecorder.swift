import AVFoundation
import AudioToolbox
import CoreAudio
import Foundation
import os

struct GapRecord: Codable {
    /// Position in the recording timeline where the gap was inserted.
    let atMs: Int
    /// Wall-clock length of the gap; the same amount of silence was written so the
    /// recording timeline stays aligned with wall-clock time.
    let lengthMs: Int
    let reason: String
}

struct CaptureStatus: Codable {
    let state: String
    let elapsedMs: Int
    let recordedMs: Int
    let micRms: Float
    let sysRms: Float
    let inputDevice: String
    let outputDevice: String
    let sampleRate: Double
    let chunks: Int
    let gaps: Int
    let systemSilentMs: Int
    /// "ok" | "silent" (tap delivers only zeros while others play) | "missing" (no tap stream)
    let systemAudio: String
    let droppedFrames: Int64
}

struct CaptureResult: Codable {
    let recordingPath: String
    let durationMs: Int
    let sampleRate: Double
    let gaps: [GapRecord]
    let droppedFrames: Int64
}

/// Records the user's microphone and the system's output (every other process,
/// i.e. the remote side of a Zoom/Meet/Teams call) into one sample-aligned
/// two-channel stream: channel 0 = mic, channel 1 = system.
///
/// Mechanism: a private aggregate device whose main sub-device is the default
/// input device plus a global Core Audio process tap (macOS 14.2+) in the
/// aggregate's tap list with drift compensation. One IOProc delivers both.
///
/// Threading: Core Audio dispatches IOBlocks *synchronously* on `ioQueue`, so
/// the real-time thread is blocked for as long as anything on that queue runs.
/// `handleIO` therefore only mixes into preallocated scratch space and pushes
/// interleaved frames into a lock-light SPSC ring buffer. `writerQueue` drains
/// the ring to raw PCM chunk files every 100 ms (crash-safe: raw PCM has no
/// header to finalise). `controlQueue` owns the graph lifecycle, timers,
/// device-change listeners and the watchdog. `stop()` concatenates the chunks
/// into `recording.m4a`.
final class MeetingRecorder {
    // MARK: configuration
    private let outputDir: URL
    private let chunkSeconds: Double
    private let excludedPIDs: Set<pid_t> = [getpid(), getppid()]

    // MARK: queues
    private let ioQueue = DispatchQueue(label: "dev.garbett.muesli.capture.io", qos: .userInteractive)
    private let controlQueue = DispatchQueue(label: "dev.garbett.muesli.capture.control", qos: .userInitiated)
    private let writerQueue = DispatchQueue(label: "dev.garbett.muesli.capture.writer", qos: .utility)

    // MARK: graph state (controlQueue)
    private var tapID: AudioObjectID = kAudioObjectUnknown
    private var aggID: AudioObjectID = kAudioObjectUnknown
    private var procID: AudioDeviceIOProcID?
    private var sampleRate: Double = 48_000
    private var inputUID = ""
    private var inputName = "?"
    private var outputName = "?"
    private var running = false
    private var startDate: Date?
    private var gaps: [GapRecord] = []
    private var listeners: [(AudioObjectID, AudioObjectPropertyAddress, AudioObjectPropertyListenerBlock)] = []
    private var statusTimer: DispatchSourceTimer?
    private var pendingRebuild: DispatchWorkItem?
    private var lastRebuild = Date.distantPast
    private var silentRebuilds = 0
    private var tapMissing = false
    private var watchdogTick = 0

    // MARK: IO-thread state (ioQueue; configured on controlQueue while the proc is stopped)
    private var micBuffers = 1
    private let scratchCapacity = 16_384
    private let scratchMic: UnsafeMutablePointer<Float>
    private let scratchSys: UnsafeMutablePointer<Float>

    // MARK: counters shared with status() (stateLock)
    private let stateLock: UnsafeMutablePointer<os_unfair_lock>
    private var micRms: Float = 0
    private var sysRms: Float = 0
    private var lastNonZeroSystem = Date()
    private var droppedFrames: Int64 = 0
    private var bufferRms: [Float] = [0, 0, 0, 0]  // first 4 IO buffers, diagnostics only

    // MARK: ring buffer (SPSC: ioQueue produces, writerQueue consumes)
    private let ringLock: UnsafeMutablePointer<os_unfair_lock>
    private let ringCapacity = 96_000 * 20  // frames: 20 s at 96 kHz
    private let ring: UnsafeMutablePointer<Float>  // interleaved [mic, sys]
    private var ringWrite: Int64 = 0
    private var ringRead: Int64 = 0

    // MARK: writer state (writerQueue)
    private var writerTimer: DispatchSourceTimer?
    private var chunkHandle: FileHandle?
    private var chunkFrames: Int64 = 0
    private var chunkRate: Double = 48_000
    private var chunkURLs: [URL] = []
    private var recordedSeconds: Double = 0
    private var pendingGapFrames: Int64 = 0
    private var writeFailures = 0

    var onStatus: ((CaptureStatus) -> Void)?

    /// Optional near-real-time transcriber fed from the writer queue.
    private var live: LiveTranscriber?
    private let feedCapacity = 96_000  // 1 s at 96 kHz
    private let feedMic: UnsafeMutablePointer<Float>
    private let feedSys: UnsafeMutablePointer<Float>

    init(outputDir: URL, chunkSeconds: Double = 10) {
        self.outputDir = outputDir
        self.chunkSeconds = chunkSeconds
        scratchMic = .allocate(capacity: scratchCapacity)
        scratchSys = .allocate(capacity: scratchCapacity)
        scratchMic.initialize(repeating: 0, count: scratchCapacity)
        scratchSys.initialize(repeating: 0, count: scratchCapacity)
        ring = .allocate(capacity: ringCapacity * 2)
        ring.initialize(repeating: 0, count: ringCapacity * 2)
        feedMic = .allocate(capacity: feedCapacity)
        feedSys = .allocate(capacity: feedCapacity)
        feedMic.initialize(repeating: 0, count: feedCapacity)
        feedSys.initialize(repeating: 0, count: feedCapacity)
        stateLock = .allocate(capacity: 1)
        stateLock.initialize(to: os_unfair_lock())
        ringLock = .allocate(capacity: 1)
        ringLock.initialize(to: os_unfair_lock())
    }

    deinit {
        scratchMic.deallocate()
        scratchSys.deallocate()
        ring.deallocate()
        feedMic.deallocate()
        feedSys.deallocate()
        stateLock.deallocate()
        ringLock.deallocate()
    }

    // MARK: - lifecycle

    /// Written while recording so another process (startup recovery, a second
    /// app instance) can tell the chunks are live, not abandoned.
    static func lockFile(in dir: URL) -> URL { dir.appendingPathComponent("capture.lock") }

    /// pid of a live recorder holding `dir`, if any. A stale file (its process is
    /// gone) counts as no holder.
    static func liveRecorderPid(in dir: URL) -> pid_t? {
        guard let s = try? String(contentsOf: lockFile(in: dir), encoding: .utf8), let pid = pid_t(s.trimmingCharacters(in: .whitespacesAndNewlines)) else { return nil }
        // EPERM means the process exists but belongs to someone else: still alive.
        return (kill(pid, 0) == 0 || errno == EPERM) ? pid : nil
    }

    func start() throws {
        try FileManager.default.createDirectory(at: outputDir, withIntermediateDirectories: true)
        if let pid = Self.liveRecorderPid(in: outputDir), pid != getpid() {
            throw RpcError.busy("another recorder (pid \(pid)) is already writing to \(outputDir.lastPathComponent)")
        }
        let lock = Self.lockFile(in: outputDir)
        try String(getpid()).write(to: lock, atomically: true, encoding: .utf8)
        do {
            try controlQueue.sync {
                try buildGraph()
                startDate = Date()
                running = true
                writerQueue.sync { chunkRate = sampleRate }
                startWriterTimer()
                installListeners()
                startStatusTimer()
            }
        } catch {
            // Nothing was recorded; do not leave a lock that says otherwise.
            try? FileManager.default.removeItem(at: lock)
            throw error
        }
        log("capture started: in=\(inputName) out=\(outputName) rate=\(sampleRate) micBuffers=\(micBuffers) tapMissing=\(tapMissing)")
    }

    /// Stop capturing and produce `recording.m4a`. On concatenation failure the
    /// raw chunks are kept so `recover(outputDir:)` can finish the job later.
    func stop() throws -> CaptureResult {
        let (chunks, gaps, dropped) = controlQueue.sync { () -> ([URL], [GapRecord], Int64) in
            running = false
            statusTimer?.cancel()
            statusTimer = nil
            pendingRebuild?.cancel()
            pendingRebuild = nil
            removeListeners()
            teardownGraph()
            writerQueue.sync {
                writerTimer?.cancel()
                writerTimer = nil
                drain()
                closeChunk()
            }
            return (chunkURLs, self.gaps, counters().dropped)
        }
        let recording = outputDir.appendingPathComponent("recording.m4a")
        defer { try? FileManager.default.removeItem(at: Self.lockFile(in: outputDir)) }
        let (rate, durationMs) = try Self.concatenate(chunks: chunks, to: recording)
        for c in chunks { try? FileManager.default.removeItem(at: c) }
        log("capture stopped: \(durationMs) ms, \(chunks.count) chunks, \(gaps.count) gaps, \(dropped) dropped frames → \(recording.lastPathComponent)")
        return CaptureResult(recordingPath: recording.path, durationMs: durationMs, sampleRate: rate, gaps: gaps, droppedFrames: dropped)
    }

    /// Rebuild `recording.m4a` from chunk files left behind by a crash. When the
    /// sidecar that owned the directory finalised the file itself (it does so when
    /// its parent dies) and only `recording.m4a` remains, report that file.
    static func recover(outputDir: URL) throws -> CaptureResult {
        if let pid = liveRecorderPid(in: outputDir) {
            throw RpcError.busy("recording in progress (pid \(pid)); refusing to recover \(outputDir.lastPathComponent)")
        }
        let recording = outputDir.appendingPathComponent("recording.m4a")
        let chunks = try chunkFiles(in: outputDir)
        if chunks.isEmpty {
            guard FileManager.default.fileExists(atPath: recording.path) else { throw RpcError.server("no chunk files in \(outputDir.path)") }
            let file = try AVAudioFile(forReading: recording)
            let rate = file.processingFormat.sampleRate
            let durationMs = Int(Double(file.length) / rate * 1000)
            try? FileManager.default.removeItem(at: lockFile(in: outputDir))
            log("recover: \(recording.lastPathComponent) was already finalised (\(durationMs) ms)")
            return CaptureResult(recordingPath: recording.path, durationMs: durationMs, sampleRate: rate, gaps: [], droppedFrames: 0)
        }
        let (rate, durationMs) = try concatenate(chunks: chunks, to: recording)
        for c in chunks { try? FileManager.default.removeItem(at: c) }
        try? FileManager.default.removeItem(at: lockFile(in: outputDir))
        log("recovered \(chunks.count) chunk(s) → \(recording.lastPathComponent) (\(durationMs) ms)")
        return CaptureResult(recordingPath: recording.path, durationMs: durationMs, sampleRate: rate, gaps: [], droppedFrames: 0)
    }

    func status() -> CaptureStatus {
        controlQueue.sync { currentStatus() }
    }

    var currentSampleRate: Double {
        controlQueue.sync { sampleRate }
    }

    /// Attach (or detach with nil) a live transcriber; takes effect on the next drain.
    func attachLive(_ transcriber: LiveTranscriber?) {
        writerQueue.sync { live = transcriber }
    }

    func bufferDiagnostics() -> [Float] {
        counters().bufferRms
    }

    // MARK: - status

    private struct Counters {
        var micRms: Float
        var sysRms: Float
        var lastNonZeroSystem: Date
        var dropped: Int64
        var bufferRms: [Float]
    }

    private func counters() -> Counters {
        os_unfair_lock_lock(stateLock)
        defer { os_unfair_lock_unlock(stateLock) }
        return Counters(micRms: micRms, sysRms: sysRms, lastNonZeroSystem: lastNonZeroSystem, dropped: droppedFrames, bufferRms: bufferRms)
    }

    private func currentStatus() -> CaptureStatus {
        let c = counters()
        let recorded = writerQueue.sync { recordedSeconds }
        let silentMs = Int(Date().timeIntervalSince(c.lastNonZeroSystem) * 1000)
        let systemAudio = tapMissing ? "missing" : (silentRebuilds >= 2 && silentMs > 30_000 ? "silent" : "ok")
        return CaptureStatus(
            state: running ? "recording" : "stopped",
            elapsedMs: startDate.map { Int(Date().timeIntervalSince($0) * 1000) } ?? 0,
            recordedMs: Int(recorded * 1000), micRms: c.micRms, sysRms: c.sysRms, inputDevice: inputName, outputDevice: outputName,
            sampleRate: sampleRate, chunks: writerQueue.sync { chunkURLs.count }, gaps: gaps.count, systemSilentMs: silentMs,
            systemAudio: systemAudio, droppedFrames: c.dropped)
    }

    // MARK: - graph (controlQueue)

    private func buildGraph() throws {
        let micID = try AudioObjectID.defaultInputDevice()
        let outID = try AudioObjectID.defaultOutputDevice()
        guard micID.isValid else { throw RpcError.server("no default input device") }
        let micUID = try micID.deviceUID
        inputUID = micUID
        inputName = micID.name
        outputName = outID.isValid ? outID.name : "?"

        // Global tap of every process except ourselves and our parent (the app).
        let excluded = excludedPIDs.compactMap { AudioObjectID.processObject(forPID: $0) }
        let desc = CATapDescription(stereoGlobalTapButExcludeProcesses: excluded)
        desc.uuid = UUID()
        desc.name = "Muesli meeting tap"
        desc.isPrivate = true
        desc.muteBehavior = CATapMuteBehavior.unmuted
        var tap: AudioObjectID = kAudioObjectUnknown
        var st = AudioHardwareCreateProcessTap(desc, &tap)
        guard st == noErr, tap.isValid else { throw CoreAudioError(what: "AudioHardwareCreateProcessTap", status: st) }
        tapID = tap

        let aggDesc: [String: Any] = [
            kAudioAggregateDeviceNameKey: "Muesli Capture",
            kAudioAggregateDeviceUIDKey: "dev.garbett.muesli.capture." + UUID().uuidString,
            kAudioAggregateDeviceIsPrivateKey: true,
            kAudioAggregateDeviceIsStackedKey: false,
            kAudioAggregateDeviceTapAutoStartKey: true,
            kAudioAggregateDeviceMainSubDeviceKey: micUID,
            kAudioAggregateDeviceSubDeviceListKey: [[kAudioSubDeviceUIDKey: micUID]],
            kAudioAggregateDeviceTapListKey: [[kAudioSubTapUIDKey: desc.uuid.uuidString, kAudioSubTapDriftCompensationKey: true]],
        ]
        var agg: AudioObjectID = kAudioObjectUnknown
        st = AudioHardwareCreateAggregateDevice(aggDesc as CFDictionary, &agg)
        guard st == noErr, agg.isValid else {
            AudioHardwareDestroyProcessTap(tapID)
            tapID = kAudioObjectUnknown
            throw CoreAudioError(what: "AudioHardwareCreateAggregateDevice", status: st)
        }
        aggID = agg
        sampleRate = (try? aggID.read(kAudioDevicePropertyNominalSampleRate, initial: Double(48_000))) ?? 48_000

        // Sub-device streams come first, the tap stream last. Derive the mic buffer
        // count from the aggregate itself so a multi-stream mic or a missing tap
        // cannot shift the mapping.
        let streams = aggID.inputStreamCount()
        tapMissing = streams < 2
        micBuffers = max(streams - (tapMissing ? 0 : 1), 1)
        log("aggregate \(aggID): \(streams) input streams (mic \(micBuffers), tap \(tapMissing ? "missing" : "present")), rate \(sampleRate)")

        var pid: AudioDeviceIOProcID?
        st = AudioDeviceCreateIOProcIDWithBlock(&pid, aggID, ioQueue) { [weak self] _, inData, _, _, _ in
            self?.handleIO(inData)
        }
        guard st == noErr, let pid else {
            teardownGraph()
            throw CoreAudioError(what: "AudioDeviceCreateIOProcIDWithBlock", status: st)
        }
        procID = pid
        st = AudioDeviceStart(aggID, procID)
        guard st == noErr else {
            teardownGraph()
            throw CoreAudioError(what: "AudioDeviceStart", status: st)
        }
        installRateListener()
    }

    private func teardownGraph() {
        removeRateListener()
        if aggID.isValid {
            if let procID {
                AudioDeviceStop(aggID, procID)
                // Let any in-flight IOBlock finish before destroying the proc.
                ioQueue.sync {}
                AudioDeviceDestroyIOProcID(aggID, procID)
            }
            AudioHardwareDestroyAggregateDevice(aggID)
        }
        if tapID.isValid { AudioHardwareDestroyProcessTap(tapID) }
        procID = nil
        aggID = kAudioObjectUnknown
        tapID = kAudioObjectUnknown
    }

    /// Tear down and rebuild the graph, inserting silence for the time lost so the
    /// recording timeline stays aligned with wall-clock time.
    private func rebuild(reason: String) {
        guard running else { return }
        let gapStart = Date()
        log("rebuilding capture graph: \(reason)")
        teardownGraph()
        // Drain frames captured at the old rate before anything at the new rate arrives.
        writerQueue.sync { drain() }
        var attempt = 0
        while running {
            do {
                try buildGraph()
                break
            } catch {
                attempt += 1
                log("rebuild attempt \(attempt) failed: \(error)")
                if attempt >= 5 { return }
                Thread.sleep(forTimeInterval: 1)
            }
        }
        guard running else { return }
        let gapSeconds = min(Date().timeIntervalSince(gapStart), 120)
        let rate = sampleRate
        let atMs = writerQueue.sync { () -> Int in
            if chunkRate != rate {
                closeChunk()
                chunkRate = rate
            }
            pendingGapFrames += Int64(gapSeconds * rate)
            return Int(recordedSeconds * 1000)
        }
        gaps.append(GapRecord(atMs: atMs, lengthMs: Int(gapSeconds * 1000), reason: reason))
        lastRebuild = Date()
        os_unfair_lock_lock(stateLock)
        lastNonZeroSystem = Date()
        os_unfair_lock_unlock(stateLock)
    }

    /// Coalesce bursts of device notifications into one rebuild.
    private func scheduleRebuild(reason: String) {
        pendingRebuild?.cancel()
        let item = DispatchWorkItem { [weak self] in
            guard let self, self.running else { return }
            self.pendingRebuild = nil
            // Nothing to do if the default input is unchanged and the graph is alive.
            if reason.contains("input"), let id = try? AudioObjectID.defaultInputDevice(), (try? id.deviceUID) == self.inputUID, self.aggID.isValid {
                return
            }
            self.rebuild(reason: reason)
        }
        pendingRebuild = item
        controlQueue.asyncAfter(deadline: .now() + 0.5, execute: item)
    }

    // MARK: - listeners, timers, watchdog (controlQueue)

    private func installListeners() {
        for selector in [kAudioHardwarePropertyDefaultInputDevice, kAudioHardwarePropertyDefaultOutputDevice] {
            var addr = address(selector)
            let name = selector == kAudioHardwarePropertyDefaultInputDevice ? "default input device changed" : "default output device changed"
            let block: AudioObjectPropertyListenerBlock = { [weak self] _, _ in
                self?.scheduleRebuild(reason: name)
            }
            AudioObjectAddPropertyListenerBlock(AudioObjectID.system, &addr, controlQueue, block)
            listeners.append((AudioObjectID.system, addr, block))
        }
    }

    private var rateListener: (AudioObjectID, AudioObjectPropertyAddress, AudioObjectPropertyListenerBlock)?

    private func installRateListener() {
        var addr = address(kAudioDevicePropertyNominalSampleRate)
        let block: AudioObjectPropertyListenerBlock = { [weak self] _, _ in
            self?.scheduleRebuild(reason: "sample rate changed")
        }
        AudioObjectAddPropertyListenerBlock(aggID, &addr, controlQueue, block)
        rateListener = (aggID, addr, block)
    }

    private func removeRateListener() {
        if let (obj, addr, block) = rateListener {
            var a = addr
            AudioObjectRemovePropertyListenerBlock(obj, &a, controlQueue, block)
            rateListener = nil
        }
    }

    private func removeListeners() {
        for (obj, addr, block) in listeners {
            var a = addr
            AudioObjectRemovePropertyListenerBlock(obj, &a, controlQueue, block)
        }
        listeners.removeAll()
    }

    private func startStatusTimer() {
        let t = DispatchSource.makeTimerSource(queue: controlQueue)
        t.schedule(deadline: .now() + 0.25, repeating: 0.25)
        t.setEventHandler { [weak self] in
            guard let self, self.running else { return }
            self.onStatus?(self.currentStatus())
            self.watchdogTick += 1
            if self.watchdogTick % 4 == 0 { self.watchdog() }
        }
        t.resume()
        statusTimer = t
    }

    /// A tap that delivers only exact zeros for 30 s while another process is
    /// playing audio is broken (seen on macOS 26.x). Rebuild at most twice per
    /// recording, then report `systemAudio: "silent"` instead of looping.
    private func watchdog() {
        guard !tapMissing else { return }
        let silent = Date().timeIntervalSince(counters().lastNonZeroSystem)
        if silent < 5 { silentRebuilds = 0 }
        guard silent > 30, silentRebuilds < 2, Date().timeIntervalSince(lastRebuild) > 60 else { return }
        guard AudioObjectID.otherProcessesRunningOutput(excluding: excludedPIDs) else { return }
        silentRebuilds += 1
        rebuild(reason: "silent system tap while another process plays audio")
    }

    // MARK: - IO (ioQueue, synchronous with the HAL thread: no allocation, no locks held long)

    private func handleIO(_ inData: UnsafePointer<AudioBufferList>) {
        let abl = UnsafeMutableAudioBufferListPointer(UnsafeMutablePointer(mutating: inData))
        let count = abl.count
        guard count > 0 else { return }
        let micRange = 0..<min(micBuffers, count)
        let tapIndex = (!tapMissing && count > micBuffers) ? count - 1 : -1

        // Frame count from the first non-empty mic buffer (the main sub-device).
        var frames = 0
        for i in micRange {
            let b = abl[i]
            if b.mDataByteSize > 0 {
                frames = Int(b.mDataByteSize) / (4 * max(Int(b.mNumberChannels), 1))
                break
            }
        }
        if frames == 0, tapIndex >= 0 {
            let b = abl[tapIndex]
            frames = Int(b.mDataByteSize) / (4 * max(Int(b.mNumberChannels), 1))
        }
        guard frames > 0 else { return }
        if frames > scratchCapacity {
            os_unfair_lock_lock(stateLock)
            droppedFrames += Int64(frames - scratchCapacity)
            os_unfair_lock_unlock(stateLock)
            frames = scratchCapacity
        }

        for f in 0..<frames {
            scratchMic[f] = 0
            scratchSys[f] = 0
        }
        var rms0: Float = 0, rms1: Float = 0, rms2: Float = 0, rms3: Float = 0
        var activeMic = 0
        for i in micRange {
            let b = abl[i]
            guard b.mDataByteSize > 0 else { continue }
            activeMic += 1
            let e = Self.mixInto(scratchMic, b, frames: frames)
            switch i {
            case 0: rms0 = e
            case 1: rms1 = e
            case 2: rms2 = e
            default: rms3 = e
            }
        }
        if activeMic > 1 {
            let inv = 1 / Float(activeMic)
            for f in 0..<frames { scratchMic[f] *= inv }
        }
        var sysE: Float = 0
        if tapIndex >= 0 {
            let b = abl[tapIndex]
            if b.mDataByteSize > 0 {
                sysE = Self.mixInto(scratchSys, b, frames: frames)
                switch tapIndex {
                case 1: rms1 = sysE
                case 2: rms2 = sysE
                case 3: rms3 = sysE
                default: break
                }
            }
        }
        var micE: Float = 0
        for f in 0..<frames { micE += scratchMic[f] * scratchMic[f] }
        let micLevel = (micE / Float(frames)).squareRoot()
        let sysLevel = sysE

        // Push interleaved frames into the ring. Only the index exchange is locked.
        os_unfair_lock_lock(ringLock)
        let w = ringWrite
        let r = ringRead
        os_unfair_lock_unlock(ringLock)
        let free = Int64(ringCapacity) - (w - r)
        var dropped: Int64 = 0
        var toWrite = frames
        if Int64(toWrite) > free {
            dropped = Int64(toWrite) - free
            toWrite = Int(free)
        }
        if toWrite > 0 {
            var pos = Int(w % Int64(ringCapacity))
            for f in 0..<toWrite {
                ring[pos * 2] = scratchMic[f]
                ring[pos * 2 + 1] = scratchSys[f]
                pos += 1
                if pos == ringCapacity { pos = 0 }
            }
            os_unfair_lock_lock(ringLock)
            ringWrite = w + Int64(toWrite)
            os_unfair_lock_unlock(ringLock)
        }

        os_unfair_lock_lock(stateLock)
        micRms = micRms * 0.8 + micLevel * 0.2
        sysRms = sysRms * 0.8 + sysLevel * 0.2
        if sysE != 0 { lastNonZeroSystem = Date() }
        droppedFrames += dropped
        bufferRms[0] = rms0
        bufferRms[1] = rms1
        bufferRms[2] = rms2
        bufferRms[3] = rms3
        os_unfair_lock_unlock(stateLock)
    }

    /// Add the mono mix of `b` into `dst`; returns the RMS of the mix.
    @inline(__always)
    private static func mixInto(_ dst: UnsafeMutablePointer<Float>, _ b: AudioBuffer, frames: Int) -> Float {
        guard let p = b.mData?.assumingMemoryBound(to: Float.self) else { return 0 }
        let ch = max(Int(b.mNumberChannels), 1)
        let available = Int(b.mDataByteSize) / (4 * ch)
        let n = min(frames, available)
        let inv = 1 / Float(ch)
        var e: Float = 0
        if ch == 1 {
            for f in 0..<n {
                let s = p[f]
                dst[f] += s
                e += s * s
            }
        } else {
            for f in 0..<n {
                var s: Float = 0
                for c in 0..<ch { s += p[f * ch + c] }
                s *= inv
                dst[f] += s
                e += s * s
            }
        }
        return n > 0 ? (e / Float(n)).squareRoot() : 0
    }

    // MARK: - writer (writerQueue)

    private func startWriterTimer() {
        let t = DispatchSource.makeTimerSource(queue: writerQueue)
        t.schedule(deadline: .now() + 0.1, repeating: 0.1)
        t.setEventHandler { [weak self] in self?.drain() }
        t.resume()
        writerTimer = t
    }

    /// Move everything the IO thread has produced from the ring to the current chunk.
    private func drain() {
        os_unfair_lock_lock(ringLock)
        var r = ringRead
        let w = ringWrite
        os_unfair_lock_unlock(ringLock)
        var frames = w - r
        guard frames > 0 || pendingGapFrames > 0 else { return }
        if chunkHandle == nil { openChunk() }
        if pendingGapFrames > 0 {
            writeSilence(frames: pendingGapFrames)
            pendingGapFrames = 0
        }
        let cap = Int64(ringCapacity)
        while frames > 0 {
            let pos = Int(r % cap)
            let n = Int(min(frames, cap - Int64(pos)))
            writeFrames(from: ring + pos * 2, frames: n)
            r += Int64(n)
            frames -= Int64(n)
            rotateIfNeeded()
        }
        os_unfair_lock_lock(ringLock)
        ringRead = r
        os_unfair_lock_unlock(ringLock)
    }

    private func writeFrames(from ptr: UnsafeMutablePointer<Float>, frames: Int) {
        guard let handle = chunkHandle, frames > 0 else { return }
        let data = Data(bytesNoCopy: UnsafeMutableRawPointer(ptr), count: frames * 8, deallocator: .none)
        do {
            try handle.write(contentsOf: data)
            chunkFrames += Int64(frames)
            recordedSeconds += Double(frames) / chunkRate
        } catch {
            writeFailures += 1
            if writeFailures == 1 || writeFailures % 100 == 0 { log("chunk write failed (\(writeFailures)): \(error)") }
        }
        feedLive(from: ptr, frames: frames)
    }

    /// De-interleave a block for the live transcriber, in slices of at most 1 s.
    private func feedLive(from ptr: UnsafeMutablePointer<Float>, frames: Int) {
        guard let live, live.isRunning else { return }
        var offset = 0
        while offset < frames {
            let n = min(frames - offset, feedCapacity)
            var micE: Float = 0, sysE: Float = 0
            for f in 0..<n {
                let m = ptr[(offset + f) * 2]
                let s = ptr[(offset + f) * 2 + 1]
                feedMic[f] = m
                feedSys[f] = s
                micE += m * m
                sysE += s * s
            }
            live.feed(mic: feedMic, sys: feedSys, frames: n, micEnergy: micE, sysEnergy: sysE)
            offset += n
        }
    }

    private func writeSilence(frames: Int64) {
        guard frames > 0 else { return }
        var remaining = frames
        let block = 48_000
        let zeros = [Float](repeating: 0, count: block * 2)
        while remaining > 0 {
            let n = Int(min(remaining, Int64(block)))
            zeros.withUnsafeBufferPointer { p in
                writeFrames(from: UnsafeMutablePointer(mutating: p.baseAddress!), frames: n)
            }
            remaining -= Int64(n)
            rotateIfNeeded()
        }
    }

    private func rotateIfNeeded() {
        if chunkFrames >= Int64(chunkSeconds * chunkRate) {
            closeChunk()
            openChunk()
        }
    }

    /// Chunks are raw interleaved Float32 LE stereo; the sample rate is in the name
    /// so no header or manifest needs finalising if the process dies.
    private func openChunk() {
        let url = outputDir.appendingPathComponent(String(format: "chunk-%05d-%d.pcm", chunkURLs.count, Int(chunkRate)))
        guard FileManager.default.createFile(atPath: url.path, contents: nil), let h = FileHandle(forWritingAtPath: url.path) else {
            writeFailures += 1
            if writeFailures == 1 { log("cannot open chunk \(url.lastPathComponent)") }
            return
        }
        chunkHandle = h
        chunkFrames = 0
        chunkURLs.append(url)
    }

    private func closeChunk() {
        try? chunkHandle?.close()
        chunkHandle = nil
        chunkFrames = 0
    }

    // MARK: - finalisation

    static func chunkFiles(in dir: URL) throws -> [URL] {
        let items = try FileManager.default.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil)
        return items.filter { $0.lastPathComponent.hasPrefix("chunk-") && $0.pathExtension == "pcm" }.sorted { $0.lastPathComponent < $1.lastPathComponent }
    }

    private static func sampleRate(ofChunk chunk: URL) -> Double {
        let parts = chunk.deletingPathExtension().lastPathComponent.split(separator: "-")
        return parts.count >= 3 ? Double(parts[2]) ?? 48_000 : 48_000
    }

    /// Concatenate raw PCM chunks into one AAC file; returns the output sample rate
    /// and duration. Chunks recorded at a different rate are resampled.
    static func concatenate(chunks: [URL], to url: URL) throws -> (Double, Int) {
        try? FileManager.default.removeItem(at: url)
        guard let first = chunks.first else { throw RpcError.server("no audio captured") }
        let rate = sampleRate(ofChunk: first)
        let outFormat = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: rate, channels: 2, interleaved: true)!
        let settings: [String: Any] = [
            AVFormatIDKey: kAudioFormatMPEG4AAC, AVSampleRateKey: rate, AVNumberOfChannelsKey: 2, AVEncoderBitRateKey: 160_000,
        ]
        let out: AVAudioFile
        do {
            out = try AVAudioFile(forWriting: url, settings: settings, commonFormat: .pcmFormatFloat32, interleaved: true)
        } catch {
            throw RpcError.server("cannot create \(url.lastPathComponent): \(error.localizedDescription)")
        }
        var totalFrames: Int64 = 0
        do {
            for chunk in chunks {
                let chunkRate = sampleRate(ofChunk: chunk)
                let inFormat = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: chunkRate, channels: 2, interleaved: true)!
                let handle = try FileHandle(forReadingFrom: chunk)
                defer { try? handle.close() }
                let converter = chunkRate == rate ? nil : AVAudioConverter(from: inFormat, to: outFormat)
                let blockFrames = Int(chunkRate)  // 1 s
                while true {
                    guard let data = try handle.read(upToCount: blockFrames * 8), !data.isEmpty else { break }
                    let frames = data.count / 8
                    guard frames > 0, let inBuf = AVAudioPCMBuffer(pcmFormat: inFormat, frameCapacity: AVAudioFrameCount(frames)) else { break }
                    data.withUnsafeBytes { raw in
                        inBuf.floatChannelData![0].update(from: raw.bindMemory(to: Float.self).baseAddress!, count: frames * 2)
                    }
                    inBuf.frameLength = AVAudioFrameCount(frames)
                    if let converter {
                        let ratio = rate / chunkRate
                        guard let outBuf = AVAudioPCMBuffer(pcmFormat: outFormat, frameCapacity: AVAudioFrameCount(Double(frames) * ratio) + 64) else { break }
                        var err: NSError?
                        var fed = false
                        _ = converter.convert(to: outBuf, error: &err) { (_: AVAudioPacketCount, status: UnsafeMutablePointer<AVAudioConverterInputStatus>) -> AVAudioBuffer? in
                            if fed {
                                status.pointee = AVAudioConverterInputStatus.noDataNow
                                return nil
                            }
                            fed = true
                            status.pointee = AVAudioConverterInputStatus.haveData
                            return inBuf
                        }
                        if let err { throw err }
                        if outBuf.frameLength > 0 {
                            try out.write(from: outBuf)
                            totalFrames += Int64(outBuf.frameLength)
                        }
                    } else {
                        try out.write(from: inBuf)
                        totalFrames += Int64(frames)
                    }
                }
            }
        } catch {
            try? FileManager.default.removeItem(at: url)
            throw RpcError.server("concatenating chunks failed: \(error.localizedDescription); chunks kept in \(url.deletingLastPathComponent().path)")
        }
        return (rate, Int(Double(totalFrames) / rate * 1000))
    }
}
