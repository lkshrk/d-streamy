import AppKit
import ScreenCaptureKit

/// Borderless, transparent floating NSPanel that sits over the target window
/// and hosts CropOverlayView.
final class CropOverlayPanel: NSPanel {

    let overlayView: CropOverlayView
    var onApply: (() -> Void)?

    init(frame: NSRect) {
        overlayView = CropOverlayView(frame: NSRect(origin: .zero, size: frame.size))

        super.init(
            contentRect: frame,
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false
        )

        isOpaque = false
        backgroundColor = .clear
        level = .floating
        ignoresMouseEvents = false
        isMovable = false
        collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        hasShadow = false

        overlayView.onApply = { [weak self] in self?.onApply?() }
        contentView = overlayView
    }

    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { false }
}
