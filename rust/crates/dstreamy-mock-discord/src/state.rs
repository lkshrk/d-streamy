use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};

use crate::config::MockConfig;
use crate::depacketize::VideoCodec;
use crate::voice::VoiceConn;

/// Which socket an op arrived on; voice sockets carry their IDENTIFY `server_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Socket {
    Gateway,
    Voice(String),
    Stream(String),
}

impl Socket {
    pub fn kind(&self) -> Option<VoiceKind> {
        match self {
            Socket::Gateway => None,
            Socket::Voice(_) => Some(VoiceKind::Voice),
            Socket::Stream(_) => Some(VoiceKind::Stream),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum OpPayload {
    Json(Value),
    Binary(Vec<u8>),
}

/// One client → server message, in arrival order.
#[derive(Debug, Clone, PartialEq)]
pub struct ReceivedOp {
    pub socket: Socket,
    pub op: u8,
    pub payload: OpPayload,
}

impl ReceivedOp {
    pub fn json(&self) -> Option<&Value> {
        match &self.payload {
            OpPayload::Json(v) => Some(v),
            OpPayload::Binary(_) => None,
        }
    }

    /// The payload after the opcode byte of a binary op.
    pub fn binary(&self) -> Option<&[u8]> {
        match &self.payload {
            OpPayload::Binary(b) => Some(b),
            OpPayload::Json(_) => None,
        }
    }

    pub fn is(&self, kind: Option<VoiceKind>, op: u8) -> bool {
        self.socket.kind() == kind && self.op == op
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VoiceKind {
    Voice,
    Stream,
}

/// One reassembled access unit, DAVE-decrypted when it was encrypted.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedVideoFrame {
    pub kind: VoiceKind,
    pub server_id: String,
    pub ssrc: u32,
    pub payload_type: u8,
    pub codec: VideoCodec,
    pub rtp_timestamp: u32,
    /// Annex B with 4-byte start codes.
    pub data: Vec<u8>,
    pub e2ee: bool,
    pub playout_delay: Option<[u8; 3]>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordedAudioFrame {
    pub kind: VoiceKind,
    pub server_id: String,
    pub ssrc: u32,
    pub payload_type: u8,
    pub rtp_timestamp: u32,
    pub data: Vec<u8>,
    pub e2ee: bool,
}

/// Everything the mock received, for assertions.
#[derive(Debug, Default, Clone)]
pub struct Recorded {
    pub ops: Vec<ReceivedOp>,
    pub video: Vec<RecordedVideoFrame>,
    pub audio: Vec<RecordedAudioFrame>,
    /// Frames lost to sequence gaps, depacketization or DAVE decryption errors.
    pub dropped_frames: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceStateSnapshot {
    pub guild_id: String,
    pub channel_id: String,
    pub session_id: String,
    pub self_mute: bool,
    pub self_deaf: bool,
    pub self_video: bool,
    pub self_stream: bool,
}

impl VoiceStateSnapshot {
    pub(crate) fn dispatch(&self, user_id: &str) -> Value {
        json!({
            "guild_id": self.guild_id,
            "channel_id": self.channel_id,
            "user_id": user_id,
            "session_id": self.session_id,
            "deaf": false,
            "mute": false,
            "self_deaf": self.self_deaf,
            "self_mute": self.self_mute,
            "self_video": self.self_video,
            "self_stream": self.self_stream,
            "suppress": false,
            "request_to_speak_timestamp": null,
            "member": { "user": { "id": user_id }, "roles": [], "deaf": false, "mute": false },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSnapshot {
    pub stream_key: String,
    pub rtc_server_id: String,
    pub paused: bool,
}

#[derive(Debug)]
pub(crate) enum Control {
    Send(String),
    /// `None` drops the TCP connection without a close frame.
    Close(Option<u16>),
}

pub(crate) struct Connection {
    pub id: u64,
    pub tx: mpsc::UnboundedSender<Control>,
}

pub(crate) struct GatewaySession {
    pub seq: u64,
    pub buffer: Vec<(u64, String)>,
}

#[derive(Debug, Clone)]
pub(crate) struct VoiceAuth {
    pub server_id: String,
    pub kind: VoiceKind,
    /// Voice channel for voice connections, `rtc_server_id - 1` for streams (the DAVE group id).
    pub dave_channel_id: u64,
}

#[derive(Default)]
pub(crate) struct State {
    pub sessions: HashMap<String, GatewaySession>,
    /// The user's live gateway session; dispatches are buffered here while no socket is bound.
    pub session: Option<String>,
    pub gateway: Option<(Connection, Option<String>)>,
    pub gateway_zlib: Option<bool>,
    pub voice: Option<VoiceStateSnapshot>,
    pub streams: HashMap<String, StreamSnapshot>,
    pub voice_auth: HashMap<String, VoiceAuth>,
    pub voice_conns: HashMap<String, Arc<tokio::sync::Mutex<VoiceConn>>>,
    pub voice_sockets: HashMap<String, (VoiceKind, Connection)>,
    pub recorded: Recorded,
    pub next_id: u64,
}

pub(crate) struct Shared {
    pub config: MockConfig,
    pub gateway_url: String,
    pub voice_endpoint: String,
    pub voice_port: u16,
    state: Mutex<State>,
    changed: Notify,
}

impl Shared {
    pub fn new(
        config: MockConfig,
        gateway_url: String,
        voice_endpoint: String,
        voice_port: u16,
    ) -> Self {
        Self {
            config,
            gateway_url,
            voice_endpoint,
            voice_port,
            state: Mutex::new(State::default()),
            changed: Notify::new(),
        }
    }

    // A panic while holding the lock leaves plain data behind; keep serving it.
    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn record(&self, f: impl FnOnce(&mut Recorded)) {
        f(&mut self.state().recorded);
        self.changed.notify_waiters();
    }

    pub fn notify(&self) {
        self.changed.notify_waiters();
    }

    pub fn next_id(&self) -> u64 {
        let mut s = self.state();
        s.next_id += 1;
        s.next_id
    }

    pub async fn wait_for<T>(
        &self,
        timeout: Duration,
        mut f: impl FnMut(&Recorded) -> Option<T>,
    ) -> Option<T> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(v) = f(&self.state().recorded) {
                return Some(v);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return None;
            }
        }
    }

    /// Send a dispatch on the current gateway session, buffered for RESUME replay.
    pub fn dispatch(&self, state: &mut State, t: &str, d: Value) {
        let Some(session_id) = state.session.clone() else {
            return;
        };
        let Some(session) = state.sessions.get_mut(&session_id) else {
            return;
        };
        session.seq += 1;
        let payload = json!({ "op": 0, "t": t, "s": session.seq, "d": d }).to_string();
        session.buffer.push((session.seq, payload.clone()));
        if let Some((conn, Some(bound))) = &state.gateway
            && *bound == session_id
        {
            let _ = conn.tx.send(Control::Send(payload));
        }
    }
}
