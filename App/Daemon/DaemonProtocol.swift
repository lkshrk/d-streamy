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

struct DaemonRequest: Codable, Equatable {
    let id: String
    let command: DaemonCommand

    private enum Keys: String, CodingKey {
        case id, cmd, token, guildId, channelId, target, crop, codec, fps, resolution, bitrateKbps, gain
    }

    init(id: String, command: DaemonCommand) {
        self.id = id
        self.command = command
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        id = try c.decode(String.self, forKey: .id)
        let cmd = try c.decode(String.self, forKey: .cmd)
        switch cmd {
        case "hello": command = .hello
        case "set_token": command = .setToken(try c.decode(String.self, forKey: .token))
        case "logged_in": command = .loggedIn
        case "guilds": command = .guilds
        case "channels": command = .channels(guildId: try c.decode(String.self, forKey: .guildId))
        case "targets": command = .targets
        case "start": command = .start(try StartRequest(from: decoder))
        case "update": command = .update(try UpdateRequest(from: decoder))
        case "stop": command = .stop
        case "shutdown": command = .shutdown
        default:
            throw DecodingError.dataCorruptedError(forKey: .cmd, in: c, debugDescription: "unknown cmd \(cmd)")
        }
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

struct StreamStats: Codable, Equatable {
    let videoFramesSent: Int
    let videoFramesDropped: Int
    let audioFramesSent: Int
    let bytesSent: Int
    let resumes: Int
    let fps: Double
    let lastError: String?
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

    private enum Keys: String, CodingKey { case reason, attempts, message }

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
        default:
            throw DecodingError.dataCorruptedError(forKey: .reason, in: c, debugDescription: "unknown reason \(reason)")
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
        }
    }
}

enum SessionState: Decodable, Equatable {
    case idle
    case connecting
    case streaming
    case reconnecting(attempt: Int)
    case stopped(StopReason)

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
        default:
            throw DecodingError.dataCorruptedError(forKey: .state, in: c, debugDescription: "unknown state \(state)")
        }
    }
}

enum SessionEvent: Decodable, Equatable {
    case state(SessionState)
    case health(HealthSnapshot)
    case channelMoved(guildId: String?, channelId: String)
    case resumed
    case error(message: String, fatal: Bool)

    private enum Keys: String, CodingKey { case event, state, guildId, channelId, message, fatal }

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
        case "error":
            self = .error(
                message: try c.decode(String.self, forKey: .message),
                fatal: try c.decode(Bool.self, forKey: .fatal)
            )
        default:
            throw DecodingError.dataCorruptedError(forKey: .event, in: c, debugDescription: "unknown event \(event)")
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
    init() {}
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
