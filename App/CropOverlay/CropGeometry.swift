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
    let list = CGWindowListCopyWindowInfo([.optionIncludingWindow], windowID) as? [[String: Any]]
    guard let info = list?.first,
          let bounds = info[kCGWindowBounds as String] as? [String: CGFloat],
          let x = bounds["X"], let y = bounds["Y"],
          let w = bounds["Width"], let h = bounds["Height"] else { return nil }
    return CGRect(x: x, y: primaryScreenHeight - y - h, width: w, height: h)
}
