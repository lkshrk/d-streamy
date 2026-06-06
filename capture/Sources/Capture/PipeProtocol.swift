import Foundation

public enum FrameType: UInt8 {
    case video = 0x01
    case audio = 0x02
}

public final class PipeWriter {
    public let fd: Int32
    private let lock = NSLock()

    public init(fd: Int32 = STDOUT_FILENO) {
        self.fd = fd
    }

    /// Write a frame to stdout.
    /// Header: [type 1B][timestamp 8B LE µs][length 4B LE][data NB]
    /// Drops the frame silently on EAGAIN (backpressure).
    public func write(type: FrameType, timestamp: UInt64, data: Data) {
        lock.lock()
        defer { lock.unlock() }

        var header = Data(capacity: 13)
        header.append(type.rawValue)
        var ts = timestamp.littleEndian
        withUnsafeBytes(of: &ts) { header.append(contentsOf: $0) }
        var len = UInt32(data.count).littleEndian
        withUnsafeBytes(of: &len) { header.append(contentsOf: $0) }

        guard writeAll(header) else { return }
        _ = writeAll(data)
    }

    private func writeAll(_ data: Data) -> Bool {
        data.withUnsafeBytes { ptr in
            guard let base = ptr.baseAddress else { return true }
            var written = 0
            let total = data.count
            while written < total {
                let n = Darwin.write(fd, base.advanced(by: written), total - written)
                if n > 0 {
                    written += n
                } else if n == -1 && errno == EAGAIN {
                    return false
                } else if n == -1 && errno == EINTR {
                    continue
                } else {
                    return false
                }
            }
            return true
        }
    }
}
