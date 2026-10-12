import AVFoundation
import AudioToolbox
import CoreAudio
import Foundation
import os

/// Processes whose audio a Muesli tap must never include: this sidecar and the
/// app that spawned it. Shared by the recorder and the permission probe.
let selfAndParentPIDs: Set<pid_t> = [getpid(), getppid()]

/// Raw facts from one system-audio check. The Rust side turns them into a
/// verdict so the rule can change without rebuilding the sidecar.
struct SystemAudioProbeResult: Codable {
    var subDevice = ""
    var tapCreate: Int32 = 0
    var aggregateCreate: Int32 = 0
    /// Input streams of the aggregate (sub-device streams first, tap last).
    var streams = 0
    /// Input streams the sub-device contributes on its own (0 for plain speakers,
    /// more for a duplex headset), so `streams > subDeviceStreams` means the tap is there.
    var subDeviceStreams = 0
    var tapPresent = false
    var start: Int32 = 0
    var firstIoMs: Int?
    /// Whether the tap delivered real audio: a sample at least `heardThreshold`
    /// in magnitude (a fifth of the probe's own tone). A denied tap measured
    /// exactly 0.0 on every attempt on macOS 26.5; the threshold keeps a stray
    /// 1e-12 blip from ending an attempt before the tone has arrived.
    var nonZero = false
    /// Largest |sample| seen on the tap stream (0 when it stayed silent).
    var peak: Float = 0
    /// Whether the probe's own tone was actually playing (AVAudioEngine running).
    var toneRunning = false
    var attempts = 0
    var elapsedMs = 0
    var error: String?
}

/// Exercises a Core Audio process tap the way a recording does and reports
/// whether it delivers audio.
///
/// macOS has no public API for the System Audio Recording permission:
/// `AudioHardwareCreateProcessTap` succeeds without it and a denied tap simply
/// delivers zeros. An IOProc firing proves nothing either, because the
/// sub-device drives it. So the probe plays an inaudible tone itself (-80 dBFS)
/// and looks for any non-zero sample on the tap stream; the tap sees each
/// process's audio before the device volume is applied. If macOS has not
/// decided yet, its prompt appears during the first attempt; attempts repeat
/// until audio arrives or the budget runs out, because the tap can stay silent
/// until the user answers.
enum SystemAudioProbe {
    /// Tone amplitude the probe plays, and the fraction of it that counts as heard.
    static let toneAmplitude: Float = 1e-4
    static let heardThreshold: Float = 2e-5

    static func run(timeoutMs: Int) async -> SystemAudioProbeResult {
        await withCheckedContinuation { (cont: CheckedContinuation<SystemAudioProbeResult, Never>) in
            DispatchQueue.global(qos: .userInitiated).async {
                cont.resume(returning: runBlocking(timeoutMs: timeoutMs))
            }
        }
    }

    private static func runBlocking(timeoutMs: Int) -> SystemAudioProbeResult {
        let started = Date()
        let deadline = started.addingTimeInterval(Double(max(timeoutMs, 500)) / 1000)
        var result = SystemAudioProbeResult()
        let tone = ToneGenerator()
        tone.start()
        result.toneRunning = tone.isRunning
        while true {
            result.attempts += 1
            let remaining = deadline.timeIntervalSinceNow
            attempt(waitSeconds: min(1.0, max(remaining, 0.3)), into: &result)
            log(
                "system audio probe attempt \(result.attempts): sub=\(result.subDevice) tap=\(result.tapCreate) agg=\(result.aggregateCreate) "
                    + "streams=\(result.streams)/\(result.subDeviceStreams) present=\(result.tapPresent) start=\(result.start) "
                    + "firstIo=\(result.firstIoMs.map(String.init) ?? "-") nonZero=\(result.nonZero) peak=\(result.peak) tone=\(result.toneRunning)")
            if result.nonZero || result.error != nil || Date() >= deadline { break }
            Thread.sleep(forTimeInterval: 0.5)
        }
        tone.stop()
        result.elapsedMs = Int(Date().timeIntervalSince(started) * 1000)
        return result
    }

    /// One create → start → listen → tear down cycle. Mirrors `MeetingRecorder.buildGraph`
    /// on purpose (same tap kind, same aggregate shape) so the permission semantics match
    /// a real recording, but uses the default output device as the sub-device so the probe
    /// never touches the microphone and works on a Mac without an input device.
    private static func attempt(waitSeconds: TimeInterval, into r: inout SystemAudioProbeResult) {
        r.nonZero = false
        r.firstIoMs = nil
        r.peak = 0
        var sub = (try? AudioObjectID.defaultOutputDevice()) ?? kAudioObjectUnknown
        if !sub.isValid { sub = (try? AudioObjectID.defaultInputDevice()) ?? kAudioObjectUnknown }
        guard sub.isValid, let subUID = try? sub.deviceUID else {
            r.error = "no default audio device"
            return
        }
        r.subDevice = sub.name
        r.subDeviceStreams = sub.inputStreamCount()

        // Global tap excluding only the app: the sidecar's own tone must be included.
        let excluded = [getppid()].compactMap { AudioObjectID.processObject(forPID: $0) }
        let desc = CATapDescription(stereoGlobalTapButExcludeProcesses: excluded)
        desc.uuid = UUID()
        desc.name = "Muesli permission check"
        desc.isPrivate = true
        desc.muteBehavior = CATapMuteBehavior.unmuted
        var tap: AudioObjectID = kAudioObjectUnknown
        r.tapCreate = AudioHardwareCreateProcessTap(desc, &tap)
        guard r.tapCreate == noErr, tap.isValid else { return }
        defer { AudioHardwareDestroyProcessTap(tap) }

        let aggDesc: [String: Any] = [
            kAudioAggregateDeviceNameKey: "Muesli Permission Check",
            kAudioAggregateDeviceUIDKey: "dev.garbett.muesli.probe." + UUID().uuidString,
            kAudioAggregateDeviceIsPrivateKey: true,
            kAudioAggregateDeviceIsStackedKey: false,
            kAudioAggregateDeviceTapAutoStartKey: true,
            kAudioAggregateDeviceMainSubDeviceKey: subUID,
            kAudioAggregateDeviceSubDeviceListKey: [[kAudioSubDeviceUIDKey: subUID]],
            kAudioAggregateDeviceTapListKey: [[kAudioSubTapUIDKey: desc.uuid.uuidString, kAudioSubTapDriftCompensationKey: true]],
        ]
        var agg: AudioObjectID = kAudioObjectUnknown
        r.aggregateCreate = AudioHardwareCreateAggregateDevice(aggDesc as CFDictionary, &agg)
        guard r.aggregateCreate == noErr, agg.isValid else { return }
        defer { AudioHardwareDestroyAggregateDevice(agg) }

        r.streams = agg.inputStreamCount()
        r.tapPresent = r.streams > r.subDeviceStreams
        guard r.tapPresent else { return }

        let listener = TapListener(subDeviceStreams: r.subDeviceStreams, threshold: heardThreshold)
        let queue = DispatchQueue(label: "dev.garbett.muesli.probe.io", qos: .userInteractive)
        var procID: AudioDeviceIOProcID?
        let created = AudioDeviceCreateIOProcIDWithBlock(&procID, agg, queue) { _, inData, _, _, _ in
            listener.handle(inData)
        }
        guard created == noErr, let procID else {
            r.error = "AudioDeviceCreateIOProcIDWithBlock failed: OSStatus \(created)"
            return
        }
        defer {
            AudioDeviceStop(agg, procID)
            // Let any in-flight IOBlock finish before destroying the proc.
            queue.sync {}
            AudioDeviceDestroyIOProcID(agg, procID)
        }
        let t0 = Date()
        r.start = AudioDeviceStart(agg, procID)
        guard r.start == noErr else { return }
        _ = listener.heard.wait(timeout: .now() + waitSeconds)
        let (firstIo, nonZero, peak) = listener.snapshot()
        r.firstIoMs = firstIo.map { Int($0.timeIntervalSince(t0) * 1000) }
        r.nonZero = nonZero
        r.peak = peak
    }
}

/// Watches the tap buffers of the probe aggregate for the first non-zero sample.
/// Runs on the HAL's IO thread: no allocation, one short lock.
private final class TapListener: @unchecked Sendable {
    let heard = DispatchSemaphore(value: 0)
    private let subDeviceStreams: Int
    private let threshold: Float
    private let lock: UnsafeMutablePointer<os_unfair_lock>
    private var firstIo: Date?
    private var nonZero = false
    private var peak: Float = 0
    private var signalled = false

    init(subDeviceStreams: Int, threshold: Float) {
        self.subDeviceStreams = subDeviceStreams
        self.threshold = threshold
        lock = .allocate(capacity: 1)
        lock.initialize(to: os_unfair_lock())
    }

    deinit { lock.deallocate() }

    func handle(_ inData: UnsafePointer<AudioBufferList>) {
        let abl = UnsafeMutableAudioBufferListPointer(UnsafeMutablePointer(mutating: inData))
        let count = abl.count
        var found = false
        var maxAbs: Float = 0
        // Sub-device buffers come first; everything after them belongs to the tap.
        if count > subDeviceStreams {
            for i in subDeviceStreams..<count {
                let b = abl[i]
                guard let p = b.mData?.assumingMemoryBound(to: Float.self) else { continue }
                let n = Int(b.mDataByteSize) / 4
                var j = 0
                while j < n {
                    let a = abs(p[j])
                    if a > maxAbs { maxAbs = a }
                    j += 1
                }
            }
            found = maxAbs >= threshold
        }
        os_unfair_lock_lock(lock)
        if firstIo == nil { firstIo = Date() }
        if found { nonZero = true }
        if maxAbs > peak { peak = maxAbs }
        let signal = found && !signalled
        if signal { signalled = true }
        os_unfair_lock_unlock(lock)
        if signal { heard.signal() }
    }

    func snapshot() -> (Date?, Bool, Float) {
        os_unfair_lock_lock(lock)
        defer { os_unfair_lock_unlock(lock) }
        return (firstIo, nonZero, peak)
    }
}

/// A 440 Hz sine at -80 dBFS on the default output: inaudible, but non-zero
/// where the tap reads it. Lives for the duration of one probe.
private final class ToneGenerator {
    private let engine = AVAudioEngine()

    func start() {
        guard let format = AVAudioFormat(standardFormatWithSampleRate: 48_000, channels: 1) else { return }
        var phase: Float = 0
        let step = Float(2 * Double.pi * 440 / 48_000)
        let node = AVAudioSourceNode(format: format) { _, _, frameCount, abl -> OSStatus in
            let buffers = UnsafeMutableAudioBufferListPointer(abl)
            for frame in 0..<Int(frameCount) {
                let v = sinf(phase) * SystemAudioProbe.toneAmplitude
                phase += step
                if phase > 2 * .pi { phase -= 2 * .pi }
                for b in buffers {
                    b.mData?.assumingMemoryBound(to: Float.self)[frame] = v
                }
            }
            return noErr
        }
        engine.attach(node)
        engine.connect(node, to: engine.mainMixerNode, format: format)
        do {
            try engine.start()
        } catch {
            log("probe tone unavailable: \(error)")
        }
    }

    func stop() {
        engine.stop()
    }

    var isRunning: Bool { engine.isRunning }
}
