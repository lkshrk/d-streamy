import CoreVideo

public struct CropFilter {
    public let x: Int
    public let y: Int
    public let width: Int
    public let height: Int

    public init(x: Int, y: Int, width: Int, height: Int) {
        self.x = x
        self.y = y
        self.width = width
        self.height = height
    }

    /// Crop a BGRA CVPixelBuffer.
    /// Returns nil if the crop rect is out of bounds or allocation fails.
    public func apply(to source: CVPixelBuffer) -> CVPixelBuffer? {
        CVPixelBufferLockBaseAddress(source, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(source, .readOnly) }

        let srcWidth = CVPixelBufferGetWidth(source)
        let srcHeight = CVPixelBufferGetHeight(source)
        let bytesPerRow = CVPixelBufferGetBytesPerRow(source)

        // Validate crop rect
        guard x >= 0, y >= 0,
              width > 0, height > 0,
              x + width <= srcWidth,
              y + height <= srcHeight else {
            return nil
        }

        guard let srcBase = CVPixelBufferGetBaseAddress(source) else { return nil }

        let pixelFormat = CVPixelBufferGetPixelFormatType(source)
        let dstBytesPerRow = width * 4  // BGRA = 4 bytes per pixel

        var dst: CVPixelBuffer?
        let status = CVPixelBufferCreate(
            kCFAllocatorDefault,
            width,
            height,
            pixelFormat,
            nil,
            &dst
        )
        guard status == kCVReturnSuccess, let dstBuffer = dst else { return nil }

        CVPixelBufferLockBaseAddress(dstBuffer, [])
        defer { CVPixelBufferUnlockBaseAddress(dstBuffer, []) }

        guard let dstBase = CVPixelBufferGetBaseAddress(dstBuffer) else { return nil }

        // Copy row by row
        for row in 0..<height {
            let srcRow = srcBase
                .advanced(by: (y + row) * bytesPerRow + x * 4)
            let dstRow = dstBase
                .advanced(by: row * dstBytesPerRow)
            memcpy(dstRow, srcRow, dstBytesPerRow)
        }

        return dstBuffer
    }
}
