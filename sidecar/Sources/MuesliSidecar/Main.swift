import FluidAudio
import Foundation

let sidecarVersion = "0.1.0"

@main
struct Main {
    static func main() async {
        // A dead parent must not kill us mid-write: writes then fail with EPIPE,
        // which RpcOutput turns into a graceful shutdown.
        signal(SIGPIPE, SIG_IGN)

        let args = Array(CommandLine.arguments.dropFirst())
        let store = ModelStore(root: ModelStore.defaultRoot())
        switch args.first {
        case "process":
            let server = RpcServer()
            ProcessRole.register(on: server, store: store)
            await server.output.setOnWriteFailure { exit(0) }
            await server.run()
        case "capture":
            ModelHub.offlineMode = true
            let server = RpcServer()
            CaptureRole.register(on: server, store: store)
            await server.output.setOnWriteFailure {
                Task {
                    await CaptureRole.shutdown()
                    exit(0)
                }
            }
            await server.run()
            // stdin closed: the app quit or crashed. Finalise before exiting.
            await CaptureRole.shutdown()
        case "spike":
            await Spikes.run(Array(args.dropFirst()), store: store)
        case "version":
            print("muesli-sidecar \(sidecarVersion)")
        default:
            FileHandle.standardError.write(
                """
                usage: muesli-sidecar <process|capture|spike|version>
                  process            JSON-RPC worker: models.*, decode, asr.run, diarize.run
                  capture            JSON-RPC recorder: capture.start/stop/status/recover, permissions.*
                  spike models       download Parakeet v3 + pyannote community-1
                  spike decode <in> <out.wav>
                  spike asr <wav16k>
                  spike diarize <wav16k>
                  spike capture <seconds> <outDir>
                  spike live <seconds> <outDir>       capture with a live transcript printed as it arrives
                  spike recover <outDir>
                env: MUESLI_MODELS_ROOT (default ~/Library/Application Support/Muesli/models/FluidInference)

                """.data(using: .utf8)!)
            exit(2)
        }
    }
}
