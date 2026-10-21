import AppKit
import Foundation
import os

private let log = Logger(subsystem: "me.harke.d-streamy", category: "app")

func normalizeDiscordToken(_ value: String) -> String {
    let value = value.trimmingCharacters(in: .whitespacesAndNewlines)
    guard value.lowercased().hasPrefix("authorization:") else { return value }
    return String(value.dropFirst("authorization:".count))
        .trimmingCharacters(in: .whitespacesAndNewlines)
}

/// Single source of truth for the app.
@MainActor
final class AppState: ObservableObject {
    @Published var daemonError: String?
    @Published var isAuthenticated = false
    @Published var authError: String?

    @Published var guilds: [GuildInfo] = []
    @Published var channels: [ChannelInfo] = []
    @Published var selectedGuild: GuildInfo?
    @Published var selectedChannel: ChannelInfo?

    @Published var targets: [CaptureTarget] = []
    @Published var selectedTarget: CaptureTarget?

    // Overlay points, bottom-left origin, over a window `cropViewSize` points large.
    @Published var cropRect: CGRect?
    private var cropViewSize: CGSize?
    @Published var isCropOverlayVisible = false

    @Published var streamState: StreamState = .idle
    @Published var health: HealthSnapshot?
    @Published var streamStartedAt: Date?

    @Published var healthLevel: HealthLevel = .good
    @Published var menuDotVisible = true
    private var blinkTimer: Timer?

    @Published var maxWidth: Int { didSet { defaults.set(maxWidth, forKey: "maxWidth") } }
    @Published var maxHeight: Int { didSet { defaults.set(maxHeight, forKey: "maxHeight") } }
    @Published var fps: Int { didSet { defaults.set(fps, forKey: "fps") } }
    @Published var bitrateMbps: Double { didSet { defaults.set(bitrateMbps, forKey: "bitrateMbps") } }
    @Published var audioGain: Float { didSet { defaults.set(audioGain, forKey: "audioGain") } }
    @Published var videoCodec: String { didSet { defaults.set(videoCodec, forKey: "videoCodec") } }

    private var daemon: DaemonClient?
    private var launching: Task<DaemonClient?, Never>?
    private var startInFlight = false
    private var cancelRequested = false
    private var cropSentForStart = false
    private let defaults = UserDefaults.standard
    let cropOverlayController = CropOverlayController()
    private var cropUpdateTask: Task<Void, Never>?

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
        maxWidth = defaults.object(forKey: "maxWidth") as? Int ?? 1920
        maxHeight = defaults.object(forKey: "maxHeight") as? Int ?? 1080
        fps = defaults.object(forKey: "fps") as? Int ?? 30
        bitrateMbps = defaults.object(forKey: "bitrateMbps") as? Double ?? 6.0
        audioGain = defaults.object(forKey: "audioGain") as? Float ?? 1.0
        videoCodec = defaults.string(forKey: "videoCodec") ?? "H264"

        if let gId = defaults.string(forKey: "lastGuildId"),
           let gName = defaults.string(forKey: "lastGuildName") {
            selectedGuild = GuildInfo(id: gId, name: gName)
        }
        if let cId = defaults.string(forKey: "lastChannelId"),
           let cName = defaults.string(forKey: "lastChannelName") {
            selectedChannel = ChannelInfo(id: cId, name: cName)
        }

        cropOverlayController.onCropChanged = { [weak self] rect, size in
            self?.cropChanged(rect, viewSize: size)
        }
        cropOverlayController.onApply = { [weak self] in
            self?.cropOverlayController.hide()
            self?.isCropOverlayVisible = false
        }

        Task { await bootstrap() }
    }

    var canCropSelectedContent: Bool {
        selectedTarget?.windowID != nil
    }

    func bootstrap() async {
        guard let daemon = await ensureDaemon() else { return }
        do {
            let status: LoggedInInfo = try await daemon.request(.loggedIn, timeout: .seconds(10))
            isAuthenticated = status.loggedIn
            if !isAuthenticated, let legacy = await Task.detached(operation: LegacyKeychainToken.read).value {
                log.info("migrating the Keychain token to the daemon")
                try await daemon.request(.setToken(legacy), as: Empty.self, timeout: .seconds(30))
                await Task.detached(operation: LegacyKeychainToken.delete).value
                isAuthenticated = true
            }
        } catch {
            authError = error.localizedDescription
        }
        await refreshTargets()
        if isAuthenticated { await fetchGuilds() }
    }

    private func ensureDaemon() async -> DaemonClient? {
        if let daemon, daemon.isRunning { return daemon }
        if let launching { return await launching.value }
        let task = Task { await launchDaemon() }
        launching = task
        let client = await task.value
        launching = nil
        return client
    }

    private func launchDaemon() async -> DaemonClient? {
        guard let url = DaemonBinary.locate() else {
            daemonError = "dstreamy not found. Build it with `cargo build -p dstreamy-cli` or set DSTREAMY_BIN."
            return nil
        }
        let client = DaemonClient(executableURL: url)
        client.onEvent = { [weak self] event in
            DispatchQueue.main.async { MainActor.assumeIsolated { self?.handle(event) } }
        }
        client.onExit = { [weak self, weak client] status in
            DispatchQueue.main.async { MainActor.assumeIsolated { self?.daemonExited(client, status: status) } }
        }
        do {
            try client.launch()
            try await client.handshake()
        } catch {
            client.terminate()
            daemonError = "Could not start dstreamy: \(error.localizedDescription)"
            return nil
        }
        daemon = client
        daemonError = nil
        return client
    }

    private func daemonExited(_ client: DaemonClient?, status: Int32) {
        guard let client, client === daemon else { return }
        daemon = nil
        if streamState.canStop {
            streamState = .error("dstreamy exited (status \(status))")
            endStream()
        }
    }

    func fetchGuilds() async {
        guard isAuthenticated, let daemon = await ensureDaemon() else { return }
        do {
            guilds = try await daemon.request(.guilds, timeout: .seconds(30))
            if let saved = selectedGuild, guilds.contains(where: { $0.id == saved.id }) {
                await fetchChannels(guildId: saved.id)
            }
        } catch {
            authError = error.localizedDescription
        }
    }

    func fetchChannels(guildId: String) async {
        guard isAuthenticated, let daemon = await ensureDaemon() else { return }
        do {
            let all: [ChannelInfo] = try await daemon.request(.channels(guildId: guildId), timeout: .seconds(30))
            channels = all.sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
        } catch {
            channels = []
            authError = error.localizedDescription
        }
    }

    func saveToken(_ newToken: String) async {
        let token = normalizeDiscordToken(newToken)
        guard !token.isEmpty else {
            authError = "Paste an Authorization header value."
            return
        }
        guard let daemon = await ensureDaemon() else { return }
        do {
            try await daemon.request(.setToken(token), as: Empty.self, timeout: .seconds(30))
            isAuthenticated = true
            authError = nil
            await fetchGuilds()
        } catch {
            isAuthenticated = false
            guilds = []
            channels = []
            authError = error.localizedDescription
        }
    }

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

    func refreshTargets() async {
        guard let daemon = await ensureDaemon() else { return }
        do {
            targets = try await daemon.request(.targets, timeout: .seconds(15))
        } catch {
            log.error("targets failed: \(error.localizedDescription, privacy: .public)")
            return
        }
        if let current = selectedTarget {
            if let fresh = targets.first(where: { $0.id == current.id }) { selectedTarget = fresh }
        } else if let restored = restoredTarget(from: targets) {
            selectTarget(restored)
        }
    }

    func selectTarget(_ target: CaptureTarget) {
        if isCropOverlayVisible {
            cropOverlayController.hide()
            isCropOverlayVisible = false
        }
        selectedTarget = target
        defaults.set(target.id, forKey: "lastTargetId")
        defaults.set(target.app ?? "", forKey: "lastTargetApp")
        defaults.set(target.title, forKey: "lastTargetTitle")
        loadCrop()
    }

    private func restoredTarget(from targets: [CaptureTarget]) -> CaptureTarget? {
        guard let id = defaults.string(forKey: "lastTargetId") else { return nil }
        if let exact = targets.first(where: { $0.id == id }) { return exact }
        guard id.hasPrefix("window:"), let app = defaults.string(forKey: "lastTargetApp"), !app.isEmpty else { return nil }
        let title = defaults.string(forKey: "lastTargetTitle") ?? ""
        let sameApp = targets.filter { $0.kind == .window && $0.app == app }
        return sameApp.first { $0.title == title } ?? sameApp.first
    }

    func startStream() async {
        guard selectedTarget != nil,
              let guild = selectedGuild,
              let channel = selectedChannel,
              isAuthenticated else { return }
        guard let daemon = await ensureDaemon() else {
            streamState = .error(daemonError ?? "dstreamy is not running")
            return
        }

        streamState = .connecting
        health = nil
        streamStartedAt = nil
        startInFlight = true
        cancelRequested = false
        cropSentForStart = true
        defer { startInFlight = false }
        await refreshTargets()
        guard !cancelRequested, let target = selectedTarget else { return }
        let request = StartRequest(
            guildId: guild.id,
            channelId: channel.id,
            target: target.id,
            crop: pixelCrop(for: target),
            codec: videoCodec.lowercased(),
            fps: fps,
            resolution: "\(maxWidth)x\(maxHeight)",
            bitrateKbps: Int((bitrateMbps * 1000).rounded()),
            gain: Double(audioGain)
        )
        log.info("start target=\(target.id, privacy: .public) guild=\(guild.id, privacy: .public) channel=\(channel.id, privacy: .public)")
        do {
            try await daemon.request(.start(request), as: Empty.self, timeout: .seconds(60))
            if cancelRequested {
                _ = try? await daemon.send(.stop, timeout: .seconds(15))
            }
        } catch {
            guard !cancelRequested, streamState.canStop else { return }
            streamState = .error(error.localizedDescription)
            endStream()
        }
    }

    func stopStream(timeout: Duration = .seconds(15)) async {
        if startInFlight { cancelRequested = true }
        if let daemon, daemon.isRunning {
            do {
                let stats: StreamStats = try await daemon.request(.stop, timeout: timeout)
                log.info("stopped sent=\(stats.videoFramesSent) dropped=\(stats.videoFramesDropped)")
            } catch {
                log.error("stop failed: \(error.localizedDescription, privacy: .public)")
            }
        }
        streamState = .idle
        endStream()
    }

    func shutdownForQuit() async {
        if isCropOverlayVisible {
            cropOverlayController.hide()
            isCropOverlayVisible = false
        }
        if streamState.canStop { await stopStream(timeout: .seconds(5)) }
        await daemon?.shutdown()
        daemon = nil
    }

    private func endStream() {
        cropUpdateTask?.cancel()
        health = nil
        streamStartedAt = nil
        resetHealth()
    }

    func handle(_ event: SessionEvent) {
        switch event {
        case .state(let state):
            log.info("session state \(String(describing: state), privacy: .public)")
            guard !cancelRequested else { return }
            switch state {
            case .idle:
                if streamState.canStop { streamState = .idle; endStream() }
            case .connecting:
                streamState = .connecting
            case .streaming:
                streamState = .streaming
                if streamStartedAt == nil { streamStartedAt = Date() }
                if !cropSentForStart {
                    cropSentForStart = true
                    sendCropUpdate()
                }
            case .reconnecting(let attempt):
                streamState = .reconnecting(attempt)
            case .stopped(.requested):
                streamState = .idle
                endStream()
            case .stopped(let reason):
                streamState = .error(reason.message)
                endStream()
            }
        case .health(let snapshot):
            guard streamState.canStop else { return }
            health = snapshot
            healthLevel = snapshot.health.overallLevel
            updateBlink()
        case .channelMoved(let guildId, let channelId):
            log.info("moved to channel \(channelId, privacy: .public)")
            if let moved = channels.first(where: { $0.id == channelId }), guildId == nil || guildId == selectedGuild?.id {
                selectedChannel = moved
                persistConfig()
            }
        case .resumed:
            log.info("connection resumed")
        case .error(let message, let fatal):
            log.error("session error fatal=\(fatal) \(message, privacy: .public)")
            if fatal {
                streamState = .error(message)
                endStream()
            }
        }
    }

    private func updateBlink() {
        if healthLevel == .bad {
            guard blinkTimer == nil else { return }
            let timer = Timer(timeInterval: 0.5, repeats: true) { [weak self] _ in
                Task { @MainActor in self?.menuDotVisible.toggle() }
            }
            RunLoop.main.add(timer, forMode: .common)
            blinkTimer = timer
        } else {
            blinkTimer?.invalidate()
            blinkTimer = nil
            menuDotVisible = true
        }
    }

    private func resetHealth() {
        blinkTimer?.invalidate()
        blinkTimer = nil
        healthLevel = .good
        menuDotVisible = true
    }

    func toggleCropOverlay() {
        if isCropOverlayVisible {
            cropOverlayController.hide()
            isCropOverlayVisible = false
            return
        }
        guard let target = selectedTarget, let windowID = target.windowID else { return }
        if let pid = windowOwnerPID(windowID), let app = NSRunningApplication(processIdentifier: pid) {
            app.activate()
        }
        isCropOverlayVisible = cropOverlayController.show(windowID: windowID, currentCrop: cropRect)
    }

    private func cropChanged(_ rect: CGRect, viewSize: CGSize) {
        cropRect = rect
        cropViewSize = viewSize
        persistCrop()
        if streamState == .connecting { cropSentForStart = false }
        guard streamState.isActive else { return }
        sendCropUpdate()
    }

    private func sendCropUpdate() {
        cropUpdateTask?.cancel()
        cropUpdateTask = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(300))
            guard !Task.isCancelled, let self, let daemon = self.daemon else { return }
            await self.refreshTargets()
            guard !Task.isCancelled, let target = self.selectedTarget, let crop = self.pixelCrop(for: target) else { return }
            do {
                try await daemon.request(.update(UpdateRequest(crop: crop)), as: Empty.self, timeout: .seconds(10))
            } catch {
                log.error("crop update failed: \(error.localizedDescription, privacy: .public)")
            }
        }
    }

    private func pixelCrop(for target: CaptureTarget) -> CropRect? {
        guard target.kind == .window, let rect = cropRect, let size = cropViewSize else { return nil }
        return sourcePixelCrop(viewRect: rect, viewSize: size, sourceWidth: target.width, sourceHeight: target.height)
    }

    private func cropKey() -> String? {
        guard let target = selectedTarget, target.kind == .window else { return nil }
        return "crop_\(target.app ?? "unknown")_\(target.title)"
    }

    private func persistCrop() {
        guard let key = cropKey() else { return }
        if let rect = cropRect, let size = cropViewSize {
            defaults.set([rect.origin.x, rect.origin.y, rect.width, rect.height, size.width, size.height], forKey: key)
        } else {
            defaults.removeObject(forKey: key)
        }
    }

    private func loadCrop() {
        guard let key = cropKey(),
              let arr = defaults.array(forKey: key) as? [CGFloat], arr.count == 6 else {
            cropRect = nil
            cropViewSize = nil
            return
        }
        cropRect = CGRect(x: arr[0], y: arr[1], width: arr[2], height: arr[3])
        cropViewSize = CGSize(width: arr[4], height: arr[5])
    }

    private func windowOwnerPID(_ windowID: CGWindowID) -> pid_t? {
        let list = CGWindowListCopyWindowInfo([.optionIncludingWindow], windowID) as? [[String: Any]]
        return (list?.first?[kCGWindowOwnerPID as String] as? NSNumber).map { pid_t($0.int32Value) }
    }
}

/// The token older app versions kept in the Keychain; moved to the daemon once, then deleted.
enum LegacyKeychainToken {
    static func read() -> String? {
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/usr/bin/security")
        proc.arguments = ["find-generic-password", "-s", "dstreamy", "-a", "discord-token", "-w"]
        let pipe = Pipe()
        proc.standardOutput = pipe
        proc.standardError = Pipe()
        guard (try? proc.run()) != nil else { return nil }
        proc.waitUntilExit()
        guard proc.terminationStatus == 0 else { return nil }
        let token = String(decoding: pipe.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        return token.isEmpty ? nil : token
    }

    static func delete() {
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/usr/bin/security")
        proc.arguments = ["delete-generic-password", "-s", "dstreamy", "-a", "discord-token"]
        proc.standardOutput = Pipe()
        proc.standardError = Pipe()
        guard (try? proc.run()) != nil else { return }
        proc.waitUntilExit()
    }
}
