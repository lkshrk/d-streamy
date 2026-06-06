import SwiftUI
import AppKit
import ScreenCaptureKit
import CoreMedia
import CaptureLib

@main
struct DStreamyApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) var appDelegate
    @StateObject private var appState: AppState

    init() {
        if CommandLine.arguments.contains("--capture-diagnostic") {
            CaptureDiagnosticRunner.runAndExit()
        }

        _appState = StateObject(wrappedValue: AppState())
    }

    var body: some Scene {
        MenuBarExtra {
            PopoverView()
                .environmentObject(appState)
                .frame(width: 300)
                .onAppear {
                    appDelegate.appState = appState
                }
        } label: {
            MenuBarLabel(
                isActive: appState.streamState.isActive,
                dotColor: appState.healthLevel.nsColor,
                dotVisible: appState.menuDotVisible
            )
        }
        .menuBarExtraStyle(.window)
    }
}

struct MenuBarLabel: View {
    let isActive: Bool
    let dotColor: NSColor
    let dotVisible: Bool

    var body: some View {
        Image(nsImage: makeStatusIcon(active: isActive))
    }

    private func makeStatusIcon(active: Bool) -> NSImage {
        let size = NSSize(width: 18, height: 18)
        let image = NSImage(size: size, flipped: false) { rect in
            let symbolName = active ? "video.fill" : "video"
            if let symbol = NSImage(systemSymbolName: symbolName, accessibilityDescription: nil) {
                var config = NSImage.SymbolConfiguration(pointSize: 13, weight: .regular)
                if active {
                    // Tint white so it looks correct against dark menu bar when non-template
                    config = config.applying(.init(paletteColors: [.white]))
                }
                let configured = symbol.withSymbolConfiguration(config) ?? symbol
                let symbolSize = configured.size
                let symbolOrigin = NSPoint(
                    x: (rect.width - symbolSize.width) / 2 - 2,
                    y: (rect.height - symbolSize.height) / 2
                )
                configured.draw(in: NSRect(origin: symbolOrigin, size: symbolSize))
            }
            if active && dotVisible {
                let dotSize: CGFloat = 5
                let dotRect = NSRect(x: rect.width - dotSize - 1, y: rect.height - dotSize - 1, width: dotSize, height: dotSize)
                dotColor.setFill()
                NSBezierPath(ovalIn: dotRect).fill()
            }
            return true
        }
        image.isTemplate = !active
        return image
    }
}

class AppDelegate: NSObject, NSApplicationDelegate {
    private var eventMonitor: Any?
    weak var appState: AppState?
    private var isTerminating = false

    func applicationDidFinishLaunching(_ notification: Notification) {
        // Right-click on status bar item → context menu
        eventMonitor = NSEvent.addLocalMonitorForEvents(matching: .rightMouseDown) { event in
            guard let view = event.window?.contentView?.hitTest(event.locationInWindow),
                  String(describing: type(of: view)) == "NSStatusBarButton" else {
                return event
            }

            let menu = NSMenu()
            menu.addItem(withTitle: "Quit D-Streamy", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
            NSMenu.popUpContextMenu(menu, with: event, for: view)
            return nil
        }
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard !isTerminating else { return .terminateNow }
        guard let appState else { return .terminateNow }

        isTerminating = true
        Task { @MainActor in
            await appState.shutdownForQuit()
            sender.reply(toApplicationShouldTerminate: true)
        }

        return .terminateLater
    }
}

private enum CaptureDiagnosticRunner {
    static func runAndExit() -> Never {
        _ = NSApplication.shared

        var exitCode: Int32 = 1
        var done = false

        Task {
            exitCode = await run()
            done = true
            CFRunLoopStop(CFRunLoopGetMain())
        }

        while !done {
            CFRunLoopRun()
        }

        Foundation.exit(exitCode)
    }

    private static func run() async -> Int32 {
        do {
            guard let window = try await savedWindow() else {
                print("[capture-diag] no saved window found")
                return 2
            }

            let app = window.owningApplication?.applicationName ?? "?"
            let title = window.title ?? ""
            print("[capture-diag] window=\(app) — \(title)")

            let mode = diagnosticMode()
            let filter: SCContentFilter
            if mode == "display" {
                filter = SCContentFilter(
                    display: try await displayContaining(window: window),
                    excludingApplications: [],
                    exceptingWindows: []
                )
                print("[capture-diag] mode=display")
            } else if mode == "app",
               let application = window.owningApplication {
                filter = SCContentFilter(
                    display: try await displayContaining(window: window),
                    including: [application],
                    exceptingWindows: []
                )
                print("[capture-diag] mode=app")
            } else {
                filter = SCContentFilter(desktopIndependentWindow: window)
                print("[capture-diag] mode=window")
            }
            let rect = filter.contentRect
            let scale = filter.pointPixelScale
            let sourceWidth = max(2, Int(rect.width * CGFloat(scale)))
            let sourceHeight = max(2, Int(rect.height * CGFloat(scale)))
            let dimensions = CaptureDimensions.resolve(
                sourceWidth: sourceWidth,
                sourceHeight: sourceHeight,
                maxWidth: 1280,
                maxHeight: 720,
                crop: nil
            )

            let capture = WindowCapture()
            let counters = CaptureDiagnosticCounters()
            capture.onVideoFrame = { _, _ in counters.recordVideo() }
            capture.onAudioBuffer = { sampleBuffer in counters.recordAudio(sampleBuffer) }

            do {
                try await withTimeout(seconds: 10) {
                    try await capture.start(filter: filter, width: dimensions.streamWidth, height: dimensions.streamHeight, fps: 30)
                }
            } catch {
                await capture.stop()
                print("[capture-diag] start failed: \(error.localizedDescription)")
                return 3
            }

            print("[capture-diag] started \(dimensions.streamWidth)x\(dimensions.streamHeight), source=\(sourceWidth)x\(sourceHeight), sampling for 5s")
            try? await Task.sleep(for: .seconds(5))
            await capture.stop()

            let snapshot = counters.snapshot()
            print(
                "[capture-diag] result videoFrames=\(snapshot.videoFrames) audioBuffers=\(snapshot.audioBuffers) audioSamples=\(snapshot.audioSamples) firstAudio=\(snapshot.firstAudioFormat)"
            )

            return snapshot.audioBuffers > 0 ? 0 : 4
        } catch {
            print("[capture-diag] failed: \(error.localizedDescription)")
            return 1
        }
    }

    private static func savedWindow() async throws -> SCWindow? {
        let defaults = UserDefaults.standard
        guard let bundleId = defaults.string(forKey: "lastWindowBundleId"),
              !bundleId.isEmpty else { return nil }

        let savedTitle = defaults.string(forKey: "lastWindowTitle") ?? ""
        let savedFrame = defaults.array(forKey: "lastWindowFrame") as? [CGFloat]
        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
        let candidates = content.windows.filter {
            $0.owningApplication?.bundleIdentifier == bundleId
            && $0.frame.width > 100
            && $0.frame.height > 100
        }

        if let exact = candidates.first(where: { $0.title == savedTitle }) {
            return exact
        }

        if let partial = candidates.first(where: { ($0.title ?? "").contains(savedTitle) }) {
            return partial
        }

        if let savedFrame, savedFrame.count == 4 {
            let saved = CGRect(x: savedFrame[0], y: savedFrame[1], width: savedFrame[2], height: savedFrame[3])
            return candidates.min { frameDistance($0.frame, saved) < frameDistance($1.frame, saved) }
        }

        return candidates.first
    }

    private static func frameDistance(_ a: CGRect, _ b: CGRect) -> CGFloat {
        abs(a.minX - b.minX)
        + abs(a.minY - b.minY)
        + abs(a.width - b.width)
        + abs(a.height - b.height)
    }

    private static func diagnosticMode() -> String {
        guard let index = CommandLine.arguments.firstIndex(of: "--capture-diagnostic-mode"),
              CommandLine.arguments.indices.contains(index + 1) else {
            return "window"
        }

        return CommandLine.arguments[index + 1]
    }

    private static func displayContaining(window: SCWindow) async throws -> SCDisplay {
        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
        let nearestScreenNumber = NSScreen.screens
            .compactMap { screen -> (screenNumber: NSNumber, distance: CGFloat)? in
                guard let screenNumber = screen.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber else {
                    return nil
                }
                let frame = screen.frame
                let windowMid = CGPoint(x: window.frame.midX, y: window.frame.midY)
                let screenMid = CGPoint(x: frame.midX, y: frame.midY)
                let contains = frame.contains(windowMid)
                let distance = contains
                    ? 0
                    : abs(windowMid.x - screenMid.x) + abs(windowMid.y - screenMid.y)
                return (screenNumber, distance)
            }
            .sorted { $0.distance < $1.distance }
            .first?
            .screenNumber

        if let nearestScreenNumber,
           let display = content.displays.first(where: { $0.displayID == CGDirectDisplayID(nearestScreenNumber.uint32Value) }) {
            return display
        }

        if let display = content.displays.first {
            return display
        }

        throw CaptureDiagnosticError.noDisplay
    }

    private static func withTimeout(
        seconds: Int,
        operation: @escaping @Sendable () async throws -> Void
    ) async throws {
        try await withThrowingTaskGroup(of: Void.self) { group in
            group.addTask {
                try await operation()
            }
            group.addTask {
                try await Task.sleep(for: .seconds(seconds))
                throw CaptureDiagnosticError.timeout
            }

            try await group.next()
            group.cancelAll()
        }
    }
}

private enum CaptureDiagnosticError: LocalizedError {
    case timeout
    case noDisplay

    var errorDescription: String? {
        switch self {
        case .timeout:
            return "timed out starting capture"
        case .noDisplay:
            return "no shareable display found"
        }
    }
}

private final class CaptureDiagnosticCounters {
    private let lock = NSLock()
    private var videoFrames = 0
    private var audioBuffers = 0
    private var audioSamples = 0
    private var firstAudioFormat = "none"

    func recordVideo() {
        lock.lock()
        videoFrames += 1
        lock.unlock()
    }

    func recordAudio(_ sampleBuffer: CMSampleBuffer) {
        let samples = CMSampleBufferGetNumSamples(sampleBuffer)
        let format = audioFormatDescription(sampleBuffer)

        lock.lock()
        audioBuffers += 1
        audioSamples += samples
        if firstAudioFormat == "none" {
            firstAudioFormat = format
        }
        lock.unlock()
    }

    func snapshot() -> (videoFrames: Int, audioBuffers: Int, audioSamples: Int, firstAudioFormat: String) {
        lock.lock()
        defer { lock.unlock() }
        return (videoFrames, audioBuffers, audioSamples, firstAudioFormat)
    }

    private func audioFormatDescription(_ sampleBuffer: CMSampleBuffer) -> String {
        guard let formatDescription = CMSampleBufferGetFormatDescription(sampleBuffer),
              let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(formatDescription) else {
            return "unknown"
        }

        let channels = asbd.pointee.mChannelsPerFrame
        let sampleRate = asbd.pointee.mSampleRate
        let formatId = asbd.pointee.mFormatID
        let flags = asbd.pointee.mFormatFlags
        return "rate=\(Int(sampleRate)) channels=\(channels) format=\(formatId) flags=\(flags)"
    }
}
