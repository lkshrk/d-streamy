import CoreGraphics
import Foundation

let daemonProtocolVersion = 1

struct CropRect: Codable, Equatable {
    let x: Int
    let y: Int
    let width: Int
    let height: Int
}

struct StartRequest: Codable, Equatable {
    let guildId: String
    let channelId: String
    let target: String
    var crop: CropRect?
    var codec: String?
    var fps: Int?
    var resolution: String?
    var bitrateKbps: Int?
    var gain: Double?
}

struct UpdateRequest: Codable, Equatable {
    var fps: Int?
    var resolution: String?
    var bitrateKbps: Int?
    var crop: CropRect?
}

enum DaemonCommand: Equatable {
    case hello
    case setToken(String)
    case loggedIn
    case guilds
    case channels(guildId: String)
    case targets
    case start(StartRequest)
    case update(UpdateRequest)
    case stop
    case shutdown

    var name: String {
        switch self {
        case .hello: return "hello"
        case .setToken: return "set_token"
        case .loggedIn: return "logged_in"
        case .guilds: return "guilds"
        case .channels: return "channels"
        case .targets: return "targets"
        case .start: return "start"
        case .update: return "update"
        case .stop: return "stop"
        case .shutdown: return "shutdown"
        }
    }
}

struct DaemonRequest: Encodable, Equatable {
    let id: String
    let command: DaemonCommand

    enum Keys: String, CodingKey {
        case id, cmd, token, guildId, channelId, target, crop, codec, fps, resolution, bitrateKbps, gain
    }

    init(id: String, command: DaemonCommand) {
        self.id = id
        self.command = command
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: Keys.self)
        try c.encode(id, forKey: .id)
        try c.encode(command.name, forKey: .cmd)
        switch command {
        case .setToken(let token): try c.encode(token, forKey: .token)
        case .channels(let guildId): try c.encode(guildId, forKey: .guildId)
        case .start(let request): try request.encode(to: encoder)
        case .update(let request): try request.encode(to: encoder)
        case .hello, .loggedIn, .guilds, .targets, .stop, .shutdown: break
        }
    }
}

struct HelloInfo: Codable, Equatable {
    let `protocol`: Int
    let version: String
    let platform: String
}

struct LoggedInInfo: Codable, Equatable {
    let loggedIn: Bool
}

struct GuildInfo: Codable, Identifiable, Hashable {
    let id: String
    let name: String
}

struct ChannelInfo: Codable, Identifiable, Hashable {
    let id: String
    let name: String
}

struct CaptureTarget: Codable, Identifiable, Hashable {
    enum Kind: String, Codable {
        case display
        case window
    }

    let id: String
    let kind: Kind
    let title: String
    let app: String?
    let width: Int
    let height: Int

    var label: String {
        switch kind {
        case .display:
            return "\(title) — \(width)x\(height)"
        case .window:
            guard let app, !app.isEmpty else { return title }
            return title.isEmpty ? app : "\(app) — \(title)"
        }
    }

    var windowID: CGWindowID? {
        guard kind == .window, let raw = id.split(separator: ":").last else { return nil }
        return CGWindowID(raw)
    }
}

enum E2eeState: String, Codable {
    case unknown, encrypted, unencrypted

    init(from decoder: Decoder) throws {
        let value = try decoder.singleValueContainer().decode(String.self)
        self = E2eeState(rawValue: value) ?? .unknown
    }
}

struct EncryptionStatus: Equatable {
    var state: E2eeState = .unknown
    var daveProtocolVersion: Int?

    var title: String {
        switch state {
        case .encrypted: return "End-to-end encrypted"
        case .unencrypted: return "Not end-to-end encrypted"
        case .unknown: return "Encryption status unknown"
        }
    }

    var detail: String {
        if let version = daveProtocolVersion {
            return "DAVE protocol \(version)"
        }
        return "DAVE protocol not reported"
    }

    var warning: String? {
        state == .unencrypted
            ? "This session is not end-to-end encrypted. Discord can read its audio and video."
            : nil
    }
}

struct StreamStats: Codable, Equatable {
    let videoFramesSent: Int
    let videoFramesDropped: Int
    let audioFramesSent: Int
    let bytesSent: Int
    let resumes: Int
    let fps: Double
    let lastError: String?
    var e2ee: E2eeState?
    var daveProtocolVersion: Int?
    let sourceVideoFramesDropped: Int
    let sourceAudioFramesDropped: Int

    var totalVideoFramesDropped: Int { videoFramesDropped + sourceVideoFramesDropped }

    private enum CodingKeys: String, CodingKey {
        case videoFramesSent, videoFramesDropped, audioFramesSent, bytesSent, resumes, fps, lastError
        case e2ee, daveProtocolVersion, sourceVideoFramesDropped, sourceAudioFramesDropped
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        videoFramesSent = try c.decode(Int.self, forKey: .videoFramesSent)
        videoFramesDropped = try c.decode(Int.self, forKey: .videoFramesDropped)
        audioFramesSent = try c.decode(Int.self, forKey: .audioFramesSent)
        bytesSent = try c.decode(Int.self, forKey: .bytesSent)
        resumes = try c.decode(Int.self, forKey: .resumes)
        fps = try c.decode(Double.self, forKey: .fps)
        lastError = try c.decodeIfPresent(String.self, forKey: .lastError)
        e2ee = try c.decodeIfPresent(E2eeState.self, forKey: .e2ee)
        daveProtocolVersion = try c.decodeIfPresent(Int.self, forKey: .daveProtocolVersion)
        sourceVideoFramesDropped = try c.decodeIfPresent(Int.self, forKey: .sourceVideoFramesDropped) ?? 0
        sourceAudioFramesDropped = try c.decodeIfPresent(Int.self, forKey: .sourceAudioFramesDropped) ?? 0
    }
}

struct StreamMetrics: Codable, Equatable {
    let fps: Int
    let gapP95Ms: Int
    let gapMaxMs: Int
    let gapTrunc: Bool
    let bitrateKbps: Int
    let vDrop: Int
    let aEnc: Int
    let aSent: Int
    let aDropNotReady: Int
    let aEncErr: Int
}

enum HealthLevel: String, Codable, Comparable {
    case bad, warn, good

    private var rank: Int {
        switch self {
        case .bad: return 0
        case .warn: return 1
        case .good: return 2
        }
    }

    static func < (a: HealthLevel, b: HealthLevel) -> Bool { a.rank < b.rank }
}

struct StreamHealth: Codable, Equatable {
    let videoScore: Int
    let audioScore: Int
    let videoLevel: HealthLevel
    let audioLevel: HealthLevel
    let fpsSeries: [Double]
    let audioSeries: [Double]

    var overallLevel: HealthLevel { min(videoLevel, audioLevel) }
}

struct HealthSnapshot: Codable, Equatable {
    let metrics: StreamMetrics
    let health: StreamHealth
    let stats: StreamStats
}

enum StopReason: Decodable, Equatable {
    case requested
    case captureEnded
    case gatewayClosed
    case gaveUp(attempts: Int)
    case sourceFailed(message: String)
    case voiceTakenOver
    case e2eeUnavailable
    case identifyLimit(retryAfterSecs: Int?)
    case invalidToken
    case taskEnded
    case unknown(String)

    private enum Keys: String, CodingKey { case reason, attempts, message, retryAfterSecs }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        let reason = try c.decode(String.self, forKey: .reason)
        switch reason {
        case "requested": self = .requested
        case "capture_ended": self = .captureEnded
        case "gateway_closed": self = .gatewayClosed
        case "gave_up": self = .gaveUp(attempts: try c.decode(Int.self, forKey: .attempts))
        case "source_failed": self = .sourceFailed(message: try c.decode(String.self, forKey: .message))
        case "voice_taken_over": self = .voiceTakenOver
        case "e2ee_unavailable": self = .e2eeUnavailable
        case "identify_limit": self = .identifyLimit(retryAfterSecs: try c.decodeIfPresent(Int.self, forKey: .retryAfterSecs))
        case "invalid_token": self = .invalidToken
        case "task_ended": self = .taskEnded
        default: self = .unknown(reason)
        }
    }

    var message: String {
        switch self {
        case .requested: return "Stopped"
        case .captureEnded: return "The capture source ended"
        case .gatewayClosed: return "The Discord connection closed"
        case .gaveUp(let attempts): return "Gave up after \(attempts) reconnect attempts"
        case .sourceFailed(let message): return "Capture failed: \(message)"
        case .voiceTakenOver: return "Another device of this account joined voice"
        case .e2eeUnavailable: return "Session stopped because end-to-end encryption is unavailable. Try another channel or retry later."
        case .identifyLimit(let retryAfterSecs):
            let base = "Discord kept ending the session; D-Streamy stopped logging in again to protect the account."
            guard let retryAfterSecs, retryAfterSecs > 0 else { return base + " Try again later." }
            return base + " Try again in about \(Self.roughDuration(seconds: retryAfterSecs))."
        case .invalidToken: return "Discord rejected the token. Log in again or change the token."
        case .taskEnded: return "The stream stopped unexpectedly. Press Start to go live again."
        case .unknown(let reason): return "Stopped: \(reason)"
        }
    }

    static func roughDuration(seconds: Int) -> String {
        let minutes = max(1, (seconds + 59) / 60)
        let hours = minutes / 60
        if hours == 0 { return "\(minutes) min" }
        return minutes % 60 == 0 ? "\(hours) h" : "\(hours) h \(minutes % 60) min"
    }
}

enum SessionState: Decodable, Equatable {
    case idle
    case connecting
    case streaming
    case reconnecting(attempt: Int)
    case stopped(StopReason)
    case unknown(String)

    private enum Keys: String, CodingKey { case state, attempt, reason }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        let state = try c.decode(String.self, forKey: .state)
        switch state {
        case "idle": self = .idle
        case "connecting": self = .connecting
        case "streaming": self = .streaming
        case "reconnecting": self = .reconnecting(attempt: try c.decode(Int.self, forKey: .attempt))
        case "stopped": self = .stopped(try c.decode(StopReason.self, forKey: .reason))
        default: self = .unknown(state)
        }
    }
}

enum SessionEvent: Decodable, Equatable {
    case state(SessionState)
    case health(HealthSnapshot)
    case channelMoved(guildId: String?, channelId: String)
    case resumed
    case e2ee(state: E2eeState, daveProtocolVersion: Int?)
    case error(message: String, fatal: Bool)
    case unknown(String)

    private enum Keys: String, CodingKey { case event, state, guildId, channelId, message, fatal, daveProtocolVersion }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        let event = try c.decode(String.self, forKey: .event)
        switch event {
        case "state": self = .state(try c.decode(SessionState.self, forKey: .state))
        case "health": self = .health(try HealthSnapshot(from: decoder))
        case "channel_moved":
            self = .channelMoved(
                guildId: try c.decodeIfPresent(String.self, forKey: .guildId),
                channelId: try c.decode(String.self, forKey: .channelId)
            )
        case "resumed": self = .resumed
        case "e2ee":
            self = .e2ee(
                state: try c.decodeIfPresent(E2eeState.self, forKey: .state) ?? .unknown,
                daveProtocolVersion: try c.decodeIfPresent(Int.self, forKey: .daveProtocolVersion)
            )
        case "error":
            self = .error(
                message: try c.decode(String.self, forKey: .message),
                fatal: try c.decode(Bool.self, forKey: .fatal)
            )
        default: self = .unknown(event)
        }
    }
}

enum DaemonMessage: Decodable {
    case result(id: String?, data: JSONValue)
    case error(id: String?, code: String, message: String)
    case event(SessionEvent)

    private enum Keys: String, CodingKey { case type, id, data, code, message, event }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        let type = try c.decode(String.self, forKey: .type)
        switch type {
        case "result":
            self = .result(
                id: try c.decodeIfPresent(String.self, forKey: .id),
                data: try c.decodeIfPresent(JSONValue.self, forKey: .data) ?? .null
            )
        case "error":
            self = .error(
                id: try c.decodeIfPresent(String.self, forKey: .id),
                code: try c.decode(String.self, forKey: .code),
                message: try c.decode(String.self, forKey: .message)
            )
        case "event":
            self = .event(try c.decode(SessionEvent.self, forKey: .event))
        default:
            throw DecodingError.dataCorruptedError(forKey: .type, in: c, debugDescription: "unknown type \(type)")
        }
    }
}

enum JSONValue: Codable, Equatable {
    case null
    case bool(Bool)
    case integer(Int64)
    case number(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])

    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() {
            self = .null
        } else if let b = try? c.decode(Bool.self) {
            self = .bool(b)
        } else if let i = try? c.decode(Int64.self) {
            self = .integer(i)
        } else if let n = try? c.decode(Double.self) {
            self = .number(n)
        } else if let s = try? c.decode(String.self) {
            self = .string(s)
        } else if let a = try? c.decode([JSONValue].self) {
            self = .array(a)
        } else {
            self = .object(try c.decode([String: JSONValue].self))
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .null: try c.encodeNil()
        case .bool(let b): try c.encode(b)
        case .integer(let i): try c.encode(i)
        case .number(let n): try c.encode(n)
        case .string(let s): try c.encode(s)
        case .array(let a): try c.encode(a)
        case .object(let o): try c.encode(o)
        }
    }

    func decode<T: Decodable>(as type: T.Type = T.self) throws -> T {
        try DaemonCoding.decoder.decode(T.self, from: DaemonCoding.encoder.encode(self))
    }
}

struct Empty: Decodable, Equatable {
    init(from decoder: Decoder) throws {
        guard try decoder.singleValueContainer().decodeNil() else {
            throw DecodingError.dataCorrupted(.init(codingPath: decoder.codingPath, debugDescription: "expected null"))
        }
    }
}

enum DaemonCoding {
    static let decoder: JSONDecoder = {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }()

    static let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.keyEncodingStrategy = .convertToSnakeCase
        e.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        return e
    }()
}
