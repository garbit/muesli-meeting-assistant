import Foundation

/// Newline-delimited JSON-RPC 2.0 over stdin/stdout.
///
/// Requests: {"jsonrpc":"2.0","id":1,"method":"asr.run","params":{...}}
/// Responses: {"jsonrpc":"2.0","id":1,"result":{...}} | {"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"..."}}
/// Notifications (server → client): {"jsonrpc":"2.0","method":"job.progress","params":{...}}
///
/// All diagnostic logging goes to stderr; stdout carries protocol lines only.
struct RpcError: Error {
    let code: Int
    let message: String
    let data: JSONValue?
    init(_ code: Int, _ message: String, data: JSONValue? = nil) {
        self.code = code
        self.message = message
        self.data = data
    }
    static func invalidParams(_ m: String) -> RpcError { RpcError(-32602, m) }
    static func server(_ m: String, data: JSONValue? = nil) -> RpcError { RpcError(-32000, m, data: data) }
    /// Another live recorder owns the resource; the caller may retry later.
    static func busy(_ m: String) -> RpcError { RpcError(-32002, m) }
}

/// Serialises writes to stdout so concurrent handlers never interleave lines.
/// A failed write means the parent is gone; `onWriteFailure` lets the role
/// finalise (e.g. stop a recording) before the process exits.
actor RpcOutput {
    private let handle = FileHandle.standardOutput
    private var failed = false
    private let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.outputFormatting = [.withoutEscapingSlashes]
        return e
    }()
    private var onWriteFailure: (@Sendable () -> Void)?

    func setOnWriteFailure(_ handler: @escaping @Sendable () -> Void) {
        onWriteFailure = handler
    }

    func send(_ message: [String: JSONValue]) {
        guard !failed else { return }
        var obj = message
        obj["jsonrpc"] = "2.0"
        let data: Data
        do {
            data = try encoder.encode(JSONValue.object(obj))
        } catch {
            // Never swallow a response: the client would wait forever.
            if let id = obj["id"], id != .null {
                let fallback = "{\"jsonrpc\":\"2.0\",\"id\":\(idLiteral(id)),\"error\":{\"code\":-32603,\"message\":\"result not encodable: \(String(describing: error).replacingOccurrences(of: "\"", with: "'"))\"}}\n"
                write(Data(fallback.utf8))
            }
            return
        }
        var line = data
        line.append(0x0A)
        write(line)
    }

    private func idLiteral(_ id: JSONValue) -> String {
        switch id {
        case .number(let n): return n.rounded() == n ? String(Int64(n)) : String(n)
        case .string(let s): return "\"\(s)\""
        default: return "null"
        }
    }

    private func write(_ data: Data) {
        do {
            try handle.write(contentsOf: data)
        } catch {
            failed = true
            log("stdout write failed (parent gone?): \(error)")
            onWriteFailure?()
        }
    }

    func respond(id: JSONValue, result: JSONValue) {
        send(["id": id, "result": result])
    }

    func fail(id: JSONValue, error: RpcError) {
        var e: [String: JSONValue] = ["code": .number(Double(error.code)), "message": .string(error.message)]
        if let d = error.data { e["data"] = d }
        send(["id": id, "error": .object(e)])
    }

    func notify(_ method: String, _ params: JSONValue) {
        send(["method": .string(method), "params": params])
    }
}

/// Tracks running jobs so `job.cancel` can cancel them. Lock-based so a job is
/// reserved *before* its task starts and a fast finish cannot race registration.
final class JobRegistry: @unchecked Sendable {
    private let lock = NSLock()
    private var tasks: [String: Task<Void, Never>?] = [:]
    private var cancelledBeforeStart: Set<String> = []

    /// Returns false if the id is already running.
    func reserve(_ id: String) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if tasks[id] != nil { return false }
        tasks[id] = .some(nil)
        return true
    }

    func attach(_ id: String, _ task: Task<Void, Never>) {
        lock.lock()
        defer { lock.unlock() }
        guard tasks[id] != nil else { return }  // already finished
        tasks[id] = task
        if cancelledBeforeStart.remove(id) != nil { task.cancel() }
    }

    func finish(_ id: String) {
        lock.lock()
        defer { lock.unlock() }
        tasks[id] = nil
        cancelledBeforeStart.remove(id)
    }

    func cancel(_ id: String) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard let entry = tasks[id] else { return false }
        if let t = entry { t.cancel() } else { cancelledBeforeStart.insert(id) }
        return true
    }
}

final class RpcServer {
    typealias Handler = @Sendable (JSONValue, RpcServer) async throws -> JSONValue

    let output = RpcOutput()
    let jobs = JobRegistry()
    private var handlers: [String: Handler] = [:]

    func register(_ method: String, _ handler: @escaping Handler) {
        handlers[method] = handler
    }

    /// Emit a `job.progress` notification.
    func progress(jobId: String, phase: String, fraction: Double, extra: [String: JSONValue] = [:]) async {
        var p: [String: JSONValue] = [
            "jobId": .string(jobId), "phase": .string(phase), "fraction": .number(min(max(fraction, 0), 1)),
        ]
        for (k, v) in extra { p[k] = v }
        await output.notify("job.progress", .object(p))
    }

    /// Read stdin until EOF, dispatching each line as a request. Returns when the
    /// parent closes the pipe.
    func run() async {
        log("rpc server ready (pid \(ProcessInfo.processInfo.processIdentifier))")
        do {
            for try await line in FileHandle.standardInput.bytes.lines {
                guard !line.isEmpty else { continue }
                handle(line: line)
            }
        } catch {
            log("stdin read error: \(error)")
        }
        log("stdin closed")
    }

    private func handle(line: String) {
        guard let data = line.data(using: .utf8),
            let req = try? JSONDecoder().decode(JSONValue.self, from: data),
            let method = req["method"]?.string
        else {
            Task { await output.fail(id: .null, error: RpcError(-32700, "parse error")) }
            return
        }
        let id = req["id"] ?? .null
        let params = req["params"] ?? .object([:])
        guard let handler = handlers[method] else {
            Task { await output.fail(id: id, error: RpcError(-32601, "method not found: \(method)")) }
            return
        }
        let jobId = params["jobId"]?.string
        if let jobId, !jobs.reserve(jobId) {
            Task { await output.fail(id: id, error: RpcError.invalidParams("jobId \(jobId) is already running")) }
            return
        }
        let task = Task { [output, jobs] in
            defer { if let jobId { jobs.finish(jobId) } }
            do {
                let result = try await handler(params, self)
                if id != .null { await output.respond(id: id, result: result) }
            } catch let e as RpcError {
                if id != .null { await output.fail(id: id, error: e) }
            } catch {
                if id != .null {
                    if error is CancellationError || Task.isCancelled || String(describing: error).localizedCaseInsensitiveContains("cancel") {
                        await output.fail(id: id, error: RpcError(-32001, "cancelled"))
                    } else {
                        await output.fail(id: id, error: RpcError.server(String(describing: error)))
                    }
                }
            }
        }
        if let jobId { jobs.attach(jobId, task) }
    }
}

private let logFormatter: ISO8601DateFormatter = {
    let f = ISO8601DateFormatter()
    f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
    return f
}()

/// stderr logger with timestamps (stdout is reserved for the protocol).
func log(_ message: String) {
    let line = "[\(logFormatter.string(from: Date()))] \(message)\n"
    try? FileHandle.standardError.write(contentsOf: Data(line.utf8))
}
