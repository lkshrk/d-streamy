import AppKit

/// NSView that draws a dimmed overlay with a transparent crop region the user can drag and resize.
final class CropOverlayView: NSView {
    var onCropChanged: ((CGRect) -> Void)?
    var onApply: (() -> Void)?

    var cropRect: CGRect = .zero {
        didSet {
            needsDisplay = true
            layoutCropControls()
            window?.invalidateCursorRects(for: self)
            if notifiesChanges { onCropChanged?(cropRect) }
        }
    }

    /// False while tracking moves the panel, so the move isn't reported as a user crop.
    var notifiesChanges = true

    private lazy var applyButton: NSButton = {
        let btn = NSButton(title: "Apply", target: self, action: #selector(applyTapped))
        btn.bezelStyle = .rounded
        btn.isBordered = true
        btn.contentTintColor = .white
        btn.wantsLayer = true
        btn.layer?.backgroundColor = NSColor.controlAccentColor.cgColor
        btn.layer?.cornerRadius = 5
        btn.font = .systemFont(ofSize: 12, weight: .medium)
        btn.sizeToFit()
        btn.frame.size.width = max(btn.frame.width + 16, 64)
        return btn
    }()

    private lazy var dimensionsLabel: NSTextField = {
        let label = NSTextField(labelWithString: "")
        label.font = .monospacedDigitSystemFont(ofSize: 13, weight: .bold)
        label.textColor = .white
        label.backgroundColor = .clear
        label.isBezeled = false
        label.drawsBackground = false
        label.alignment = .center
        label.shadow = {
            let s = NSShadow()
            s.shadowColor = NSColor.black.withAlphaComponent(0.5)
            s.shadowBlurRadius = 2
            s.shadowOffset = NSSize(width: 0, height: -1)
            return s
        }()
        return label
    }()

    private let handleRadius: CGFloat = 6
    private let minCropSize: CGFloat = 100
    private let overlayAlpha: CGFloat = 0.4
    private let borderColor = NSColor.white
    private let borderWidth: CGFloat = 2

    private enum DragMode {
        case none
        case move(startMouseInView: CGPoint, startCropRect: CGRect)
        case resize(handle: CropHandle, startMouseInView: CGPoint, startCropRect: CGRect)
    }

    private var dragMode: DragMode = .none

    override init(frame: NSRect) {
        super.init(frame: frame)
        addControls()
        cropRect = bounds
    }

    required init?(coder: NSCoder) {
        super.init(coder: coder)
        addControls()
        cropRect = bounds
    }

    private func addControls() {
        addSubview(applyButton)
        addSubview(dimensionsLabel)
    }

    @objc private func applyTapped() {
        onApply?()
    }

    private func layoutCropControls() {
        let padding: CGFloat = 14

        let btnW = applyButton.frame.width
        applyButton.frame.origin = CGPoint(
            x: cropRect.midX - btnW / 2,
            y: cropRect.minY + padding
        )

        dimensionsLabel.stringValue = " \(Int(cropRect.width)) × \(Int(cropRect.height)) "
        dimensionsLabel.sizeToFit()
        let lblW = dimensionsLabel.frame.width + 8
        let lblH = dimensionsLabel.frame.height
        dimensionsLabel.frame = NSRect(
            x: cropRect.midX - lblW / 2,
            y: cropRect.maxY - lblH - padding,
            width: lblW,
            height: lblH
        )
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        if cropRect == .zero {
            cropRect = bounds
        }
    }

    override func layout() {
        super.layout()
        // If crop was full view before resize, keep it full
        if cropRect == .zero || cropRect == CGRect(origin: .zero, size: CGSize(width: bounds.width, height: bounds.height)) {
            cropRect = bounds
        }
    }

    override func draw(_ dirtyRect: NSRect) {
        guard let ctx = NSGraphicsContext.current?.cgContext else { return }

        ctx.setFillColor(NSColor.black.withAlphaComponent(overlayAlpha).cgColor)
        ctx.fill(bounds)
        ctx.clear(cropRect)
        ctx.setStrokeColor(borderColor.cgColor)
        ctx.setLineWidth(borderWidth)
        ctx.stroke(cropRect.insetBy(dx: borderWidth / 2, dy: borderWidth / 2))

        for handle in CropHandle.allCases {
            let center = handle.point(in: cropRect)
            let handleRect = CGRect(
                x: center.x - handleRadius,
                y: center.y - handleRadius,
                width: handleRadius * 2,
                height: handleRadius * 2
            )
            ctx.setFillColor(NSColor.white.cgColor)
            ctx.setStrokeColor(NSColor(white: 0.3, alpha: 1).cgColor)
            ctx.setLineWidth(1)
            ctx.fillEllipse(in: handleRect)
            ctx.strokeEllipse(in: handleRect)
        }
    }

    private func hitHandle(at point: CGPoint) -> CropHandle? {
        for handle in CropHandle.allCases {
            let center = handle.point(in: cropRect)
            let dx = point.x - center.x
            let dy = point.y - center.y
            if dx * dx + dy * dy <= (handleRadius + 4) * (handleRadius + 4) {
                return handle
            }
        }
        return nil
    }

    override func mouseDown(with event: NSEvent) {
        let loc = convert(event.locationInWindow, from: nil)
        if applyButton.frame.contains(loc) { return super.mouseDown(with: event) }
        if let handle = hitHandle(at: loc) {
            dragMode = .resize(handle: handle, startMouseInView: loc, startCropRect: cropRect)
        } else if let edgeHandle = CropHandle.onEdge(at: loc, of: cropRect, width: handleRadius + 4) {
            dragMode = .resize(handle: edgeHandle, startMouseInView: loc, startCropRect: cropRect)
        } else if cropRect.contains(loc) {
            dragMode = .move(startMouseInView: loc, startCropRect: cropRect)
            NSCursor.closedHand.push()
        } else {
            dragMode = .none
        }
    }

    override func mouseDragged(with event: NSEvent) {
        let loc = convert(event.locationInWindow, from: nil)
        switch dragMode {
        case .none:
            break
        case .move(let startMouse, let startRect):
            let dx = loc.x - startMouse.x
            let dy = loc.y - startMouse.y
            var newRect = startRect.offsetBy(dx: dx, dy: dy)
            newRect.origin.x = max(0, min(newRect.origin.x, bounds.width - newRect.width))
            newRect.origin.y = max(0, min(newRect.origin.y, bounds.height - newRect.height))
            cropRect = newRect
        case .resize(let handle, let startMouse, let startRect):
            let dx = loc.x - startMouse.x
            let dy = loc.y - startMouse.y
            cropRect = handle.resize(startRect, dx: dx, dy: dy, minSize: minCropSize, bounds: bounds.size)
        }
    }

    override func mouseUp(with event: NSEvent) {
        if case .move = dragMode {
            NSCursor.pop()
        }
        dragMode = .none
    }

    override func resetCursorRects() {
        discardCursorRects()

        let edgeW: CGFloat = handleRadius + 4
        let interior = cropRect.insetBy(dx: edgeW, dy: edgeW)
        if interior.width > 0 && interior.height > 0 {
            addCursorRect(interior, cursor: .openHand)
        }

        addCursorRect(CGRect(x: cropRect.minX + edgeW, y: cropRect.maxY - edgeW,
                             width: cropRect.width - edgeW * 2, height: edgeW), cursor: .resizeUpDown)
        addCursorRect(CGRect(x: cropRect.minX + edgeW, y: cropRect.minY,
                             width: cropRect.width - edgeW * 2, height: edgeW), cursor: .resizeUpDown)
        addCursorRect(CGRect(x: cropRect.minX, y: cropRect.minY + edgeW,
                             width: edgeW, height: cropRect.height - edgeW * 2), cursor: .resizeLeftRight)
        addCursorRect(CGRect(x: cropRect.maxX - edgeW, y: cropRect.minY + edgeW,
                             width: edgeW, height: cropRect.height - edgeW * 2), cursor: .resizeLeftRight)

        addCursorRect(CGRect(x: cropRect.minX, y: cropRect.minY, width: edgeW, height: edgeW), cursor: Self.cursorNESW)
        addCursorRect(CGRect(x: cropRect.maxX - edgeW, y: cropRect.minY, width: edgeW, height: edgeW), cursor: Self.cursorNWSE)
        addCursorRect(CGRect(x: cropRect.minX, y: cropRect.maxY - edgeW, width: edgeW, height: edgeW), cursor: Self.cursorNWSE)
        addCursorRect(CGRect(x: cropRect.maxX - edgeW, y: cropRect.maxY - edgeW, width: edgeW, height: edgeW), cursor: Self.cursorNESW)

        addCursorRect(applyButton.frame, cursor: .arrow)
    }

    private static let cursorNWSE: NSCursor = {
        if let obj = NSCursor.perform(NSSelectorFromString("_windowResizeNorthWestSouthEastCursor"))?
            .takeUnretainedValue() as? NSCursor { return obj }
        return .resizeUpDown
    }()

    private static let cursorNESW: NSCursor = {
        if let obj = NSCursor.perform(NSSelectorFromString("_windowResizeNorthEastSouthWestCursor"))?
            .takeUnretainedValue() as? NSCursor { return obj }
        return .resizeUpDown
    }()

    override var acceptsFirstResponder: Bool { true }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
}
