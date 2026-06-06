import Foundation

enum AnnexB {
    static let startCode: [UInt8] = [0x00, 0x00, 0x00, 0x01]

    /// Convert AVCC (4-byte big-endian length-prefixed) NALUs to Annex B, appending to `out`.
    static func appendNALUs(from buffer: UnsafeBufferPointer<UInt8>, into out: inout Data) {
        guard let base = buffer.baseAddress else { return }
        let total = buffer.count
        var offset = 0
        while offset + 4 <= total {
            let length = (UInt32(buffer[offset]) << 24)
                | (UInt32(buffer[offset + 1]) << 16)
                | (UInt32(buffer[offset + 2]) << 8)
                | UInt32(buffer[offset + 3])
            offset += 4
            let n = Int(length)
            guard n > 0, offset + n <= total else { break }
            out.append(contentsOf: startCode)
            out.append(UnsafeBufferPointer(start: base + offset, count: n))
            offset += n
        }
    }
}
