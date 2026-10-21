//! In-process fake of Discord's main gateway, voice gateway, WebRTC media server and DAVE peer, for tests.

pub mod config;
pub mod dave;
pub mod depacketize;
mod gateway;
pub mod rtc;
mod sink;
mod state;
mod util;
mod voice;

use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::task::JoinHandle;

pub use config::{
    ChannelKind, MOCK_GUILD_ID, MOCK_OTHER_VOICE_CHANNEL_ID, MOCK_TEXT_CHANNEL_ID, MOCK_TOKEN,
    MOCK_USER_ID, MOCK_VOICE_CHANNEL_ID, MockChannel, MockConfig, MockGuild, MockUser,
};
pub use depacketize::VideoCodec;
pub use state::{
    OpPayload, ReceivedOp, Recorded, RecordedAudioFrame, RecordedVideoFrame, Socket,
    StreamSnapshot, VoiceKind, VoiceStateSnapshot,
};

pub mod opcodes {
    pub mod gateway {
        pub use crate::gateway::{
            CLOSE_ALREADY_AUTHENTICATED, CLOSE_AUTHENTICATION_FAILED, CLOSE_DECODE_ERROR,
            CLOSE_NOT_AUTHENTICATED, CLOSE_UNKNOWN_ERROR, OP_DISPATCH, OP_HEARTBEAT,
            OP_HEARTBEAT_ACK, OP_HELLO, OP_IDENTIFY, OP_INVALID_SESSION, OP_RECONNECT, OP_RESUME,
            OP_STREAM_CREATE, OP_STREAM_DELETE, OP_STREAM_SET_PAUSED, OP_STREAM_WATCH,
            OP_VOICE_STATE_UPDATE,
        };
    }
    pub mod voice {
        pub use crate::voice::{
            CLOSE_AUTHENTICATION_FAILED, CLOSE_DISCONNECTED, CLOSE_NOT_AUTHENTICATED,
            CLOSE_SERVER_CRASHED, CLOSE_SESSION_NO_LONGER_VALID, OP_CLIENTS_CONNECT,
            OP_DAVE_EXECUTE_TRANSITION, OP_DAVE_TRANSITION_READY, OP_HEARTBEAT, OP_HEARTBEAT_ACK,
            OP_HELLO, OP_IDENTIFY, OP_MLS_ANNOUNCE_COMMIT_TRANSITION, OP_MLS_COMMIT_WELCOME,
            OP_MLS_EXTERNAL_SENDER, OP_MLS_INVALID_COMMIT_WELCOME, OP_MLS_KEY_PACKAGE,
            OP_MLS_PROPOSALS, OP_READY, OP_RESUME, OP_RESUMED, OP_SELECT_PROTOCOL,
            OP_SELECT_PROTOCOL_ACK, OP_SPEAKING, OP_VIDEO,
        };
    }
}

use state::{Control, Shared};

/// A running mock; all listeners are on 127.0.0.1 and stop when it is dropped.
pub struct MockDiscord {
    shared: Arc<Shared>,
    tasks: Vec<JoinHandle<()>>,
}

impl MockDiscord {
    pub async fn start() -> std::io::Result<Self> {
        Self::start_with(MockConfig::default()).await
    }

    pub async fn start_with(config: MockConfig) -> std::io::Result<Self> {
        let gateway = TcpListener::bind("127.0.0.1:0").await?;
        let voice = TcpListener::bind("127.0.0.1:0").await?;
        let gateway_url = format!("ws://{}", gateway.local_addr()?);
        let voice_port = voice.local_addr()?.port();
        let voice_endpoint = format!("ws://127.0.0.1:{voice_port}");
        let shared = Arc::new(Shared::new(config, gateway_url, voice_endpoint, voice_port));
        let tasks = vec![
            tokio::spawn(gateway::serve(shared.clone(), gateway)),
            tokio::spawn(voice::serve(shared.clone(), voice)),
        ];
        Ok(Self { shared, tasks })
    }

    pub fn config(&self) -> &MockConfig {
        &self.shared.config
    }

    /// `ws://127.0.0.1:<port>`; any path and query are accepted.
    pub fn gateway_url(&self) -> String {
        self.shared.gateway_url.clone()
    }

    /// The `endpoint` sent in VOICE_SERVER_UPDATE and STREAM_SERVER_UPDATE (with a `ws://` scheme).
    pub fn voice_endpoint(&self) -> String {
        self.shared.voice_endpoint.clone()
    }

    pub fn received_ops(&self) -> Vec<ReceivedOp> {
        self.shared.state().recorded.ops.clone()
    }

    pub fn recorded_video(&self) -> Vec<RecordedVideoFrame> {
        self.shared.state().recorded.video.clone()
    }

    pub fn recorded_audio(&self) -> Vec<RecordedAudioFrame> {
        self.shared.state().recorded.audio.clone()
    }

    pub fn recorded(&self) -> Recorded {
        self.shared.state().recorded.clone()
    }

    /// Re-evaluate `f` whenever something is recorded until it returns `Some` or `timeout` passes.
    pub async fn wait_for<T>(
        &self,
        timeout: Duration,
        f: impl FnMut(&Recorded) -> Option<T>,
    ) -> Option<T> {
        self.shared.wait_for(timeout, f).await
    }

    /// The `n`-th (0-based) received op `op` on sockets of `kind` (`None`: main gateway).
    pub async fn wait_for_op(
        &self,
        kind: Option<VoiceKind>,
        op: u8,
        n: usize,
        timeout: Duration,
    ) -> Option<ReceivedOp> {
        self.wait_for(timeout, |r| {
            r.ops.iter().filter(|o| o.is(kind, op)).nth(n).cloned()
        })
        .await
    }

    /// Whether the latest main gateway socket asked for `compress=zlib-stream`.
    pub fn gateway_zlib(&self) -> Option<bool> {
        self.shared.state().gateway_zlib
    }

    pub fn session_id(&self) -> Option<String> {
        self.shared.state().session.clone()
    }

    pub fn voice_state(&self) -> Option<VoiceStateSnapshot> {
        self.shared.state().voice.clone()
    }

    pub fn streams(&self) -> Vec<StreamSnapshot> {
        self.shared.state().streams.values().cloned().collect()
    }

    /// Whether the simulated participant shares an MLS group with the client on that connection.
    pub async fn dave_active(&self, kind: VoiceKind) -> bool {
        let conns: Vec<_> = self.shared.state().voice_conns.values().cloned().collect();
        for conn in conns {
            let c = conn.lock().await;
            if c.kind == kind && c.dave_active() {
                return true;
            }
        }
        false
    }

    /// The voice privacy code of the MLS group on that connection, once DAVE is active.
    pub async fn voice_privacy_code(&self, kind: VoiceKind) -> Option<String> {
        let conns: Vec<_> = self.shared.state().voice_conns.values().cloned().collect();
        for conn in conns {
            let c = conn.lock().await;
            if c.kind == kind
                && let Some(code) = c.privacy_code()
            {
                return Some(code);
            }
        }
        None
    }

    /// Send a viewer PLI for the video SSRC of every peer connection of `kind`.
    pub async fn send_pli(&self, kind: VoiceKind) -> bool {
        let conns: Vec<_> = self.shared.state().voice_conns.values().cloned().collect();
        let mut sent = false;
        for conn in conns {
            let c = conn.lock().await;
            if c.kind == kind {
                sent |= c.send_pli().await;
            }
        }
        sent
    }

    /// Close the voice WebSocket of `kind`; `None` drops TCP without a close frame. The connection stays resumable.
    pub fn drop_voice(&self, kind: VoiceKind, close_code: Option<u16>) -> bool {
        let state = self.shared.state();
        let socket = state.voice_sockets.values().find(|(k, _)| *k == kind);
        socket.is_some_and(|(_, c)| c.tx.send(Control::Close(close_code)).is_ok())
    }

    /// Close the main gateway socket; the session stays resumable and buffers dispatches.
    pub fn drop_gateway(&self, close_code: Option<u16>) -> bool {
        let state = self.shared.state();
        state
            .gateway
            .as_ref()
            .is_some_and(|(c, _)| c.tx.send(Control::Close(close_code)).is_ok())
    }

    pub fn request_reconnect(&self) -> bool {
        let state = self.shared.state();
        let msg =
            serde_json::json!({ "op": gateway::OP_RECONNECT, "d": null, "s": null, "t": null });
        state
            .gateway
            .as_ref()
            .is_some_and(|(c, _)| c.tx.send(Control::Send(msg.to_string())).is_ok())
    }

    /// Op 9 INVALID_SESSION; when not resumable the session is forgotten and the client must IDENTIFY.
    pub fn invalidate_session(&self, resumable: bool) -> bool {
        let mut state = self.shared.state();
        gateway::invalidate_session(&mut state, resumable)
    }

    /// Move the user to another voice channel, as when someone drags them: VOICE_STATE_UPDATE only.
    pub fn move_to_channel(&self, guild_id: &str, channel_id: &str) -> bool {
        let mut state = self.shared.state();
        let moved = gateway::move_to_channel(&self.shared, &mut state, guild_id, channel_id);
        drop(state);
        self.shared.notify();
        moved
    }

    pub async fn shutdown(mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
        voice::close_all(&self.shared).await;
    }
}

impl Drop for MockDiscord {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        let mut state = self.shared.state();
        state.gateway = None;
        state.voice_sockets.clear();
    }
}
