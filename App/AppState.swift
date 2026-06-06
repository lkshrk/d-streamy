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
    @Published var captureKind: CaptureKind = .window
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
    private var captureStartTask: Task<Void, Never>?
    private var captureStartWatchdogTask: Task<Void, Never>?
    private var connectWatchdogTask: Task<Void, Never>?
    private let streamLog = StreamFileLogger.shared
    private let captureMetricsLog = Logger(subsystem: "me.harke.d-streamy", category: "capture")
    private let streamMetricsLog = Logger(subsystem: "me.harke.d-streamy", category: "stream")
    private let healthLog = Logger(subsystem: "me.harke.d-streamy", category: "health")
    private var sessionId = ""
    private var metricsTimer: Timer?
    private var metricsStart = Date()

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

        var canStop: Bool {
            switch self {
            case .connecting, .streaming, .reconnecting: return true
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
        Task { await restoreCaptureSelection() }
    }

    var canCropSelectedContent: Bool {
        guard captureFilter != nil else { return false }
        return captureKind == .window
    }

    private func restoreCaptureSelection() async {
        if defaults.string(forKey: "lastCaptureKind") == CaptureKind.display.rawValue,
           await restoreDisplay() {
            return
        }

        await restoreWindow()
    }

    private func restoreDisplay() async -> Bool {
        let savedDisplayID = CGDirectDisplayID(defaults.integer(forKey: "lastDisplayID"))
        guard savedDisplayID != 0 else { return false }

        do {
            let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
            guard let display = content.displays.first(where: { $0.displayID == savedDisplayID }) else {
                log.info("restoreDisplay: no match for \(savedDisplayID)")
                return false
            }

            let filter = SCContentFilter(display: display, excludingApplications: [], exceptingWindows: [])
            captureFilter = filter
            captureKind = .display
            cropRect = nil
            captureLabel = displayCaptureLabel(for: display, filter: filter)
            log.info("restoreDisplay: auto-selected \(self.captureLabel)")
            streamLog.write(component: "app", "restore display: \(captureLabel)")
            return true
        } catch {
            log.error("restoreDisplay failed: \(error.localizedDescription)")
            return false
        }
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
            captureKind = .window
            let app = window.owningApplication?.applicationName ?? savedApp
            let title = window.title ?? savedTitle
            captureLabel = app.isEmpty ? title : "\(app) — \(title)"
            loadCropForCurrentWindow()
            log.info("restoreWindow: auto-selected \(app) — \(title)")
            streamLog.write(component: "app", "restore window: \(captureLabel)")
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
            streamLog.write(component: "app", "start guard failed filter=\(captureFilter != nil) guild=\(selectedGuild != nil) channel=\(selectedChannel != nil) token=\(!token.isEmpty)")
            return
        }

        log.info("startStream: resetting stale sharing session")
        streamLog.write(component: "app", "start: reset stale sharing session")
        cancelStreamTasks()
        await resetSharingSessionBeforeStart()

        streamState = .connecting
        log.info("startStream: connecting")
        streamLog.write(component: "app", "start: connecting source=\(captureLabel) kind=\(captureKind.rawValue)")

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
                    streamLog.write(component: "app", "start: daemon failed to connect")
                    return
                }
            } catch {
                streamState = .error("Failed to start daemon: \(error.localizedDescription)")
                streamLog.write(component: "app", "start: daemon start failed: \(error.localizedDescription)")
                return
            }
        }
        log.info("startStream: daemon connected")
        streamLog.write(component: "app", "start: daemon connected")

        guard let mediaFd = daemon.mediaFd else {
            streamState = .error("No media pipe available")
            streamLog.write(component: "app", "start: no media pipe")
            return
        }

        let rect = filter.contentRect
        let scale = filter.pointPixelScale
        let sourceW = Int(rect.width * CGFloat(scale))
        let sourceH = Int(rect.height * CGFloat(scale))
        log.debug("startStream: contentRect=\(rect.debugDescription) scale=\(scale) source=\(sourceW)x\(sourceH)")
        streamLog.write(component: "app", "start: source=\(sourceW)x\(sourceH) max=\(maxWidth)x\(maxHeight) fps=\(fps) bitrate=\(bitrateMbps)")

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

        sessionId = UUID().uuidString
        metricsStart = Date()
        healthLog.info("session start session=\(self.sessionId, privacy: .public)")

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

        startCaptureThenConnect(
            config: config,
            token: token,
            guildId: guild.id,
            channelId: channel.id,
            fps: fps
        )
    }

    private func resetSharingSessionBeforeStart() async {
        // Keep the picker active here: the selected SCContentFilter can rely on
        // picker-backed authorization, especially for system audio.
        await captureSession.stop()

        if daemon.isConnected {
            daemon.send(.disconnect)
            try? await Task.sleep(for: .milliseconds(500))
        }
        daemon.stop()
    }

    private func startCaptureThenConnect(
        config: CaptureConfig,
        token: String,
        guildId: String,
        channelId: String,
        fps: Int
    ) {
        captureStartTask?.cancel()
        captureStartWatchdogTask?.cancel()

        log.info("startStream: starting capture")

        captureStartTask = Task { [weak self] in
            guard let self else { return }

            do {
                try await self.captureSession.start(config: config)
            } catch {
                guard !Task.isCancelled else { return }
                log.error("startStream: capture failed: \(error.localizedDescription)")
                self.streamLog.write(component: "app", "start: capture failed: \(error.localizedDescription)")
                await self.failStart("Capture failed: \(error.localizedDescription)")
                return
            }

            guard !Task.isCancelled else { return }
            guard self.streamState == .connecting, self.daemon.isConnected else { return }

            self.captureStartWatchdogTask?.cancel()
            log.info("startStream: capture started \(self.captureSession.captureWidth)x\(self.captureSession.captureHeight)")
            self.streamLog.write(component: "app", "start: capture started \(self.captureSession.captureWidth)x\(self.captureSession.captureHeight)")

            self.daemon.send(.connect(.init(
                token: token,
                guildId: guildId,
                channelId: channelId,
                width: self.captureSession.captureWidth,
                height: self.captureSession.captureHeight,
                fps: fps,
                session: self.sessionId
            )))
            log.info("startStream: connect command sent")
            self.streamLog.write(component: "app", "start: connect command sent")
            self.startConnectWatchdog()
        }

        captureStartWatchdogTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(12))
            guard !Task.isCancelled else { return }
            await self?.failStartIfStillConnecting("Timed out starting screen capture")
        }
    }

    private func startConnectWatchdog() {
        connectWatchdogTask?.cancel()
        connectWatchdogTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(35))
            guard !Task.isCancelled else { return }
            await self?.failStartIfStillConnecting("Timed out connecting to Discord")
        }
    }

    private func failStartIfStillConnecting(_ message: String) async {
        guard streamState == .connecting else { return }
        log.error("startStream: \(message)")
        streamLog.write(component: "app", "start failed: \(message)")
        await failStart(message)
    }

    private func failStart(_ message: String) async {
        cancelStreamTasks()
        SCContentSharingPicker.shared.isActive = false
        daemon.send(.disconnect)
        await captureSession.stop()
        try? await Task.sleep(for: .milliseconds(250))
        daemon.stop()
        streamState = .error(message)
    }

    private func cancelStreamTasks() {
        captureStartTask?.cancel()
        captureStartTask = nil
        captureStartWatchdogTask?.cancel()
        captureStartWatchdogTask = nil
        connectWatchdogTask?.cancel()
        connectWatchdogTask = nil
    }

    // MARK: - Crop overlay

    func toggleCropOverlay() {
        if isCropOverlayVisible {
            cropOverlayController.hide()
            isCropOverlayVisible = false
        } else {
            guard canCropSelectedContent, let filter = captureFilter else { return }
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
            streamLog.write(component: "app", "switch source: \(captureLabel) \(captureSession.captureWidth)x\(captureSession.captureHeight)")
        } catch {
            log.error("switchCaptureSource failed: \(error.localizedDescription)")
            streamLog.write(component: "app", "switch source failed: \(error.localizedDescription)")
        }
    }

    func stopStream() async {
        streamLog.write(component: "app", "stop requested")
        cancelStreamTasks()
        SCContentSharingPicker.shared.isActive = false
        daemon.send(.disconnect)
        await captureSession.stop()
        // Wait for leave opcode to flush over WS before killing daemon
        try? await Task.sleep(for: .milliseconds(1000))
        daemon.stop()
        streamState = .idle
        stats = StatsPayload(uptime: 0, fps: 0, bitrate: 0, droppedFrames: 0)
    }

    func shutdownForQuit() async {
        streamLog.write(component: "app", "quit cleanup requested")
        cancelStreamTasks()
        SCContentSharingPicker.shared.isActive = false
        if isCropOverlayVisible {
            cropOverlayController.hide()
            isCropOverlayVisible = false
        }

        daemon.send(.disconnect)
        await captureSession.stop()
        try? await Task.sleep(for: .milliseconds(1000))
        daemon.stop()
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

    private func startMetricsTimer() {
        metricsTimer?.invalidate()
        let timer = Timer(timeInterval: 1.0, repeats: true) { [weak self] _ in
            guard let self else { return }
            Task { @MainActor in
                let s = self.captureSession.metrics.snapshot()
                let t = Int(Date().timeIntervalSince(self.metricsStart))
                self.captureMetricsLog.info(
                    "session=\(self.sessionId, privacy: .public) t=\(t, privacy: .public) delivered=\(s.delivered, privacy: .public) encSubmit=\(s.encSubmit, privacy: .public) encDrop=\(s.encDrop, privacy: .public) audioBuf=\(s.audioBuf, privacy: .public) audioConvFail=\(s.audioConvFail, privacy: .public)"
                )
            }
        }
        RunLoop.main.add(timer, forMode: .common)
        metricsTimer = timer
    }

    private func stopMetricsTimer() {
        metricsTimer?.invalidate()
        metricsTimer = nil
    }

    private func handleDaemonEvent(_ type: String, _ payload: Data?) {
        if type != "stats" && type != "metrics" { log.debug("daemon event: \(type)") }
        switch type {
        case "connected":
            cancelStreamTasks()
            streamState = .streaming
            streamLog.write(component: "app", "stream connected")
            healthLog.info("connected session=\(self.sessionId, privacy: .public)")
            startMetricsTimer()
        case "disconnected":
            cancelStreamTasks()
            streamState = .idle
            streamLog.write(component: "app", "stream disconnected")
            healthLog.info("disconnected session=\(self.sessionId, privacy: .public)")
            stopMetricsTimer()
            Task { await captureSession.stop() }
        case "reconnecting":
            if let data = payload,
               let obj = try? JSONDecoder().decode([String: Int].self, from: data),
               let attempt = obj["attempt"] {
                streamState = .reconnecting(attempt)
                streamLog.write(component: "app", "stream reconnecting attempt=\(attempt)")
                healthLog.info("reconnecting session=\(self.sessionId, privacy: .public) attempt=\(attempt, privacy: .public)")
            }
        case "stats":
            if let data = payload, let s = try? JSONDecoder().decode(StatsPayload.self, from: data) {
                stats = s
            }
        case "metrics":
            if let data = payload, let m = try? JSONDecoder().decode(MetricsPayload.self, from: data) {
                streamMetricsLog.info(
                    "session=\(m.session, privacy: .public) t=\(m.t, privacy: .public) fps=\(m.fps, privacy: .public) gapP95=\(m.gapP95Ms, privacy: .public)ms gapMax=\(m.gapMaxMs, privacy: .public)ms gapTrunc=\(m.gapTrunc, privacy: .public) bitrate=\(m.bitrateKbps, privacy: .public)kbps vDrop=\(m.vDrop, privacy: .public) aEnc=\(m.aEnc, privacy: .public) aSent=\(m.aSent, privacy: .public) aDropNotReady=\(m.aDropNotReady, privacy: .public) aEncErr=\(m.aEncErr, privacy: .public) pipeBuf=\(m.pipeBuf, privacy: .public)B audioBuf=\(m.audioBuf, privacy: .public)B rss=\(m.rssMb, privacy: .public)MB"
                )
            }
        case "error":
            cancelStreamTasks()
            if let data = payload, let msg = String(data: data, encoding: .utf8) {
                let clean = msg.trimmingCharacters(in: CharacterSet(charactersIn: "\""))
                streamState = .error(clean)
                streamLog.write(component: "app", "stream error: \(clean)")
                healthLog.error("error session=\(self.sessionId, privacy: .public) msg=\(clean, privacy: .public)")
                stopMetricsTimer()
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
        guard captureKind == .window, let filter = captureFilter else { return nil }
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
