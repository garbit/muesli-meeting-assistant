import AVFoundation
import Foundation

/// Per-100 ms RMS energy for the two retained channels (L = mic, R = system).
struct EnergyTimeline: Codable {
    let frameMs: Int
    let mic: [Float]
    let system: [Float]
}

struct DecodedAudio: Codable {
    let mixPath: String
    let durationMs: Int
    let sampleRate: Int
    let sourceChannels: Int
    let energy: EnergyTimeline?
}

/// Decodes any AVFoundation-readable audio or video file to a 16 kHz mono
/// 16-bit WAV (the canonical input for ASR and diarisation) and, for stereo
/// recordings made by the capture role, an energy timeline used by Rust to
/// label the local user's speaker cluster.
enum AudioDecoder {
    static let targetRate = 16_000

    static func decode(input: URL, outputWav: URL, micSystemStereo: Bool) async throws -> DecodedAudio {
        let asset = AVURLAsset(url: input)
        let tracks = try await asset.loadTracks(withMediaType: .audio)
        guard let track = tracks.first else { throw RpcError.server("no audio track in \(input.lastPathComponent)") }
        let reader = try AVAssetReader(asset: asset)
        let settings: [String: Any] = [
            AVFormatIDKey: kAudioFormatLinearPCM,
            AVSampleRateKey: targetRate,
            AVLinearPCMBitDepthKey: 32,
            AVLinearPCMIsFloatKey: true,
            AVLinearPCMIsBigEndianKey: false,
            AVLinearPCMIsNonInterleaved: false,
        ]
        let output = AVAssetReaderTrackOutput(track: track, outputSettings: settings)
        output.alwaysCopiesSampleData = false
        reader.add(output)
        guard reader.startReading() else { throw RpcError.server("AVAssetReader failed: \(String(describing: reader.error))") }

        let fileFormat: [String: Any] = [
            AVFormatIDKey: kAudioFormatLinearPCM,
            AVSampleRateKey: targetRate,
            AVNumberOfChannelsKey: 1,
            AVLinearPCMBitDepthKey: 16,
            AVLinearPCMIsFloatKey: false,
            AVLinearPCMIsBigEndianKey: false,
        ]
        try? FileManager.default.removeItem(at: outputWav)
        let file = try AVAudioFile(forWriting: outputWav, settings: fileFormat, commonFormat: .pcmFormatFloat32, interleaved: false)
        let monoFormat = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: Double(targetRate), channels: 1, interleaved: false)!

        var channels = 0
        var totalFrames = 0
        let frameLen = targetRate / 10  // 100 ms
        var micE: [Float] = []
        var sysE: [Float] = []
        var accMic: Float = 0, accSys: Float = 0, accN = 0

        while let sb = output.copyNextSampleBuffer() {
            guard let asbd = CMSampleBufferGetFormatDescription(sb).flatMap({ CMAudioFormatDescriptionGetStreamBasicDescription($0)?.pointee }) else {
                throw RpcError.server("sample buffer without a format description")
            }
            channels = Int(asbd.mChannelsPerFrame)
            guard channels > 0 else { throw RpcError.server("audio track reports 0 channels") }
            let frames = CMSampleBufferGetNumSamples(sb)
            guard frames > 0 else { continue }
            // Output is forced interleaved, so the list has exactly one buffer.
            var abl = AudioBufferList()
            var block: CMBlockBuffer?
            let st = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
                sb, bufferListSizeNeededOut: nil, bufferListOut: &abl, bufferListSize: MemoryLayout<AudioBufferList>.size,
                blockBufferAllocator: nil, blockBufferMemoryAllocator: nil, flags: 0, blockBufferOut: &block)
            guard st == noErr, abl.mNumberBuffers == 1, let data = abl.mBuffers.mData else {
                throw RpcError.server("could not read decoded audio (status \(st), buffers \(abl.mNumberBuffers))")
            }
            let samples = data.bindMemory(to: Float.self, capacity: frames * channels)

            guard let buf = AVAudioPCMBuffer(pcmFormat: monoFormat, frameCapacity: AVAudioFrameCount(frames)) else { continue }
            let out = buf.floatChannelData![0]
            for f in 0..<frames {
                var sum: Float = 0
                for c in 0..<channels { sum += samples[f * channels + c] }
                out[f] = sum / Float(channels)
                if micSystemStereo && channels == 2 {
                    let m = samples[f * 2], s = samples[f * 2 + 1]
                    accMic += m * m
                    accSys += s * s
                    accN += 1
                    if accN == frameLen {
                        micE.append((accMic / Float(accN)).squareRoot())
                        sysE.append((accSys / Float(accN)).squareRoot())
                        accMic = 0; accSys = 0; accN = 0
                    }
                }
            }
            buf.frameLength = AVAudioFrameCount(frames)
            try file.write(from: buf)
            totalFrames += frames
            if Task.isCancelled { throw CancellationError() }
        }
        if reader.status == .failed { throw RpcError.server("decode failed: \(String(describing: reader.error))") }
        if accN > 0 {
            micE.append((accMic / Float(accN)).squareRoot())
            sysE.append((accSys / Float(accN)).squareRoot())
        }
        let energy = (micSystemStereo && channels == 2) ? EnergyTimeline(frameMs: 100, mic: micE, system: sysE) : nil
        return DecodedAudio(
            mixPath: outputWav.path, durationMs: totalFrames * 1000 / targetRate, sampleRate: targetRate,
            sourceChannels: channels, energy: energy)
    }
}
