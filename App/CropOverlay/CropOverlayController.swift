import AppKit

@MainActor
final class CropOverlayController {
    var onCropChanged: ((CGRect, CGSize) -> Void)?
    var onApply: (() -> Void)?

    private var panel: CropOverlayPanel?
    private var trackingTimer: Timer?
    private var lastKnownWindowID: CGWindowID?

    func show(windowID: CGWindowID, currentCrop: CGRect?) -> Bool {
        guard let frame = cgWindowFrame(for: windowID) else { return false }
        lastKnownWindowID = windowID

        let p = CropOverlayPanel(frame: frame)

        if let crop = currentCrop, crop != .zero {
            p.overlayView.cropRect = crop
        } else {
            p.overlayView.cropRect = CGRect(origin: .zero, size: frame.size)
        }

        p.overlayView.onCropChanged = { [weak self, weak p] rect in
            guard let size = p?.overlayView.bounds.size else { return }
            self?.onCropChanged?(rect, size)
        }

        p.overlayView.onApply = { [weak self] in
            self?.onApply?()
        }

        p.orderFront(nil)
        self.panel = p

        startTracking()
        return true
    }

    func hide() {
        stopTracking()
        panel?.close()
        panel = nil
    }

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

    private func updatePanelFrame() {
        guard let panel else { return }
        guard let windowID = lastKnownWindowID, let newFrame = cgWindowFrame(for: windowID) else { return }
        let oldFrame = panel.frame

        let originMoved = abs(newFrame.origin.x - oldFrame.origin.x) > 1
                       || abs(newFrame.origin.y - oldFrame.origin.y) > 1
        let sizeChanged = abs(newFrame.width - oldFrame.width) > 2
                       || abs(newFrame.height - oldFrame.height) > 2

        if !originMoved && !sizeChanged { return }

        if sizeChanged {
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
            panel.setFrameOrigin(newFrame.origin)
        }
    }

    private func cgWindowFrame(for windowID: CGWindowID) -> CGRect? {
        windowFrame(for: windowID, primaryScreenHeight: NSScreen.screens.first?.frame.height ?? 0)
    }
}
