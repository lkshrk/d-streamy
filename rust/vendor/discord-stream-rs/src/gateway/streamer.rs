use crate::gateway::events::GatewayEvent;
use crate::gateway::opcodes::GatewayOpCode;
use crate::utils::{StreamType, generate_stream_key, parse_stream_key};
use crate::voice::connection::{
    ConnectionError, MediaConnection, ReadyReceiver, VoiceConnection, VoiceEvent,
};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tracing::debug;
use webrtc::api::setting_engine::SettingEngine;

#[derive(Debug, Error)]
pub enum StreamerError {
    #[error("Not in voice channel")]
    NotInVoice,
    #[error("No session yet")]
    NoSession,
    #[error("Connection error: {0}")]
    Connection(#[from] ConnectionError),
}

/// Outbound opcode for the main gateway; the caller does the actual WS write.
#[derive(Debug, Clone, PartialEq)]
pub struct GatewayPayload {
    pub op: u8,
    pub d: Value,
}

/// A voice or stream connection being set up.
pub struct PendingConnection {
    ready: Option<ReadyReceiver>,
    /// Lifecycle events of this connection.
    pub events: mpsc::UnboundedReceiver<VoiceEvent>,
}

impl PendingConnection {
    /// Wait until media can be sent (after SELECT_PROTOCOL_ACK); cancel-safe, `Closed` once consumed.
    pub async fn ready(&mut self) -> Result<MediaConnection, StreamerError> {
        let Some(ready) = self.ready.as_mut() else {
            return Err(StreamerError::Connection(ConnectionError::Closed));
        };
        let result = ready.await;
        self.ready = None;
        match result {
            Ok(result) => Ok(result?),
            Err(_) => Err(StreamerError::Connection(ConnectionError::Closed)),
        }
    }
}

type ReadySender = oneshot::Sender<Result<MediaConnection, ConnectionError>>;

/// A connection that starts once session id and server tokens are both known.
struct Slot {
    pending: Option<Box<VoiceConnection>>,
    ready_tx: Option<ReadySender>,
    started: Option<MediaConnection>,
}

impl Slot {
    fn new(conn: VoiceConnection, ready_tx: ReadySender) -> Self {
        Self {
            pending: Some(Box::new(conn)),
            ready_tx: Some(ready_tx),
            started: None,
        }
    }

    fn set_session(&mut self, session_id: String) {
        if let Some(c) = &mut self.pending {
            c.set_session(session_id);
        } else if let Some(m) = &self.started {
            m.set_session(session_id);
        }
    }

    fn set_tokens(&mut self, server: String, token: String) {
        if let Some(c) = &mut self.pending {
            c.set_tokens(server, token);
        } else if let Some(m) = &self.started {
            m.set_tokens(server, token);
        }
    }

    fn start_if_ready(&mut self, webrtc: &WebRtcConfig) {
        if !self.pending.as_ref().is_some_and(|c| c.can_start()) {
            return;
        }
        let Some(mut conn) = self.pending.take() else {
            return;
        };
        webrtc.apply(&mut conn);
        let (media, ready, _task) = conn.start();
        if let Some(tx) = self.ready_tx.take() {
            tokio::spawn(async move {
                let _ = tx.send(ready.await.unwrap_or(Err(ConnectionError::Closed)));
            });
        }
        self.started = Some(media);
    }

    fn stop(&self) {
        if let Some(m) = &self.started {
            m.stop();
        }
    }
}

#[derive(Clone, Default)]
struct WebRtcConfig {
    ice_servers: Option<Vec<String>>,
    setting_engine: Option<SettingEngine>,
}

impl WebRtcConfig {
    fn apply(&self, conn: &mut VoiceConnection) {
        if let Some(w) = conn.webrtc_mut() {
            if let Some(urls) = &self.ice_servers {
                w.set_ice_servers(urls.clone());
            }
            if let Some(engine) = &self.setting_engine {
                w.set_setting_engine(engine.clone());
            }
        }
    }
}

struct VoiceState {
    guild_id: Option<String>,
    channel_id: String,
    session_id: Option<String>,
    slot: Slot,
}

struct StreamState {
    guild_id: Option<String>,
    channel_id: String,
    session_id: String,
    stream_key: Option<String>,
    endpoint: Option<(String, String)>,
    ready_tx: Option<ReadySender>,
    event_tx: mpsc::UnboundedSender<VoiceEvent>,
    slot: Option<Slot>,
}

/// Mirrors `Streamer.ts`; the caller owns the main gateway WS, feeds `handle_event` and writes `gateway_tx`.
pub struct Streamer {
    user_id: String,
    gateway_tx: mpsc::UnboundedSender<GatewayPayload>,
    voice: Option<VoiceState>,
    stream: Option<StreamState>,
    webrtc: WebRtcConfig,
}

impl Streamer {
    pub fn new(user_id: String, gateway_tx: mpsc::UnboundedSender<GatewayPayload>) -> Self {
        Self {
            user_id,
            gateway_tx,
            voice: None,
            stream: None,
            webrtc: WebRtcConfig::default(),
        }
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// ICE servers for connections started after this call (default: the JS reference's STUN server).
    pub fn set_ice_servers(&mut self, urls: Vec<String>) {
        self.webrtc.ice_servers = Some(urls);
    }

    /// webrtc-rs setting engine for connections started after this call.
    pub fn set_setting_engine(&mut self, engine: SettingEngine) {
        self.webrtc.setting_engine = Some(engine);
    }

    /// Join a voice channel (`joinVoice`); `guild_id` is `None` for DM calls.
    pub fn join_voice(
        &mut self,
        guild_id: Option<String>,
        channel_id: String,
    ) -> PendingConnection {
        self.leave_voice_connections();
        let (event_tx, events) = mpsc::unbounded_channel();
        let (ready_tx, ready) = oneshot::channel();
        let conn = VoiceConnection::new(
            guild_id.clone(),
            channel_id.clone(),
            self.user_id.clone(),
            event_tx,
        );
        let slot = Slot::new(conn, ready_tx);
        self.voice = Some(VoiceState {
            guild_id,
            channel_id,
            session_id: None,
            slot,
        });
        self.signal_video(false);
        PendingConnection {
            ready: Some(ready),
            events,
        }
    }

    /// Start a Go Live stream on the joined channel (`createStream`).
    pub fn create_stream(&mut self) -> Result<PendingConnection, StreamerError> {
        let voice = self.voice.as_ref().ok_or(StreamerError::NotInVoice)?;
        let session_id = voice.session_id.clone().ok_or(StreamerError::NoSession)?;
        let (guild_id, channel_id) = (voice.guild_id.clone(), voice.channel_id.clone());
        self.stop_stream_connection();
        self.signal_stream();
        let (event_tx, events) = mpsc::unbounded_channel();
        let (ready_tx, ready) = oneshot::channel();
        self.stream = Some(StreamState {
            guild_id,
            channel_id,
            session_id,
            stream_key: None,
            endpoint: None,
            ready_tx: Some(ready_tx),
            event_tx,
            slot: None,
        });
        Ok(PendingConnection {
            ready: Some(ready),
            events,
        })
    }

    /// Feed one main-gateway dispatch (`client.on("raw")` in JS).
    pub fn handle_event(&mut self, event: GatewayEvent) {
        match event {
            GatewayEvent::VoiceStateUpdate(d) => {
                if d.user_id != self.user_id {
                    return;
                }
                debug!("Gateway: VOICE_STATE_UPDATE session={}", d.session_id);
                if let Some(voice) = &mut self.voice {
                    voice.session_id = Some(d.session_id.clone());
                    voice.slot.set_session(d.session_id);
                    voice.slot.start_if_ready(&self.webrtc);
                }
            }
            GatewayEvent::VoiceServerUpdate(d) => {
                let Some(voice) = &mut self.voice else {
                    return;
                };
                if voice.guild_id != d.guild_id {
                    return;
                }
                if d.channel_id
                    .as_ref()
                    .is_some_and(|c| *c != voice.channel_id)
                {
                    return;
                }
                let Some(endpoint) = d.endpoint else {
                    debug!("Gateway: VOICE_SERVER_UPDATE without endpoint ignored");
                    return;
                };
                debug!("Gateway: VOICE_SERVER_UPDATE endpoint={}", endpoint);
                voice.slot.set_tokens(endpoint, d.token);
                voice.slot.start_if_ready(&self.webrtc);
            }
            GatewayEvent::StreamCreate(d) => {
                debug!("Gateway: STREAM_CREATE key={}", d.stream_key);
                let user_id = self.user_id.clone();
                let Some(stream) = &mut self.stream else {
                    return;
                };
                if !stream.matches(&d.stream_key, &user_id) || stream.slot.is_some() {
                    return;
                }
                let conn = VoiceConnection::new_stream(
                    stream.guild_id.clone(),
                    stream.channel_id.clone(),
                    user_id,
                    d.rtc_server_id,
                    stream.event_tx.clone(),
                );
                let ready_tx = stream
                    .ready_tx
                    .take()
                    .unwrap_or_else(|| oneshot::channel().0);
                let mut slot = Slot::new(conn, ready_tx);
                slot.set_session(stream.session_id.clone());
                if let Some((endpoint, token)) = stream.endpoint.take() {
                    slot.set_tokens(endpoint, token);
                }
                slot.start_if_ready(&self.webrtc);
                stream.stream_key = Some(d.stream_key);
                stream.slot = Some(slot);
            }
            GatewayEvent::StreamServerUpdate(d) => {
                debug!("Gateway: STREAM_SERVER_UPDATE key={}", d.stream_key);
                let user_id = self.user_id.clone();
                let Some(stream) = &mut self.stream else {
                    return;
                };
                if !stream.matches(&d.stream_key, &user_id) {
                    return;
                }
                let Some(endpoint) = d.endpoint else {
                    return;
                };
                match &mut stream.slot {
                    Some(slot) => {
                        slot.set_tokens(endpoint, d.token);
                        slot.start_if_ready(&self.webrtc);
                    }
                    None => stream.endpoint = Some((endpoint, d.token)),
                }
            }
            GatewayEvent::Unknown(_) => {}
        }
    }

    /// Stop the Go Live stream (`stopStream`).
    pub fn stop_stream(&mut self) {
        if self.stream.is_none() {
            return;
        }
        self.stop_stream_connection();
        self.signal_stop_stream();
    }

    /// Leave the voice channel (`leaveVoice`).
    pub fn leave_voice(&mut self) {
        self.leave_voice_connections();
        self.signal_leave_voice();
    }

    /// Retarget the next `create_stream()` after a channel move, without rejoining.
    pub fn set_voice_channel(&mut self, guild_id: Option<String>, channel_id: String) {
        if let Some(voice) = &mut self.voice {
            voice.guild_id = guild_id;
            voice.channel_id = channel_id;
        }
    }

    pub fn voice_channel(&self) -> Option<(Option<&str>, &str)> {
        self.voice
            .as_ref()
            .map(|v| (v.guild_id.as_deref(), v.channel_id.as_str()))
    }

    pub fn voice_connection(&self) -> Option<&MediaConnection> {
        self.voice.as_ref().and_then(|v| v.slot.started.as_ref())
    }

    pub fn stream_connection(&self) -> Option<&MediaConnection> {
        self.stream
            .as_ref()
            .and_then(|s| s.slot.as_ref())
            .and_then(|s| s.started.as_ref())
    }

    /// The stream key of the current or next Go Live stream.
    pub fn stream_key(&self) -> Option<String> {
        let voice = self.voice.as_ref()?;
        Some(generate_stream_key(
            stream_type(voice.guild_id.as_deref()),
            voice.guild_id.as_deref(),
            &voice.channel_id,
            &self.user_id,
        ))
    }

    /// Op 4 with the current channel, as `signalVideo(enabled)`.
    pub fn signal_video(&self, video_enabled: bool) {
        let Some(voice) = &self.voice else {
            return;
        };
        self.send_opcode(
            GatewayOpCode::VoiceStateUpdate,
            json!({
                "guild_id": voice.guild_id,
                "channel_id": voice.channel_id,
                "self_mute": false,
                "self_deaf": true,
                "self_video": video_enabled,
            }),
        );
    }

    fn leave_voice_connections(&mut self) {
        self.stop_stream_connection();
        if let Some(voice) = self.voice.take() {
            voice.slot.stop();
        }
    }

    fn stop_stream_connection(&mut self) {
        if let Some(stream) = self.stream.take()
            && let Some(slot) = stream.slot
        {
            slot.stop();
        }
    }

    fn send_opcode(&self, op: GatewayOpCode, d: Value) {
        let _ = self.gateway_tx.send(GatewayPayload { op: op as u8, d });
    }

    fn signal_stream(&self) {
        let (Some(voice), Some(key)) = (&self.voice, self.stream_key()) else {
            return;
        };
        self.send_opcode(
            GatewayOpCode::StreamCreate,
            json!({
                "type": stream_type(voice.guild_id.as_deref()).to_string(),
                "guild_id": voice.guild_id,
                "channel_id": voice.channel_id,
                "preferred_region": null,
            }),
        );
        self.send_opcode(
            GatewayOpCode::StreamSetPaused,
            json!({ "stream_key": key, "paused": false }),
        );
    }

    fn signal_stop_stream(&self) {
        if let Some(key) = self.stream_key() {
            self.send_opcode(GatewayOpCode::StreamDelete, json!({ "stream_key": key }));
        }
    }

    fn signal_leave_voice(&self) {
        self.send_opcode(
            GatewayOpCode::VoiceStateUpdate,
            json!({
                "guild_id": null,
                "channel_id": null,
                "self_mute": true,
                "self_deaf": false,
                "self_video": false,
            }),
        );
    }
}

impl StreamState {
    fn matches(&self, stream_key: &str, user_id: &str) -> bool {
        parse_stream_key(stream_key).is_ok_and(|k| {
            k.guild_id == self.guild_id && k.channel_id == self.channel_id && k.user_id == user_id
        })
    }
}

fn stream_type(guild_id: Option<&str>) -> StreamType {
    if guild_id.is_some() {
        StreamType::Guild
    } else {
        StreamType::Call
    }
}
