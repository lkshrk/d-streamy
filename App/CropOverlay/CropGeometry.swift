import CoreGraphics

/// Converts a crop drawn in overlay points (bottom-left origin) to source pixels (top-left origin).
func sourcePixelCrop(viewRect: CGRect, viewSize: CGSize, sourceWidth: Int, sourceHeight: Int) -> CropRect? {
    guard viewSize.width > 0, viewSize.height > 0, sourceWidth > 0, sourceHeight > 0 else { return nil }
    let scaleX = CGFloat(sourceWidth) / viewSize.width
    let scaleY = CGFloat(sourceHeight) / viewSize.height
    let bounds = CGRect(origin: .zero, size: viewSize)
    let rect = viewRect.standardized.intersection(bounds)
    guard !rect.isNull, rect.width > 0, rect.height > 0 else { return nil }

    let x = min(max(0, Int((rect.minX * scaleX).rounded())), sourceWidth - 1)
    let y = min(max(0, Int(((viewSize.height - rect.maxY) * scaleY).rounded())), sourceHeight - 1)
    let width = min(Int((rect.width * scaleX).rounded()), sourceWidth - x)
    let height = min(Int((rect.height * scaleY).rounded()), sourceHeight - y)
    guard width > 0, height > 0 else { return nil }
    return CropRect(x: x, y: y, width: width, height: height)
}

/// The on-screen frame of a window in AppKit coordinates (bottom-left origin of the primary screen).
func windowFrame(for windowID: CGWindowID, primaryScreenHeight: CGFloat) -> CGRect? {
    guard let info = windowInfo(windowID),
          let bounds = info[kCGWindowBounds as String] as? [String: CGFloat],
          let x = bounds["X"], let y = bounds["Y"],
          let w = bounds["Width"], let h = bounds["Height"] else { return nil }
    return CGRect(x: x, y: primaryScreenHeight - y - h, width: w, height: h)
}

func windowInfo(_ windowID: CGWindowID) -> [String: Any]? {
    (CGWindowListCopyWindowInfo([.optionIncludingWindow], windowID) as? [[String: Any]])?.first
}

/// A crop resize handle, named by the rect coordinates it moves (view space, y up).
enum CropHandle: CaseIterable {
    case minXMinY, midXMinY, maxXMinY
    case minXMidY, maxXMidY
    case minXMaxY, midXMaxY, maxXMaxY

    func point(in rect: CGRect) -> CGPoint {
        switch self {
        case .minXMinY: return CGPoint(x: rect.minX, y: rect.minY)
        case .midXMinY: return CGPoint(x: rect.midX, y: rect.minY)
        case .maxXMinY: return CGPoint(x: rect.maxX, y: rect.minY)
        case .minXMidY: return CGPoint(x: rect.minX, y: rect.midY)
        case .maxXMidY: return CGPoint(x: rect.maxX, y: rect.midY)
        case .minXMaxY: return CGPoint(x: rect.minX, y: rect.maxY)
        case .midXMaxY: return CGPoint(x: rect.midX, y: rect.maxY)
        case .maxXMaxY: return CGPoint(x: rect.maxX, y: rect.maxY)
        }
    }

    /// The handle for the edge strip, `width` points deep inside `rect`, that contains `point`.
    static func onEdge(at point: CGPoint, of rect: CGRect, width: CGFloat) -> CropHandle? {
        guard rect.contains(point) else { return nil }
        let nearMinX = point.x - rect.minX < width
        let nearMaxX = rect.maxX - point.x < width
        let nearMaxY = rect.maxY - point.y < width
        let nearMinY = point.y - rect.minY < width

        if nearMaxY && nearMinX { return .minXMaxY }
        if nearMaxY && nearMaxX { return .maxXMaxY }
        if nearMinY && nearMinX { return .minXMinY }
        if nearMinY && nearMaxX { return .maxXMinY }
        if nearMaxY { return .midXMaxY }
        if nearMinY { return .midXMinY }
        if nearMinX { return .minXMidY }
        if nearMaxX { return .maxXMidY }
        return nil
    }

    func resize(_ rect: CGRect, dx: CGFloat, dy: CGFloat, minSize: CGFloat, bounds: CGSize) -> CGRect {
        var minX = rect.minX
        var minY = rect.minY
        var maxX = rect.maxX
        var maxY = rect.maxY

        switch self {
        case .minXMinY: minX += dx; minY += dy
        case .midXMinY: minY += dy
        case .maxXMinY: maxX += dx; minY += dy
        case .minXMidY: minX += dx
        case .maxXMidY: maxX += dx
        case .minXMaxY: minX += dx; maxY += dy
        case .midXMaxY: maxY += dy
        case .maxXMaxY: maxX += dx; maxY += dy
        }

        if maxX - minX < minSize { maxX = minX + minSize }
        if maxY - minY < minSize { maxY = minY + minSize }
        if minX > maxX - minSize { minX = maxX - minSize }
        if minY > maxY - minSize { minY = maxY - minSize }

        minX = max(0, minX)
        minY = max(0, minY)
        maxX = min(bounds.width, maxX)
        maxY = min(bounds.height, maxY)

        return CGRect(x: minX, y: minY, width: maxX - minX, height: maxY - minY)
    }
}
