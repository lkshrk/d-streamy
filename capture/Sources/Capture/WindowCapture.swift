import ScreenCaptureKit
import CoreMedia
import AVFoundation
import Foundation

public final class WindowCapture: NSObject, SCStreamDelegate, SCStreamOutput {
    public var onVideoFrame: ((CVPixelBuffer, CMTime) -> Void)?
    public var onAudioBuffer: ((CMSampleBuffer) -> Void)?

    private var stream: SCStream?
    private let queue = DispatchQueue(label: "capture.stream", qos: .userInteractive)

    public override init() { super.init() }

    // MARK: - Start

    public func start(filter: SCContentFilter, width: Int, height: Int, fps: Int) async throws {
        let config = SCStreamConfiguration()
        config.width = width
        config.height = height
        config.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(fps))
        config.pixelFormat = kCVPixelFormatType_32BGRA
        config.scalesToFit = true
        config.capturesAudio = true
        config.sampleRate = 48000
        config.channelCount = 2
        config.showsCursor = false

        let s = SCStream(filter: filter, configuration: config, delegate: self)
        try s.addStreamOutput(self, type: .screen, sampleHandlerQueue: queue)
        try s.addStreamOutput(self, type: .audio, sampleHandlerQueue: queue)
        try await s.startCapture()
        self.stream = s
    }

    // MARK: - Stop

    public func stop() async {
        guard let stream else { return }
        try? await stream.stopCapture()
        self.stream = nil
    }

    // MARK: - SCStreamOutput

    public func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer,
                of type: SCStreamOutputType) {
        switch type {
        case .screen:
            guard let imageBuffer = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
            let pts = CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
            onVideoFrame?(imageBuffer, pts)
        case .audio:
            onAudioBuffer?(sampleBuffer)
        case .microphone:
            break
        @unknown default:
            break
        }
    }

    // MARK: - SCStreamDelegate

    public func stream(_ stream: SCStream, didStopWithError error: Error) {
        fputs("SCStream stopped: \(error)\n", stderr)
    }

    // MARK: - Static helpers

    /// Find a window by app name and/or title substring.
    public static func findWindow(appName: String?, windowTitle: String?) async throws -> SCWindow {
        let content = try await SCShareableContent.excludingDesktopWindows(false,
                                                                           onScreenWindowsOnly: true)
        for window in content.windows {
            let appMatch = appName.map {
                window.owningApplication?.applicationName.localizedCaseInsensitiveContains($0) == true
            } ?? true
            let titleMatch = windowTitle.map {
                window.title?.localizedCaseInsensitiveContains($0) == true
            } ?? true
            if appMatch && titleMatch {
                return window
            }
        }
        throw CaptureError.windowNotFound(appName: appName, windowTitle: windowTitle)
    }

    /// Interactive terminal picker — lists windows, user types number.
    public static func pickWindowInteractive() async throws -> SCWindow {
        let content = try await SCShareableContent.excludingDesktopWindows(false,
                                                                           onScreenWindowsOnly: true)
        let windows = filterUserWindows(content.windows)

        guard !windows.isEmpty else {
            throw CaptureError.windowNotFound(appName: nil, windowTitle: nil)
        }

        fputs("\nAvailable windows:\n", stderr)
        for (i, w) in windows.enumerated() {
            let app = w.owningApplication?.applicationName ?? "?"
            let title = w.title ?? ""
            fputs("  [\(i + 1)] \(app) — \(title)\n", stderr)
        }
        fputs("\nSelect window number: ", stderr)

        guard let line = readLine(), let choice = Int(line), choice >= 1, choice <= windows.count else {
            throw CaptureError.pickerCancelled
        }

        return windows[choice - 1]
    }

    /// List all on-screen windows as (app, title, windowID) tuples.
    public static func listWindows() async throws -> [(app: String, title: String, windowID: UInt32)] {
        let content = try await SCShareableContent.excludingDesktopWindows(false,
                                                                           onScreenWindowsOnly: true)
        return filterUserWindows(content.windows).map { w in
            (app: w.owningApplication?.applicationName ?? "?", title: w.title ?? "", windowID: w.windowID)
        }
    }

    // MARK: - Window filtering

    private static let excludedBundleIDs: Set<String> = [
        "com.apple.dock",
        "com.apple.controlcenter",
        "com.apple.notificationcenterui",
        "com.apple.WindowManager",
        "com.apple.systemuiserver",
        "com.apple.loginwindow",
        "com.apple.Spotlight",
    ]

    private static let excludedAppNames: Set<String> = [
        "Dock",
        "Control Center",
        "Notification Center",
        "Window Server",
        "Wallpaper",
        "StatusIndicator",
        "SystemUIServer",
    ]

    public static func filterUserWindows(_ windows: [SCWindow]) -> [SCWindow] {
        windows.filter { w in
            guard let app = w.owningApplication else { return false }
            let title = w.title ?? ""
            guard !title.isEmpty else { return false }

            let bundleID = app.bundleIdentifier
            let appName = app.applicationName

            // Exclude windows with no app name
            if appName.isEmpty { return false }

            // Exclude by bundle ID
            if excludedBundleIDs.contains(bundleID) { return false }

            // Exclude by app name
            if excludedAppNames.contains(appName) { return false }

            // Exclude backdrop/backstop windows
            if title.contains("Backstop") { return false }

            // Exclude tiny windows (status indicators, menu extras)
            if w.frame.width < 50 || w.frame.height < 50 { return false }

            return true
        }
    }

    public enum CaptureError: Error, Equatable, CustomStringConvertible {
        case windowNotFound(appName: String?, windowTitle: String?)
        case pickerCancelled

        public var description: String {
            switch self {
            case .windowNotFound(let app, let title):
                return "Window not found — app: \(app ?? "*"), title: \(title ?? "*")"
            case .pickerCancelled:
                return "Window picker was cancelled"
            }
        }
    }
}

