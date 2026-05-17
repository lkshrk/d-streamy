import AppKit

/// NSView that draws a dimmed overlay with a transparent crop region the user can drag and resize.
final class CropOverlayView: NSView {

    // MARK: - Public

    /// Called whenever the crop rect changes (in view-local coordinates, top-left origin).
    var onCropChanged: ((CGRect) -> Void)?
    /// Called when Apply button is clicked.
    var onApply: (() -> Void)?

    /// Current crop rect in view-local coordinates.
    var cropRect: CGRect = .zero {
        didSet {
            needsDisplay = true
            positionApplyButton()
            window?.invalidateCursorRects(for: self)
            if notifiesChanges { onCropChanged?(cropRect) }
        }
    }

    /// Set to false to update cropRect without firing callback (e.g., during tracking).
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

    // MARK: - Private constants

    private let handleRadius: CGFloat = 6
    private let minCropSize: CGFloat = 100
    private let overlayAlpha: CGFloat = 0.4
    private let borderColor = NSColor.white
    private let borderWidth: CGFloat = 2

    // MARK: - Resize handle positions (8 handles)

    private enum Handle: CaseIterable {
        case topLeft, top, topRight
        case left, right
        case bottomLeft, bottom, bottomRight

        func point(in rect: CGRect) -> CGPoint {
            switch self {
            case .topLeft:     return CGPoint(x: rect.minX, y: rect.minY)
            case .top:         return CGPoint(x: rect.midX, y: rect.minY)
            case .topRight:    return CGPoint(x: rect.maxX, y: rect.minY)
            case .left:        return CGPoint(x: rect.minX, y: rect.midY)
            case .right:       return CGPoint(x: rect.maxX, y: rect.midY)
            case .bottomLeft:  return CGPoint(x: rect.minX, y: rect.maxY)
            case .bottom:      return CGPoint(x: rect.midX, y: rect.maxY)
            case .bottomRight: return CGPoint(x: rect.maxX, y: rect.maxY)
            }
        }
    }

    // MARK: - Drag state

    private enum DragMode {
        case none
        case move(startMouseInView: CGPoint, startCropRect: CGRect)
        case resize(handle: Handle, startMouseInView: CGPoint, startCropRect: CGRect)
    }

    private var dragMode: DragMode = .none

    // MARK: - Init

    override init(frame: NSRect) {
        super.init(frame: frame)
        addSubview(applyButton)
        addSubview(dimensionsLabel)
        cropRect = bounds  // start full-view (no crop)
    }

    required init?(coder: NSCoder) {
        super.init(coder: coder)
        addSubview(applyButton)
        addSubview(dimensionsLabel)
        cropRect = bounds
    }

    @objc private func applyTapped() {
        onApply?()
    }

    private func positionApplyButton() {
        let padding: CGFloat = 14

        // Apply button — bottom center
        let btnW = applyButton.frame.width
        applyButton.frame.origin = CGPoint(
            x: cropRect.midX - btnW / 2,
            y: cropRect.minY + padding
        )

        // Dimensions label — top center
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
        // Reset crop to full view when first placed
        if cropRect == .zero {
            cropRect = bounds
        }
    }

    // MARK: - Layout

    override func layout() {
        super.layout()
        // If crop was full view before resize, keep it full
        if cropRect == .zero || cropRect == CGRect(origin: .zero, size: CGSize(width: bounds.width, height: bounds.height)) {
            cropRect = bounds
        }
    }

    // MARK: - Drawing

    override func draw(_ dirtyRect: NSRect) {
        guard let ctx = NSGraphicsContext.current?.cgContext else { return }

        // 1. Fill entire view with semi-transparent black
        ctx.setFillColor(NSColor.black.withAlphaComponent(overlayAlpha).cgColor)
        ctx.fill(bounds)

        // 2. Cut out the crop rect (show underlying window)
        ctx.clear(cropRect)

        // 3. Draw border around crop rect
        ctx.setStrokeColor(borderColor.cgColor)
        ctx.setLineWidth(borderWidth)
        ctx.stroke(cropRect.insetBy(dx: borderWidth / 2, dy: borderWidth / 2))

        // 4. Draw resize handles
        for handle in Handle.allCases {
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

    // MARK: - Hit testing

    private func hitHandle(at point: CGPoint) -> Handle? {
        for handle in Handle.allCases {
            let center = handle.point(in: cropRect)
            let dx = point.x - center.x
            let dy = point.y - center.y
            if dx * dx + dy * dy <= (handleRadius + 4) * (handleRadius + 4) {
                return handle
            }
        }
        return nil
    }

    private func hitEdge(at point: CGPoint) -> Handle? {
        let edgeW: CGFloat = handleRadius + 4
        guard cropRect.contains(point) else { return nil }
        let nearLeft = point.x - cropRect.minX < edgeW
        let nearRight = cropRect.maxX - point.x < edgeW
        let nearTop = cropRect.maxY - point.y < edgeW
        let nearBottom = point.y - cropRect.minY < edgeW

        // Corners
        if nearTop && nearLeft { return .topLeft }
        if nearTop && nearRight { return .topRight }
        if nearBottom && nearLeft { return .bottomLeft }
        if nearBottom && nearRight { return .bottomRight }
        // Edges
        if nearTop { return .top }
        if nearBottom { return .bottom }
        if nearLeft { return .left }
        if nearRight { return .right }
        return nil
    }

    private func isInsideCropRect(_ point: CGPoint) -> Bool {
        cropRect.contains(point)
    }

    // MARK: - Mouse events

    override func mouseDown(with event: NSEvent) {
        let loc = convert(event.locationInWindow, from: nil)
        // Don't start drag if clicking the apply button
        if applyButton.frame.contains(loc) { return super.mouseDown(with: event) }
        if let handle = hitHandle(at: loc) {
            dragMode = .resize(handle: handle, startMouseInView: loc, startCropRect: cropRect)
        } else if let edgeHandle = hitEdge(at: loc) {
            dragMode = .resize(handle: edgeHandle, startMouseInView: loc, startCropRect: cropRect)
        } else if isInsideCropRect(loc) {
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
            // Clamp to view bounds
            newRect.origin.x = max(0, min(newRect.origin.x, bounds.width - newRect.width))
            newRect.origin.y = max(0, min(newRect.origin.y, bounds.height - newRect.height))
            cropRect = newRect
        case .resize(let handle, let startMouse, let startRect):
            let dx = loc.x - startMouse.x
            let dy = loc.y - startMouse.y
            cropRect = resized(startRect, handle: handle, dx: dx, dy: dy)
        }
    }

    override func mouseUp(with event: NSEvent) {
        if case .move = dragMode {
            NSCursor.pop()
        }
        dragMode = .none
    }

    // MARK: - Resize logic

    private func resized(_ rect: CGRect, handle: Handle, dx: CGFloat, dy: CGFloat) -> CGRect {
        var minX = rect.minX
        var minY = rect.minY
        var maxX = rect.maxX
        var maxY = rect.maxY

        switch handle {
        case .topLeft:
            minX += dx; minY += dy
        case .top:
            minY += dy
        case .topRight:
            maxX += dx; minY += dy
        case .left:
            minX += dx
        case .right:
            maxX += dx
        case .bottomLeft:
            minX += dx; maxY += dy
        case .bottom:
            maxY += dy
        case .bottomRight:
            maxX += dx; maxY += dy
        }

        // Enforce minimum size
        if maxX - minX < minCropSize { maxX = minX + minCropSize }
        if maxY - minY < minCropSize { maxY = minY + minCropSize }

        // Enforce non-negative origin after min-size clamp
        if minX > maxX - minCropSize { minX = maxX - minCropSize }
        if minY > maxY - minCropSize { minY = maxY - minCropSize }

        // Clamp to view bounds
        minX = max(0, minX)
        minY = max(0, minY)
        maxX = min(bounds.width, maxX)
        maxY = min(bounds.height, maxY)

        return CGRect(x: minX, y: minY, width: maxX - minX, height: maxY - minY)
    }

    // MARK: - Cursor management

    override func resetCursorRects() {
        discardCursorRects()

        let edgeW: CGFloat = handleRadius + 4  // 10pt edge strip

        // Interior (inset from edges) → open hand
        let interior = cropRect.insetBy(dx: edgeW, dy: edgeW)
        if interior.width > 0 && interior.height > 0 {
            addCursorRect(interior, cursor: .openHand)
        }

        // Edge strips (between corners) → resize
        // Top edge
        addCursorRect(CGRect(x: cropRect.minX + edgeW, y: cropRect.maxY - edgeW,
                             width: cropRect.width - edgeW * 2, height: edgeW), cursor: .resizeUpDown)
        // Bottom edge
        addCursorRect(CGRect(x: cropRect.minX + edgeW, y: cropRect.minY,
                             width: cropRect.width - edgeW * 2, height: edgeW), cursor: .resizeUpDown)
        // Left edge
        addCursorRect(CGRect(x: cropRect.minX, y: cropRect.minY + edgeW,
                             width: edgeW, height: cropRect.height - edgeW * 2), cursor: .resizeLeftRight)
        // Right edge
        addCursorRect(CGRect(x: cropRect.maxX - edgeW, y: cropRect.minY + edgeW,
                             width: edgeW, height: cropRect.height - edgeW * 2), cursor: .resizeLeftRight)

        // Corners → diagonal resize (system-style)
        // Bottom-left corner
        addCursorRect(CGRect(x: cropRect.minX, y: cropRect.minY, width: edgeW, height: edgeW), cursor: Self.cursorNESW)
        // Bottom-right corner
        addCursorRect(CGRect(x: cropRect.maxX - edgeW, y: cropRect.minY, width: edgeW, height: edgeW), cursor: Self.cursorNWSE)
        // Top-left corner
        addCursorRect(CGRect(x: cropRect.minX, y: cropRect.maxY - edgeW, width: edgeW, height: edgeW), cursor: Self.cursorNWSE)
        // Top-right corner
        addCursorRect(CGRect(x: cropRect.maxX - edgeW, y: cropRect.maxY - edgeW, width: edgeW, height: edgeW), cursor: Self.cursorNESW)

        // Apply button → arrow (overrides interior)
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

    private func cursorForHandle(_ handle: Handle) -> NSCursor {
        switch handle {
        case .topLeft, .bottomRight:
            return Self.cursorNWSE
        case .topRight, .bottomLeft:
            return Self.cursorNESW
        case .top, .bottom:
            return .resizeUpDown
        case .left, .right:
            return .resizeLeftRight
        }
    }

    // MARK: - Accept mouse events

    override var acceptsFirstResponder: Bool { true }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
}
