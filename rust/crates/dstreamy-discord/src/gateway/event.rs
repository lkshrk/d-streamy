use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::GatewayError;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CurrentUser {
    pub id: String,
    pub username: String,
    #[serde(default)]
    pub global_name: Option<String>,
}

/// One op 0 DISPATCH as received.
#[derive(Debug, Clone)]
pub struct Dispatch {
    pub name: String,
    pub seq: Option<u64>,
    pub data: Arc<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct VoiceState {
    #[serde(default)]
    pub guild_id: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    pub user_id: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub self_mute: bool,
    #[serde(default)]
    pub self_deaf: bool,
    #[serde(default)]
    pub self_video: bool,
    #[serde(default)]
    pub self_stream: bool,
}

#[derive(Debug, Clone)]
pub enum GatewayEvent {
    Ready {
        user: CurrentUser,
        session_id: String,
    },
    Resumed,
    VoiceStateUpdate(VoiceState),
    /// The guild/voice channel cache changed.
    GuildsChanged,
    Dispatch(Dispatch),
    InvalidSession {
        resumable: bool,
    },
    /// The socket dropped; the client reconnects (resuming if `resume`).
    Disconnected {
        code: Option<u16>,
        resume: bool,
    },
    /// The client stopped for good.
    Closed(GatewayError),
}
