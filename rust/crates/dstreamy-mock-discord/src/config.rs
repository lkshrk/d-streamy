use crate::depacketize::VideoCodec;

/// Channel types used by the mock, with Discord's numeric values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    Text = 0,
    Voice = 2,
    Stage = 13,
}

#[derive(Debug, Clone)]
pub struct MockChannel {
    pub id: String,
    pub name: String,
    pub kind: ChannelKind,
}

impl MockChannel {
    pub fn voice(id: &str, name: &str) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            kind: ChannelKind::Voice,
        }
    }

    pub fn text(id: &str, name: &str) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            kind: ChannelKind::Text,
        }
    }

    pub fn is_voice(&self) -> bool {
        matches!(self.kind, ChannelKind::Voice | ChannelKind::Stage)
    }
}

#[derive(Debug, Clone)]
pub struct MockGuild {
    pub id: String,
    pub name: String,
    pub channels: Vec<MockChannel>,
    /// `false`: READY lists the guild as unavailable and a GUILD_CREATE follows.
    pub in_ready: bool,
}

#[derive(Debug, Clone)]
pub struct MockUser {
    pub id: String,
    pub username: String,
    pub global_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MockConfig {
    /// The only user token IDENTIFY and RESUME accept.
    pub token: String,
    pub user: MockUser,
    pub guilds: Vec<MockGuild>,
    pub heartbeat_interval_ms: u64,
    pub voice_heartbeat_interval_ms: u64,
    /// Highest DAVE version the voice server negotiates; 0 disables E2EE.
    pub dave_protocol_version: u16,
    /// User id of the simulated call member the client adds to its MLS group.
    pub dave_participant_id: u64,
    /// `video_codec` in SELECT_PROTOCOL_ACK; the media server only accepts this codec.
    pub video_codec: VideoCodec,
}

pub const MOCK_TOKEN: &str = "mock-user-token";
pub const MOCK_USER_ID: &str = "100000000000000001";
pub const MOCK_GUILD_ID: &str = "200000000000000001";
pub const MOCK_VOICE_CHANNEL_ID: &str = "300000000000000001";
pub const MOCK_OTHER_VOICE_CHANNEL_ID: &str = "300000000000000002";
pub const MOCK_TEXT_CHANNEL_ID: &str = "300000000000000003";

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            token: MOCK_TOKEN.to_owned(),
            user: MockUser {
                id: MOCK_USER_ID.to_owned(),
                username: "mockuser".to_owned(),
                global_name: Some("Mock User".to_owned()),
            },
            guilds: vec![MockGuild {
                id: MOCK_GUILD_ID.to_owned(),
                name: "Mock Guild".to_owned(),
                channels: vec![
                    MockChannel::voice(MOCK_VOICE_CHANNEL_ID, "General"),
                    MockChannel::voice(MOCK_OTHER_VOICE_CHANNEL_ID, "Gaming"),
                    MockChannel::text(MOCK_TEXT_CHANNEL_ID, "general"),
                ],
                in_ready: true,
            }],
            heartbeat_interval_ms: 41_250,
            voice_heartbeat_interval_ms: 13_750,
            dave_protocol_version: davey::DAVE_PROTOCOL_VERSION,
            dave_participant_id: 100_000_000_000_009_999,
            video_codec: VideoCodec::H264,
        }
    }
}

impl MockConfig {
    pub fn guild(&self, id: &str) -> Option<&MockGuild> {
        self.guilds.iter().find(|g| g.id == id)
    }

    pub fn voice_channel(&self, guild_id: &str, channel_id: &str) -> Option<&MockChannel> {
        self.guild(guild_id)?
            .channels
            .iter()
            .find(|c| c.id == channel_id && c.is_voice())
    }
}
