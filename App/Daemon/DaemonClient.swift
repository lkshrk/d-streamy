import Foundation
import os

private let log = Logger(subsystem: "me.harke.d-streamy", category: "daemon")

struct DaemonError: LocalizedError, Equatable {
    let code: String
    let message: String

    var errorDescription: String? { message }
}

enum DaemonClientError: LocalizedError, Equatable {
    case notRunning
    case exited(Int32)
    case timedOut(String)
    case incompatibleProtocol(Int)

    var errorDescription: String? {
        switch self {
        case .notRunning: return "The dstreamy daemon is not running."
        case .exited(let status): return "The dstreamy daemon exited (status \(status))."
        case .timedOut(let cmd): return "The dstreamy daemon did not answer \(cmd) in time."
        case .incompatibleProtocol(let version): return "The dstreamy daemon speaks protocol \(version), expected \(daemonProtocolVersion)."
        }
    }
}

/// Runs `dstreamy daemon` and speaks protocol v1 (ADR 0003) over its stdin and stdout.
final class DaemonClient: @unchecked Sendable {
    let executableURL: URL
    let arguments: [String]
    let environment: [String: String]?

    var onEvent: (@Sendable (SessionEvent) -> Void)?
    var onExit: (@Sendable (Int32) -> Void)?

    private let lock = NSLock()
    private var process: Process?
    private var input: FileHandle?
    private var buffer = Data()
    private var pending: [String: CheckedContinuation<JSONValue, Error>] = [:]
    private var nextId = 0
    private var exitStatus: Int32?
    private var outputClosed = false

    init(executableURL: URL, arguments: [String] = ["daemon"], environment: [String: String]? = nil) {
        self.executableURL = executableURL
        self.arguments = arguments
        self.environment = environment
    }

    deinit {
        process?.terminate()
        for continuation in pending.values {
            continuation.resume(throwing: DaemonClientError.notRunning)
        }
    }

    var isRunning: Bool {
        lock.withLock { process?.isRunning ?? false }
    }

    func launch() throws {
        signal(SIGPIPE, SIG_IGN)
        let proc = Process()
        proc.executableURL = executableURL
        proc.arguments = arguments
        if let environment { proc.environment = environment }

        let stdin = Pipe()
        let stdout = Pipe()
        let stderr = Pipe()
        proc.standardInput = stdin
        proc.standardOutput = stdout
        proc.standardError = stderr

        stdout.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            if data.isEmpty {
                handle.readabilityHandler = nil
                self?.finish { $0.outputClosed = true }
            } else {
                self?.receive(data)
            }
        }
        stderr.fileHandleForReading.readabilityHandler = { handle in
            let data = handle.availableData
            guard !data.isEmpty else {
                handle.readabilityHandler = nil
                return
            }
            for line in String(decoding: data, as: UTF8.self).split(whereSeparator: \.isNewline) {
                log.info("[stderr] \(String(line), privacy: .public)")
            }
        }
        proc.terminationHandler = { [weak self] p in
            let status = p.terminationStatus
            self?.finish { $0.exitStatus = status }
        }

        lock.withLock {
            process = proc
            input = stdin.fileHandleForWriting
            buffer = Data()
            exitStatus = nil
            outputClosed = false
        }
        do {
            try proc.run()
        } catch {
            lock.withLock {
                process = nil
                input = nil
            }
            throw error
        }
        log.info("daemon spawned pid=\(proc.processIdentifier) path=\(self.executableURL.path, privacy: .public)")
    }

    @discardableResult
    func handshake() async throws -> HelloInfo {
        let hello: HelloInfo = try await request(.hello, timeout: .seconds(10))
        guard hello.protocol == daemonProtocolVersion else {
            throw DaemonClientError.incompatibleProtocol(hello.protocol)
        }
        return hello
    }

    @discardableResult
    func request<T: Decodable>(_ command: DaemonCommand, as type: T.Type = T.self, timeout: Duration? = nil) async throws -> T {
        try await send(command, timeout: timeout).decode(as: T.self)
    }

    func send(_ command: DaemonCommand, timeout: Duration? = nil) async throws -> JSONValue {
        let id: String = lock.withLock {
            nextId += 1
            return String(nextId)
        }
        var line = try DaemonCoding.encoder.encode(DaemonRequest(id: id, command: command))
        line.append(0x0A)

        if let timeout {
            Task { [weak self] in
                try? await Task.sleep(for: timeout)
                self?.resolve(id, with: .failure(DaemonClientError.timedOut(command.name)))
            }
        }

        return try await withCheckedThrowingContinuation { continuation in
            let handle: FileHandle? = lock.withLock {
                guard process?.isRunning == true, let input else { return nil }
                pending[id] = continuation
                return input
            }
            guard let handle else {
                continuation.resume(throwing: DaemonClientError.notRunning)
                return
            }
            do {
                try handle.write(contentsOf: line)
            } catch {
                resolve(id, with: .failure(error))
            }
        }
    }

    /// Asks the daemon to exit, closing stdin and then terminating it if it lingers.
    func shutdown(grace: Duration = .seconds(3)) async {
        guard isRunning else { return }
        _ = try? await send(.shutdown, timeout: grace)
        let handle: FileHandle? = lock.withLock {
            defer { input = nil }
            return input
        }
        try? handle?.close()
        let deadline = ContinuousClock.now + grace
        while isRunning, ContinuousClock.now < deadline {
            try? await Task.sleep(for: .milliseconds(50))
        }
        terminate()
    }

    func terminate() {
        let proc: Process? = lock.withLock { process }
        if proc?.isRunning == true { proc?.terminate() }
    }

    private func receive(_ data: Data) {
        let lines: [Data] = lock.withLock {
            buffer.append(data)
            var lines: [Data] = []
            while let newline = buffer.firstIndex(of: 0x0A) {
                lines.append(buffer[buffer.startIndex..<newline])
                buffer = Data(buffer[(newline + 1)...])
            }
            return lines
        }
        for line in lines where !line.isEmpty {
            handle(Data(line))
        }
    }

    private func handle(_ line: Data) {
        let message: DaemonMessage
        do {
            message = try DaemonCoding.decoder.decode(DaemonMessage.self, from: line)
        } catch {
            log.error("undecodable daemon line: \(String(decoding: line, as: UTF8.self), privacy: .public) \(String(describing: error), privacy: .public)")
            return
        }
        switch message {
        case .result(let id, let data):
            if let id { resolve(id, with: .success(data)) }
        case .error(let id, let code, let message):
            if let id {
                resolve(id, with: .failure(DaemonError(code: code, message: message)))
            } else {
                log.error("daemon error \(code, privacy: .public): \(message, privacy: .public)")
            }
        case .event(let event):
            onEvent?(event)
        }
    }

    private func resolve(_ id: String, with result: Result<JSONValue, Error>) {
        let continuation = lock.withLock { pending.removeValue(forKey: id) }
        continuation?.resume(with: result)
    }

    /// Pending requests fail and `onExit` fires only once the process has exited and stdout reached EOF, so no late reply is lost.
    private func finish(_ update: (DaemonClient) -> Void) {
        let result: (Int32, [CheckedContinuation<JSONValue, Error>])? = lock.withLock {
            update(self)
            guard let status = exitStatus, outputClosed, process != nil else { return nil }
            process = nil
            input = nil
            defer { pending = [:] }
            return (status, Array(pending.values))
        }
        guard let (status, waiting) = result else { return }
        log.info("daemon exited status=\(status)")
        for continuation in waiting {
            continuation.resume(throwing: DaemonClientError.exited(status))
        }
        onExit?(status)
    }
}
