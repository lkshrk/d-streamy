import AppKit
import ScreenCaptureKit

enum CaptureKind: String {
    case window
    case display
}

func displayCaptureLabel(for display: SCDisplay, filter: SCContentFilter) -> String {
    let displayName = screenName(for: display) ?? "Display \(display.displayID)"
    let scale = CGFloat(filter.pointPixelScale)
    let width = max(1, Int(filter.contentRect.width * scale))
    let height = max(1, Int(filter.contentRect.height * scale))
    return "\(displayName) — \(width)x\(height)"
}

private func screenName(for display: SCDisplay) -> String? {
    for (index, screen) in NSScreen.screens.enumerated() {
        guard let number = screen.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber,
              number.uint32Value == display.displayID else {
            continue
        }

        if !screen.localizedName.isEmpty {
            return screen.localizedName
        }
        return "Display \(index + 1)"
    }

    return nil
}
