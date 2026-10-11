import FluidAudio
import Foundation

/// The only CoreML models this sidecar will ever load or download.
///
/// This is the sidecar half of the model policy: the Rust manifest decides what
/// is allowed; this enum is the closed set of repos the Swift side knows how to
/// fetch. Anything else is rejected with `unknownModel`.
enum ModelId: String, CaseIterable, Codable {
    /// NVIDIA Parakeet TDT 0.6B v3 (CC-BY-4.0), CoreML port by FluidInference.
    case parakeetV3 = "parakeet-tdt-0.6b-v3"
    /// pyannote speaker-diarization-community-1 (CC-BY-4.0), CoreML port by FluidInference.
    /// Only the Community-1 artifact set is fetched (variant "offline"); the legacy
    /// wespeaker bundles are never downloaded.
    case pyannoteCommunity1 = "pyannote-community-1"

    var repo: Repo {
        switch self {
        case .parakeetV3: return .parakeetV3
        case .pyannoteCommunity1: return .diarizer
        }
    }

    /// FluidAudio download variant selecting the required file subset.
    var variant: String? {
        switch self {
        case .parakeetV3: return ParakeetEncoderPrecision.int8.rawValue
        case .pyannoteCommunity1: return "offline"
        }
    }

    /// Files that must exist for the model to count as installed.
    var requiredFiles: [String] {
        switch self {
        case .parakeetV3:
            return Array(ModelNames.ASR.requiredModelsV3(precision: .int8)) + [ModelNames.ASR.vocabularyFile]
        case .pyannoteCommunity1:
            return Array(ModelNames.OfflineDiarizer.requiredModels)
        }
    }
}

struct ModelStatus: Codable {
    let id: String
    let installed: Bool
    let path: String
    let missing: [String]
    let bytes: Int64
}

struct ModelStore {
    /// Parent directory that FluidAudio appends `repo.folderName` to, e.g.
    /// `~/Library/Application Support/Muesli/models/FluidInference`.
    let root: URL

    init(root: URL) {
        self.root = root
    }

    static func defaultRoot() -> URL {
        if let env = ProcessInfo.processInfo.environment["MUESLI_MODELS_ROOT"], !env.isEmpty {
            return URL(fileURLWithPath: env, isDirectory: true)
        }
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        return base.appendingPathComponent("Muesli/models/FluidInference", isDirectory: true)
    }

    func repoDirectory(_ id: ModelId) -> URL {
        root.appendingPathComponent(id.repo.folderName, isDirectory: true)
    }

    /// Mirrors FluidAudio's own load criteria so `installed == true` means a load
    /// in offline mode will succeed: every required file present, compiled bundles
    /// contain `coremldata.bin`, no `.partial` downloads left, and the revision
    /// marker matches the pinned revision for pinned repos.
    func status(_ id: ModelId) -> ModelStatus {
        let dir = repoDirectory(id)
        let fm = FileManager.default
        var missing: [String] = []
        for file in id.requiredFiles {
            let url = dir.appendingPathComponent(file)
            if !fm.fileExists(atPath: url.path) {
                missing.append(file)
            } else if file.hasSuffix(".mlmodelc"), !fm.fileExists(atPath: url.appendingPathComponent("coremldata.bin").path) {
                missing.append(file + " (incomplete)")
            }
        }
        if let contents = try? fm.contentsOfDirectory(atPath: dir.path), contents.contains(where: { $0.hasSuffix(".partial") }) {
            missing.append("(partial download present)")
        }
        let pinned = id.repo.revision
        if pinned != "main" {
            let marker = dir.appendingPathComponent(".fluidaudio-revision")
            let stored = (try? String(contentsOf: marker, encoding: .utf8))?.trimmingCharacters(in: .whitespacesAndNewlines)
            if stored != pinned { missing.append("(revision marker \(stored ?? "absent") != \(pinned))") }
        }
        return ModelStatus(
            id: id.rawValue, installed: missing.isEmpty, path: dir.path, missing: missing,
            bytes: missing.isEmpty ? directorySize(dir) : 0)
    }

    /// Download (or verify) a model into `root`. Progress is reported in [0, 1].
    /// The caller is responsible for `ModelHub.offlineMode` (see ProcessRole).
    func download(_ id: ModelId, progress: @escaping @Sendable (Double, String) -> Void) async throws {
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        try await ModelHub.download(
            id.repo, to: root, variant: id.variant,
            progressHandler: { p in
                let phase: String
                switch p.phase {
                case .listing: phase = "listing"
                case .downloading(let done, let total): phase = "downloading \(done)/\(total)"
                case .compiling(let name): phase = "compiling \(name)"
                }
                progress(p.fractionCompleted, phase)
            })
        let st = status(id)
        guard st.installed else {
            throw RpcError.server("download finished but files are missing: \(st.missing)")
        }
    }

    private func directorySize(_ url: URL) -> Int64 {
        guard let e = FileManager.default.enumerator(at: url, includingPropertiesForKeys: [.fileSizeKey]) else { return 0 }
        var total: Int64 = 0
        for case let f as URL in e {
            if let s = try? f.resourceValues(forKeys: [.fileSizeKey]).fileSize { total += Int64(s) }
        }
        return total
    }
}
