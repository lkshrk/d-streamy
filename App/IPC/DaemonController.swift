import Foundation
import os

private let log = Logger(subsystem: "me.harke.d-streamy", category: "daemon")

/// Manages the Node daemon process and Unix socket IPC.
/// Daemon is the socket server, Swift connects as client.
final class DaemonController: ObservableObject {
    private var process: Process?
    private var stdinPipe: Pipe?   // media data → Node stdin
    private var socketPath: String?
    private var socketHandle: FileHandle?
    private var clientFd: Int32 = -1
    private var readBuffer = Data()

    @Published var isConnected = false

    var onEvent: ((String, Data?) -> Void)?

    /// File descriptor for writing media data to the daemon's stdin.
    var mediaFd: Int32? {
        stdinPipe?.fileHandleForWriting.fileDescriptor
    }

    func start() throws {
        // Kill any orphaned daemon from previous run
        if let oldProc = process, oldProc.isRunning {
            oldProc.terminate()
        }

        let sockPath = NSTemporaryDirectory() + "dstreamy-\(ProcessInfo.processInfo.processIdentifier).sock"
        self.socketPath = sockPath

        // Clean up stale socket
        unlink(sockPath)

        // Spawn daemon (it creates the socket server)
        let proc = Process()
        proc.environment = ProcessInfo.processInfo.environment
        proc.environment?["DSTREAMY_SOCKET"] = sockPath

        let bunPath = findBun()
        log.info("bun path: \(bunPath, privacy: .public)")

        if let bundledDir = Bundle.main.resourceURL?.appendingPathComponent("daemon"),
           FileManager.default.fileExists(atPath: bundledDir.appendingPathComponent("index.js").path) {
            // Release: run bundled JS with bun
            proc.executableURL = URL(fileURLWithPath: bunPath)
            proc.arguments = ["run", bundledDir.appendingPathComponent("index.js").path]
            proc.currentDirectoryURL = bundledDir
            log.info("daemon mode: release, dir=\(bundledDir.path, privacy: .public)")
        } else {
            // Dev: run source directly
            let projectRoot = resolveProjectRoot()
            proc.executableURL = URL(fileURLWithPath: bunPath)
            proc.arguments = ["run", projectRoot + "/src/daemon/index.ts"]
            proc.currentDirectoryURL = URL(fileURLWithPath: projectRoot)
            log.info("daemon mode: dev, root=\(projectRoot, privacy: .public)")
        }

        let stdin = Pipe()
        let stderr = Pipe()
        proc.standardInput = stdin
        proc.standardError = stderr

        do {
            try proc.run()
            log.info("daemon spawned, pid=\(proc.processIdentifier)")
        } catch {
            log.error("daemon spawn failed: \(error.localizedDescription, privacy: .public)")
            throw error
        }
        self.process = proc
        self.stdinPipe = stdin

        // Pipe daemon stderr → os_log (info level so it persists in log show)
        stderr.fileHandleForReading.readabilityHandler = { handle in
            let data = handle.availableData
            guard !data.isEmpty,
                  let line = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines),
                  !line.isEmpty else { return }
            log.info("[stderr] \(line, privacy: .public)")
        }

        // Connect to daemon's socket (retry until it's listening)
        DispatchQueue.global().async { [weak self] in
            self?.connectToDaemon(sockPath: sockPath)
        }
    }

    func send(_ command: IPCCommand) {
        guard let handle = socketHandle else { return }
        guard let data = try? JSONEncoder().encode(command) else { return }
        var msg = data
        msg.append(0x0A) // newline delimiter
        handle.write(msg)
    }

    func stop() {
        process?.terminate()
        process = nil
        if clientFd >= 0 { close(clientFd); clientFd = -1 }
        socketHandle?.closeFile()
        socketHandle = nil
        if let path = socketPath { unlink(path) }
        isConnected = false
    }

    // MARK: - Private

    private func connectToDaemon(sockPath: String) {
        // Retry connecting up to 50 times (5 seconds total)
        for _ in 0..<50 {
            let fd = socket(AF_UNIX, SOCK_STREAM, 0)
            guard fd >= 0 else { continue }

            var addr = sockaddr_un()
            addr.sun_family = sa_family_t(AF_UNIX)
            let pathBytes = sockPath.utf8CString
            withUnsafeMutableBytes(of: &addr.sun_path) { buf in
                for (i, byte) in pathBytes.prefix(buf.count - 1).enumerated() {
                    buf[i] = UInt8(bitPattern: byte)
                }
            }

            let addrLen = socklen_t(MemoryLayout<sockaddr_un>.size)
            let result = withUnsafePointer(to: &addr) { ptr in
                ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sockPtr in
                    connect(fd, sockPtr, addrLen)
                }
            }

            if result == 0 {
                // Connected
                self.clientFd = fd
                let handle = FileHandle(fileDescriptor: fd, closeOnDealloc: true)
                DispatchQueue.main.async { [weak self] in
                    self?.socketHandle = handle
                    self?.isConnected = true
                }
                // Read loop
                handle.readabilityHandler = { [weak self] fh in
                    let data = fh.availableData
                    guard !data.isEmpty else {
                        DispatchQueue.main.async { self?.isConnected = false }
                        return
                    }
                    self?.handleIncoming(data)
                }
                return
            } else {
                close(fd)
                Thread.sleep(forTimeInterval: 0.1)
            }
        }
        log.error("failed to connect to daemon socket")
    }

    private func handleIncoming(_ data: Data) {
        readBuffer.append(data)

        while let newlineIndex = readBuffer.firstIndex(of: 0x0A) {
            let line = readBuffer[readBuffer.startIndex..<newlineIndex]
            readBuffer = readBuffer[(newlineIndex + 1)...]

            guard !line.isEmpty else { continue }
            DispatchQueue.main.async { [weak self] in
                self?.parseEvent(Data(line))
            }
        }
    }

    private func parseEvent(_ data: Data) {
        guard let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let type = json["type"] as? String else { return }

        var payload: Data?
        if let obj = json["payload"] {
            // NSJSONSerialization requires top-level array/dict; wrap bare values
            if obj is [Any] || obj is [String: Any] {
                payload = try? JSONSerialization.data(withJSONObject: obj)
            } else if let str = obj as? String {
                payload = str.data(using: .utf8)
            }
        }
        onEvent?(type, payload)
    }

    private func findBun() -> String {
        let home = ProcessInfo.processInfo.environment["HOME"] ?? NSHomeDirectory()
        let candidates = [
            home + "/.bun/bin/bun",
            "/usr/local/bin/bun",
            "/opt/homebrew/bin/bun",
        ]
        for path in candidates {
            if FileManager.default.isExecutableFile(atPath: path) { return path }
        }
        log.error("bun not found in: \(candidates.joined(separator: ", "), privacy: .public)")
        return candidates[0] // will fail with clear error at proc.run()
    }

    private func resolveProjectRoot() -> String {
        if let root = ProcessInfo.processInfo.environment["DSTREAMY_ROOT"] { return root }
        var dir = URL(fileURLWithPath: Bundle.main.executablePath ?? "")
        for _ in 0..<10 {
            dir = dir.deletingLastPathComponent()
            let pkg = dir.appendingPathComponent("package.json")
            if FileManager.default.fileExists(atPath: pkg.path) { return dir.path }
        }
        return FileManager.default.currentDirectoryPath
    }

    enum DaemonError: Error {
        case socketFailed
        case spawnFailed
    }
}
