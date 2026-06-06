public struct CaptureDimensions: Equatable {
    public let streamWidth: Int
    public let streamHeight: Int
    public let outputWidth: Int
    public let outputHeight: Int

    public static func resolve(
        sourceWidth: Int,
        sourceHeight: Int,
        maxWidth: Int,
        maxHeight: Int,
        crop: CropFilter?
    ) -> CaptureDimensions {
        let outputWidth: Int
        let outputHeight: Int
        if let crop {
            outputWidth = crop.width
            outputHeight = crop.height
        } else if sourceWidth > 0 && sourceHeight > 0 {
            let scaleWidth = Double(maxWidth) / Double(sourceWidth)
            let scaleHeight = Double(maxHeight) / Double(sourceHeight)
            let scale = min(scaleWidth, scaleHeight, 1.0)
            outputWidth = max(2, Int(Double(sourceWidth) * scale) & ~1)
            outputHeight = max(2, Int(Double(sourceHeight) * scale) & ~1)
        } else {
            outputWidth = maxWidth
            outputHeight = maxHeight
        }

        let streamWidth: Int
        let streamHeight: Int
        if crop != nil, sourceWidth > 0 && sourceHeight > 0 {
            streamWidth = sourceWidth
            streamHeight = sourceHeight
        } else {
            streamWidth = outputWidth
            streamHeight = outputHeight
        }

        return CaptureDimensions(
            streamWidth: streamWidth,
            streamHeight: streamHeight,
            outputWidth: outputWidth,
            outputHeight: outputHeight
        )
    }
}
