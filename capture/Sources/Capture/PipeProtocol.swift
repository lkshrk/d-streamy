import Foundation

public enum FrameType: UInt8 {
    case video = 0x01
    case audio = 0x02
}

public struct PipeWriter {
    public let fd: Int32

    public init(fd: Int32 = STDOUT_FILENO) {
        self.fd = fd
    }

    /// Write a frame to stdout.
    /// Header: [type 1B][timestamp 8B LE µs][length 4B LE][data NB]
    /// Drops the frame silently on EAGAIN (backpressure).
    public func write(type: FrameType, timestamp: UInt64, data: Data) {
        var header = Data(capacity: 13)
        header.append(type.rawValue)
        var ts = timestamp.littleEndian
        withUnsafeBytes(of: &ts) { header.append(contentsOf: $0) }
        var len = UInt32(data.count).littleEndian
        withUnsafeBytes(of: &len) { header.append(contentsOf: $0) }

        var payload = header
        payload.append(data)

        payload.withUnsafeBytes { ptr in
            guard let base = ptr.baseAddress else { return }
            var written = 0
            let total = payload.count
            while written < total {
                let n = Darwin.write(fd, base.advanced(by: written), total - written)
                if n > 0 {
                    written += n
                } else if n == -1 && errno == EAGAIN {
                    // Backpressure — drop frame
                    return
                } else if n == -1 && errno == EINTR {
                    continue
                } else {
                    // Pipe broken or other error — drop
                    return
                }
            }
        }
    }
}
