import Foundation

// MARK: - Commands (Swift → Node)

enum IPCCommand: Encodable {
    case connect(ConnectPayload)
    case disconnect
    case getGuilds
    case getChannels(guildId: String)
    case updateVideo(UpdateVideoPayload)

    struct ConnectPayload: Codable {
        let token: String
        let guildId: String
        let channelId: String
        let width: Int
        let height: Int
        let fps: Int
    }

    struct UpdateVideoPayload: Codable {
        let width: Int
        let height: Int
        let fps: Int
    }

    private enum CodingKeys: String, CodingKey {
        case type, payload
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .connect(let payload):
            try container.encode("connect", forKey: .type)
            try container.encode(payload, forKey: .payload)
        case .disconnect:
            try container.encode("disconnect", forKey: .type)
        case .getGuilds:
            try container.encode("getGuilds", forKey: .type)
        case .getChannels(let guildId):
            try container.encode("getChannels", forKey: .type)
            try container.encode(["guildId": guildId], forKey: .payload)
        case .updateVideo(let payload):
            try container.encode("updateVideo", forKey: .type)
            try container.encode(payload, forKey: .payload)
        }
    }
}

// MARK: - Events (Node → Swift)

struct IPCEvent: Decodable {
    let type: String
    let payload: EventPayload?
}

enum EventPayload: Decodable {
    case stats(StatsPayload)
    case guilds([GuildInfo])
    case channels([ChannelInfo])
    case error(String)

    init(from decoder: Decoder) throws {
        // Decoded contextually in IPCEvent parsing
        let container = try decoder.singleValueContainer()
        if let stats = try? container.decode(StatsPayload.self) {
            self = .stats(stats)
        } else if let guilds = try? container.decode([GuildInfo].self) {
            self = .guilds(guilds)
        } else if let channels = try? container.decode([ChannelInfo].self) {
            self = .channels(channels)
        } else if let msg = try? container.decode(String.self) {
            self = .error(msg)
        } else {
            self = .error("Unknown payload")
        }
    }
}

struct StatsPayload: Codable {
    let uptime: Int
    let fps: Int
    let bitrate: Int
    let droppedFrames: Int
}

struct GuildInfo: Codable, Identifiable, Hashable {
    let id: String
    let name: String
}

struct ChannelInfo: Codable, Identifiable, Hashable {
    let id: String
    let name: String
}
