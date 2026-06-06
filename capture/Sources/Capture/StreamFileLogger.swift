import Darwin
import Foundation

public final class StreamFileLogger {
    public static let shared = StreamFileLogger()

    public let fileURL: URL
    private let lock = NSLock()
    private static let maxBytes: off_t = 10 * 1024 * 1024  // truncate past 10 MB

    public init(fileURL: URL = StreamFileLogger.defaultFileURL()) {
        self.fileURL = fileURL
    }

    public static func defaultFileURL() -> URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library", isDirectory: true)
            .appendingPathComponent("Logs", isDirectory: true)
            .appendingPathComponent("D-Streamy", isDirectory: true)
            .appendingPathComponent("stream.log")
    }

    public func write(component: String, _ message: String) {
        let cleanMessage = message
            .replacingOccurrences(of: "\r", with: "\\r")
            .replacingOccurrences(of: "\n", with: "\\n")
        let line = "\(Self.timestamp()) [\(component)] \(cleanMessage)\n"

        lock.lock()
        defer { lock.unlock() }

        do {
            try FileManager.default.createDirectory(
                at: fileURL.deletingLastPathComponent(),
                withIntermediateDirectories: true
            )
            let fd = open(fileURL.path, O_WRONLY | O_CREAT | O_APPEND, S_IRUSR | S_IWUSR | S_IRGRP | S_IROTH)
            guard fd >= 0 else { return }
            defer { close(fd) }
            var st = stat()
            if fstat(fd, &st) == 0, st.st_size > Self.maxBytes {
                ftruncate(fd, 0)
            }
            _ = line.withCString { ptr in
                Darwin.write(fd, ptr, strlen(ptr))
            }
        } catch {
            // Logging must never affect capture/streaming.
        }
    }

    private static func timestamp() -> String {
        ISO8601DateFormatter().string(from: Date())
    }
}
