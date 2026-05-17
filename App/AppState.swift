import Foundation
import ScreenCaptureKit
import Combine
import CaptureLib
import os

private let log = Logger(subsystem: "me.harke.d-streamy", category: "app")

/// Single source of truth for the app.
@MainActor
final class AppState: ObservableObject {
    // Connection
    @Published var token: String = ""
    @Published var guilds: [GuildInfo] = []
    @Published var channels: [ChannelInfo] = []
    @Published var selectedGuild: GuildInfo?
    @Published var selectedChannel: ChannelInfo?

    // Capture (set by SCContentSharingPicker)
    @Published var captureFilter: SCContentFilter?
    @Published var captureLabel: String = ""   // display name for selected content
    @Published var username: String = ""

    // Crop
    @Published var cropRect: CGRect? = nil
    @Published var isCropOverlayVisible = false

    // Stream state
    @Published var streamState: StreamState = .idle
    @Published var stats = StatsPayload(uptime: 0, fps: 0, bitrate: 0, droppedFrames: 0)

    // Settings (persisted via UserDefaults)
    @Published var maxWidth: Int { didSet { defaults.set(maxWidth, forKey: "maxWidth") } }
    @Published var maxHeight: Int { didSet { defaults.set(maxHeight, forKey: "maxHeight") } }
    @Published var fps: Int { didSet { defaults.set(fps, forKey: "fps") } }
    @Published var bitrateMbps: Double { didSet { defaults.set(bitrateMbps, forKey: "bitrateMbps") } }
    @Published var audioGain: Float { didSet { defaults.set(audioGain, forKey: "audioGain") } }

    // Components
    let daemon = DaemonController()
    private var captureSession = CaptureSession()
    private let defaults = UserDefaults.standard
    let cropOverlayController = CropOverlayController()

    enum StreamState: Equatable {
        case idle
        case connecting
        case streaming
        case reconnecting(Int)
        case error(String)

        var isActive: Bool {
            switch self {
            case .streaming, .reconnecting: return true
            default: return false
            }
        }
    }

    init() {
        // Load persisted settings
        maxWidth = defaults.object(forKey: "maxWidth") as? Int ?? 1920
        maxHeight = defaults.object(forKey: "maxHeight") as? Int ?? 1080
        fps = defaults.object(forKey: "fps") as? Int ?? 30
        bitrateMbps = defaults.object(forKey: "bitrateMbps") as? Double ?? 6.0
        audioGain = defaults.object(forKey: "audioGain") as? Float ?? 1.0

        // Load saved guild/channel selection
        if let gId = defaults.string(forKey: "lastGuildId"),
           let gName = defaults.string(forKey: "lastGuildName") {
            selectedGuild = GuildInfo(id: gId, name: gName)
        }
        if let cId = defaults.string(forKey: "lastChannelId"),
           let cName = defaults.string(forKey: "lastChannelName") {
            selectedChannel = ChannelInfo(id: cId, name: cName)
        }

        loadToken()
        daemon.onEvent = { [weak self] type, payload in
            self?.handleDaemonEvent(type, payload)
        }

        cropOverlayController.onCropChanged = { [weak self] rect in
            guard let self else { return }
            self.cropRect = rect
            self.persistCrop()
            // Live-update crop on running stream
            if self.streamState.isActive, let filter = self.captureFilter {
                let scale = filter.pointPixelScale
                let sourceH = filter.contentRect.height * CGFloat(scale)
                // NSView coords are bottom-left origin; pixel buffer is top-left origin
                let pixelY = Int(sourceH - (rect.origin.y + rect.height) * CGFloat(scale))
                let crop = CropFilter(
                    x: Int(rect.origin.x * CGFloat(scale)),
                    y: max(0, pixelY),
                    width: Int(rect.width * CGFloat(scale)),
                    height: Int(rect.height * CGFloat(scale))
                )
                if let newDims = self.captureSession.updateCrop(crop) {
                    // Encoder reinitialized with new dimensions — tell Discord
                    self.daemon.send(.updateVideo(.init(
                        width: newDims.0,
                        height: newDims.1,
                        fps: self.fps
                    )))
                }
            }
        }
        cropOverlayController.onApply = { [weak self] in
            self?.cropOverlayController.hide()
            self?.isCropOverlayVisible = false
        }

        if !token.isEmpty {
            Task { await loginAndFetch() }
        }

        // Auto-restore last selected window
        Task { await restoreWindow() }
    }

    private func restoreWindow() async {
        guard let bundleId = defaults.string(forKey: "lastWindowBundleId"),
              !bundleId.isEmpty else { return }
        let savedTitle = defaults.string(forKey: "lastWindowTitle") ?? ""
        let savedApp = defaults.string(forKey: "lastWindowApp") ?? ""
        let savedFrame = defaults.array(forKey: "lastWindowFrame") as? [CGFloat]

        do {
            let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
            let candidates = content.windows.filter {
                $0.owningApplication?.bundleIdentifier == bundleId
                && $0.frame.width > 100 && $0.frame.height > 100
            }

            // Best match: exact title > contains title > closest frame position+size
            let match: SCWindow?
            if let exact = candidates.first(where: { $0.title == savedTitle }) {
                match = exact
            } else if let partial = candidates.first(where: { ($0.title ?? "").contains(savedTitle) }) {
                match = partial
            } else if let sf = savedFrame, sf.count == 4 {
                let saved = CGRect(x: sf[0], y: sf[1], width: sf[2], height: sf[3])
                match = candidates.min(by: { frameDist($0.frame, saved) < frameDist($1.frame, saved) })
            } else {
                match = candidates.first
            }

            guard let window = match else {
                log.info("restoreWindow: no match for \(bundleId)/\(savedTitle)")
                return
            }

            let filter = SCContentFilter(desktopIndependentWindow: window)
            captureFilter = filter
            let app = window.owningApplication?.applicationName ?? savedApp
            let title = window.title ?? savedTitle
            captureLabel = app.isEmpty ? title : "\(app) — \(title)"
            loadCropForCurrentWindow()
            log.info("restoreWindow: auto-selected \(app) — \(title)")
        } catch {
            log.error("restoreWindow failed: \(error.localizedDescription)")
        }
    }

    // MARK: - Actions

    func loginAndFetch() async {
        await fetchUser()
        await fetchGuilds()
    }

    func fetchUser() async {
        guard !token.isEmpty else { return }
        guard let (data, _) = try? await discordAPI("users/@me") else { return }
        struct User: Decodable { let username: String }
        if let user = try? JSONDecoder().decode(User.self, from: data) {
            username = user.username
        }
    }

    func fetchGuilds() async {
        guard !token.isEmpty else { return }
        guard let (data, _) = try? await discordAPI("users/@me/guilds") else { return }
        if let g = try? JSONDecoder().decode([GuildInfo].self, from: data) {
            guilds = g
            if let saved = selectedGuild, g.contains(where: { $0.id == saved.id }) {
                await fetchChannels(guildId: saved.id)
            }
        }
    }

    func fetchChannels(guildId: String) async {
        guard !token.isEmpty else { return }
        guard let (data, _) = try? await discordAPI("guilds/\(guildId)/channels") else { return }
        struct DiscordChannel: Decodable { let id: String; let name: String; let type: Int }
        if let all = try? JSONDecoder().decode([DiscordChannel].self, from: data) {
            channels = all
                .filter { $0.type == 2 }
                .map { ChannelInfo(id: $0.id, name: $0.name) }
                .sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
        }
    }

    func startStream() async {
        guard let filter = captureFilter,
              let guild = selectedGuild,
              let channel = selectedChannel,
              !token.isEmpty else {
            log.warning("startStream guard failed: filter=\(self.captureFilter != nil) guild=\(self.selectedGuild != nil) channel=\(self.selectedChannel != nil) token=\(!self.token.isEmpty)")
            return
        }

        streamState = .connecting
        log.info("startStream: connecting")

        // Start daemon if needed
        if !daemon.isConnected {
            do {
                try daemon.start()
                for _ in 0..<50 {
                    try await Task.sleep(for: .milliseconds(100))
                    if daemon.isConnected { break }
                }
                guard daemon.isConnected else {
                    streamState = .error("Daemon failed to connect")
                    return
                }
            } catch {
                streamState = .error("Failed to start daemon: \(error.localizedDescription)")
                return
            }
        }
        log.info("startStream: daemon connected")

        guard let mediaFd = daemon.mediaFd else {
            streamState = .error("No media pipe available")
            return
        }

        let rect = filter.contentRect
        let scale = filter.pointPixelScale
        let sourceW = Int(rect.width * CGFloat(scale))
        let sourceH = Int(rect.height * CGFloat(scale))
        log.debug("startStream: contentRect=\(rect.debugDescription) scale=\(scale) source=\(sourceW)x\(sourceH)")

        var pixelCrop: CropFilter? = nil
        if let crop = cropRect {
            let sourceH = rect.height * CGFloat(scale)
            // NSView coords are bottom-left origin; pixel buffer is top-left origin
            let pixelY = Int(sourceH - (crop.origin.y + crop.height) * CGFloat(scale))
            pixelCrop = CropFilter(
                x: Int(crop.origin.x * CGFloat(scale)),
                y: max(0, pixelY),
                width: Int(crop.width * CGFloat(scale)),
                height: Int(crop.height * CGFloat(scale))
            )
        }

        let config = CaptureConfig(
            filter: filter,
            sourceWidth: sourceW > 0 ? sourceW : maxWidth,
            sourceHeight: sourceH > 0 ? sourceH : maxHeight,
            maxWidth: maxWidth,
            maxHeight: maxHeight,
            fps: fps,
            bitrateMbps: bitrateMbps,
            crop: pixelCrop,
            audioGain: audioGain,
            outputFd: mediaFd
        )

        do {
            try await captureSession.start(config: config)
        } catch {
            log.error("startStream: capture failed: \(error.localizedDescription)")
            streamState = .error("Capture failed: \(error.localizedDescription)")
            return
        }

        log.info("startStream: capture started \(self.captureSession.captureWidth)x\(self.captureSession.captureHeight)")

        daemon.send(.connect(.init(
            token: token,
            guildId: guild.id,
            channelId: channel.id,
            width: captureSession.captureWidth,
            height: captureSession.captureHeight,
            fps: fps
        )))
        log.info("startStream: connect command sent")
    }

    // MARK: - Crop overlay

    func toggleCropOverlay() {
        if isCropOverlayVisible {
            cropOverlayController.hide()
            isCropOverlayVisible = false
        } else {
            guard let filter = captureFilter else { return }
            // Bring target window to front so it's fully visible
            if #available(macOS 15.2, *), let window = filter.includedWindows.first {
                let pid = window.owningApplication?.processID ?? 0
                if pid > 0, let app = NSRunningApplication(processIdentifier: pid) {
                    app.activate()
                }
            }
            cropOverlayController.show(for: filter, currentCrop: cropRect)
            isCropOverlayVisible = true
        }
    }

    /// Switch capture source while streaming (new window selected live).
    func switchCaptureSource() async {
        guard let filter = captureFilter,
              let mediaFd = daemon.mediaFd else { return }

        // Stop current capture
        await captureSession.stop()

        let rect = filter.contentRect
        let scale = filter.pointPixelScale
        let sourceW = Int(rect.width * CGFloat(scale))
        let sourceH = Int(rect.height * CGFloat(scale))

        var cropFilter: CropFilter? = nil
        if let crop = cropRect {
            let sourceH = rect.height * CGFloat(scale)
            let pixelY = Int(sourceH - (crop.origin.y + crop.height) * CGFloat(scale))
            cropFilter = CropFilter(
                x: Int(crop.origin.x * CGFloat(scale)),
                y: max(0, pixelY),
                width: Int(crop.width * CGFloat(scale)),
                height: Int(crop.height * CGFloat(scale))
            )
        }

        let config = CaptureConfig(
            filter: filter,
            sourceWidth: sourceW > 0 ? sourceW : maxWidth,
            sourceHeight: sourceH > 0 ? sourceH : maxHeight,
            maxWidth: maxWidth,
            maxHeight: maxHeight,
            fps: fps,
            bitrateMbps: bitrateMbps,
            crop: cropFilter,
            audioGain: audioGain,
            outputFd: mediaFd
        )

        do {
            try await captureSession.start(config: config)
            // Update Discord video attributes if dimensions changed
            daemon.send(.updateVideo(.init(
                width: captureSession.captureWidth,
                height: captureSession.captureHeight,
                fps: fps
            )))
            log.info("switchCaptureSource: now capturing \(self.captureSession.captureWidth)x\(self.captureSession.captureHeight)")
        } catch {
            log.error("switchCaptureSource failed: \(error.localizedDescription)")
        }
    }

    func stopStream() async {
        daemon.send(.disconnect)
        // Wait for leave opcode to flush over WS before killing daemon
        try? await Task.sleep(for: .milliseconds(1000))
        daemon.stop()
        await captureSession.stop()
        streamState = .idle
        stats = StatsPayload(uptime: 0, fps: 0, bitrate: 0, droppedFrames: 0)
    }

    // MARK: - Token management

    func saveToken(_ newToken: String) {
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/usr/bin/security")
        proc.arguments = ["add-generic-password", "-U", "-s", "dstreamy", "-a", "discord-token", "-w", newToken]
        proc.standardOutput = Pipe()
        proc.standardError = Pipe()
        try? proc.run()
        proc.waitUntilExit()
        token = newToken
        Task { await loginAndFetch() }
    }

    func clearToken() {
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/usr/bin/security")
        proc.arguments = ["delete-generic-password", "-s", "dstreamy", "-a", "discord-token"]
        proc.standardOutput = Pipe()
        proc.standardError = Pipe()
        try? proc.run()
        proc.waitUntilExit()
        token = ""
        username = ""
        guilds = []
        channels = []
        selectedGuild = nil
        selectedChannel = nil
    }

    // MARK: - Config persistence

    func persistConfig() {
        if let guild = selectedGuild {
            defaults.set(guild.id, forKey: "lastGuildId")
            defaults.set(guild.name, forKey: "lastGuildName")
        }
        if let channel = selectedChannel {
            defaults.set(channel.id, forKey: "lastChannelId")
            defaults.set(channel.name, forKey: "lastChannelName")
        }
    }

    // MARK: - Private

    private func handleDaemonEvent(_ type: String, _ payload: Data?) {
        if type != "stats" { log.debug("daemon event: \(type)") }
        switch type {
        case "connected":
            streamState = .streaming
        case "disconnected":
            streamState = .idle
            Task { await captureSession.stop() }
        case "reconnecting":
            if let data = payload,
               let obj = try? JSONDecoder().decode([String: Int].self, from: data),
               let attempt = obj["attempt"] {
                streamState = .reconnecting(attempt)
            }
        case "stats":
            if let data = payload, let s = try? JSONDecoder().decode(StatsPayload.self, from: data) {
                stats = s
            }
        case "error":
            if let data = payload, let msg = String(data: data, encoding: .utf8) {
                streamState = .error(msg.trimmingCharacters(in: CharacterSet(charactersIn: "\"")))
            }
        default:
            break
        }
    }

    private func frameDist(_ a: CGRect, _ b: CGRect) -> CGFloat {
        let dx = a.origin.x - b.origin.x
        let dy = a.origin.y - b.origin.y
        let dw = a.width - b.width
        let dh = a.height - b.height
        return dx*dx + dy*dy + dw*dw + dh*dh
    }

    private func discordAPI(_ path: String) async throws -> (Data, URLResponse) {
        let url = URL(string: "https://discord.com/api/v9/\(path)")!
        var req = URLRequest(url: url)
        req.setValue(token, forHTTPHeaderField: "Authorization")
        return try await URLSession.shared.data(for: req)
    }

    // MARK: - Crop persistence

    private func cropKey() -> String? {
        guard let filter = captureFilter else { return nil }
        if #available(macOS 15.2, *), let window = filter.includedWindows.first {
            let bundleId = window.owningApplication?.bundleIdentifier ?? "unknown"
            let title = window.title ?? ""
            return "crop_\(bundleId)_\(title)"
        }
        return nil
    }

    func persistCrop() {
        guard let key = cropKey() else { return }
        if let rect = cropRect {
            let arr = [rect.origin.x, rect.origin.y, rect.width, rect.height]
            defaults.set(arr, forKey: key)
        } else {
            defaults.removeObject(forKey: key)
        }
    }

    func loadCropForCurrentWindow() {
        guard let key = cropKey() else { cropRect = nil; return }
        if let arr = defaults.array(forKey: key) as? [CGFloat], arr.count == 4 {
            cropRect = CGRect(x: arr[0], y: arr[1], width: arr[2], height: arr[3])
        } else {
            cropRect = nil
        }
    }

    private func loadToken() {
        let query = Process()
        query.executableURL = URL(fileURLWithPath: "/usr/bin/security")
        query.arguments = ["find-generic-password", "-s", "dstreamy", "-a", "discord-token", "-w"]
        let pipe = Pipe()
        query.standardOutput = pipe
        query.standardError = Pipe()
        try? query.run()
        query.waitUntilExit()
        if query.terminationStatus == 0 {
            token = String(data: pipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8)?
                .trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        }
    }
}
