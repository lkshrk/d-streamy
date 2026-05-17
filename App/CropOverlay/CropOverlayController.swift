import AppKit
import ScreenCaptureKit

/// Manages the lifecycle of CropOverlayPanel: shows, hides, and tracks the target window.
@MainActor
final class CropOverlayController {

    var onCropChanged: ((CGRect) -> Void)?
    var onApply: (() -> Void)?

    private var panel: CropOverlayPanel?
    private var trackingTimer: Timer?
    private var currentFilter: SCContentFilter?

    // MARK: - Show / Hide

    func show(for filter: SCContentFilter, currentCrop: CGRect?) {
        currentFilter = filter

        if #available(macOS 15.2, *), let window = filter.includedWindows.first {
            lastKnownWindowID = window.windowID
        } else {
            lastKnownWindowID = 0
        }

        let frame = windowFrame(for: filter)
        let p = CropOverlayPanel(frame: frame)

        // Set initial crop
        if let crop = currentCrop, crop != .zero {
            // Convert from top-left (AppKit) stored crop to view coords.
            // CropOverlayView uses NSView coordinates which are bottom-left on macOS,
            // but we normalise in AppState, so just set directly.
            p.overlayView.cropRect = crop
        } else {
            p.overlayView.cropRect = CGRect(origin: .zero, size: frame.size)
        }

        p.overlayView.onCropChanged = { [weak self] rect in
            self?.onCropChanged?(rect)
        }

        p.onApply = { [weak self] in
            self?.onApply?()
        }

        p.orderFront(nil)
        self.panel = p

        startTracking()
    }

    func hide() {
        stopTracking()
        panel?.close()
        panel = nil
        currentFilter = nil
    }

    // MARK: - Window tracking

    private func startTracking() {
        stopTracking()
        trackingTimer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            Task { @MainActor [weak self] in self?.updatePanelFrame() }
        }
    }

    private func stopTracking() {
        trackingTimer?.invalidate()
        trackingTimer = nil
    }

    private var lastKnownWindowID: CGWindowID = 0

    private func updatePanelFrame() {
        guard let panel = panel else { return }
        // Only track if we can read window position (requires screen recording permission)
        guard lastKnownWindowID > 0, let newFrame = cgWindowFrame(for: lastKnownWindowID) else { return }
        let oldFrame = panel.frame

        // Only reposition if origin moved (ignore sub-point jitter)
        let originMoved = abs(newFrame.origin.x - oldFrame.origin.x) > 1
                       || abs(newFrame.origin.y - oldFrame.origin.y) > 1
        // Only resize if size actually changed meaningfully
        let sizeChanged = abs(newFrame.width - oldFrame.width) > 2
                       || abs(newFrame.height - oldFrame.height) > 2

        if !originMoved && !sizeChanged { return }

        if sizeChanged {
            // Window resized — scale crop proportionally
            let oldSize = oldFrame.size
            let newSize = newFrame.size
            var crop = panel.overlayView.cropRect
            if oldSize.width > 0 && oldSize.height > 0 {
                let scaleX = newSize.width / oldSize.width
                let scaleY = newSize.height / oldSize.height
                crop = CGRect(
                    x: crop.origin.x * scaleX,
                    y: crop.origin.y * scaleY,
                    width: crop.width * scaleX,
                    height: crop.height * scaleY
                )
            }
            panel.setFrame(newFrame, display: true)
            panel.overlayView.frame = CGRect(origin: .zero, size: newSize)
            panel.overlayView.notifiesChanges = false
            panel.overlayView.cropRect = crop
            panel.overlayView.notifiesChanges = true
        } else {
            // Just moved — reposition panel without touching crop
            panel.setFrameOrigin(newFrame.origin)
        }
    }

    // MARK: - Frame resolution

    private func windowFrame(for filter: SCContentFilter) -> CGRect {
        // Use primary screen height for CG→AppKit coord conversion (never changes)
        let screenHeight = NSScreen.screens.first?.frame.height ?? 0

        // Try to get the live window frame via CGWindowListCopyWindowInfo
        if #available(macOS 15.2, *), let window = filter.includedWindows.first {
            if let frame = cgWindowFrame(for: window.windowID) {
                return frame
            }
            // Fallback to contentRect (static from filter creation)
            let contentRect = filter.contentRect
            let y = screenHeight - contentRect.origin.y - contentRect.height
            return CGRect(x: contentRect.origin.x, y: y, width: contentRect.width, height: contentRect.height)
        }

        // Legacy fallback
        let contentRect = filter.contentRect
        let y = screenHeight - contentRect.origin.y - contentRect.height
        return CGRect(x: contentRect.origin.x, y: y, width: contentRect.width, height: contentRect.height)
    }

    private func cgWindowFrame(for windowID: CGWindowID) -> CGRect? {
        let list = CGWindowListCopyWindowInfo([.optionIncludingWindow], windowID) as? [[String: Any]]
        guard let info = list?.first,
              let boundsDict = info[kCGWindowBounds as String] as? [String: CGFloat],
              let x = boundsDict["X"],
              let y = boundsDict["Y"],
              let w = boundsDict["Width"],
              let h = boundsDict["Height"] else { return nil }

        // CGWindowListCopyWindowInfo returns CG coords (top-left origin).
        // Convert to NSScreen (bottom-left origin) using screens[0] (full display height).
        let screenHeight = NSScreen.screens.first?.frame.height ?? 0
        return CGRect(x: x, y: screenHeight - y - h, width: w, height: h)
    }
}
