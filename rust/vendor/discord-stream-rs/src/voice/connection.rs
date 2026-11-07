use crate::dave::{DaveError, DaveHandler};
use crate::voice::codec_payload::{self, ALL_VIDEO_CODECS};
use crate::voice::opcodes::{VoiceOpCode, VoiceOpCodeBinary};
use crate::voice::stream_connection::dave_channel_id;
use crate::voice::types::{
    BinaryMessage, ClientDisconnect, ClientsConnect, DaveExecuteTransition, DavePrepareEpoch,
    DavePrepareTransition, GatewayMessage, Hello, Ready, SelectProtocolAck,
};
use crate::voice::webrtc::{Flushed, VideoGate, VideoProgress, WebRtcError, WebRtcWrapper};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, LazyLock, Mutex as StdMutex, MutexGuard};
use std::time::Duration;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config, tungstenite::Message,
};
use tracing::{debug, error, warn};
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

/// Consecutive failed WebSocket connects after which a connection gives up.
pub const MAX_CONNECT_ATTEMPTS: u32 = 5;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// Sockets opened within `RECONNECT_WINDOW` before the connection gives up on a flapping server.
pub const MAX_SESSIONS_PER_WINDOW: usize = 10;
const RECONNECT_WINDOW: Duration = Duration::from_secs(60);
const MAX_DEFERRED_COMMANDS: usize = 64;
/// DAVE transitions announced but never executed are kept at most this many; the oldest go first.
const MAX_PENDING_TRANSITIONS: usize = 16;
/// Voice channels hold far fewer members; past this, CLIENTS_CONNECT ids are dropped.
const MAX_CONNECTED_USERS: usize = 10_000;
const MAX_UNACKED_HEARTBEATS: u32 = 3;
/// Discord sends about 13.75 s; the clamp keeps a hostile HELLO from spamming or silencing heartbeats.
const MIN_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const MAX_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10 * 60);

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<Ws, Message>;

// An explicit provider keeps working if another crate also enables rustls' aws-lc-rs backend.
static TLS_CONFIG: LazyLock<Arc<rustls::ClientConfig>> = LazyLock::new(|| {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
});

#[derive(Debug, Error)]
pub enum ConnectionError {
    #[error("WebSocket error: {0}")]
    WebSocket(String),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("DAVE error: {0}")]
    Dave(#[from] DaveError),
    #[error("WebRTC error: {0}")]
    WebRtc(#[from] WebRtcError),
    #[error("Connection closed unexpectedly")]
    Closed,
    #[error("Not connected")]
    NotConnected,
    #[error("Invalid DAVE channel id from {0:?}")]
    InvalidChannelId(String),
    #[error("voice endpoint {0:?} is not a Discord host; refusing to connect")]
    UntrustedEndpoint(String),
    #[error("voice gateway protocol error: {0}")]
    Protocol(&'static str),
    #[error("media path failed: {0}")]
    MediaFailed(String),
}

/// The WebRTC + SSRC parameters received in the Ready message.
#[derive(Debug, Clone)]
pub struct WebRtcParams {
    pub address: String,
    pub port: u16,
    pub audio_ssrc: u32,
    pub video_ssrc: u32,
    pub rtx_ssrc: u32,
    pub supported_encryption_modes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoAttributes {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// Events emitted by the voice connection to callers.
pub enum VoiceEvent {
    Ready(WebRtcParams),
    SelectProtocolAck {
        sdp: String,
        dave_version: u16,
    },
    Resumed,
    MlsCommitWelcome(Vec<u8>),
    /// The peer connection is dead while the WebSocket is up; the caller should rebuild the connection.
    MediaFailed {
        reason: String,
    },
    /// The connection stopped for good; `code` is the WebSocket close code.
    Closed {
        code: Option<u16>,
    },
}

// The answer SDP carries the ICE password, so only its size is shown.
impl std::fmt::Debug for VoiceEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready(params) => f.debug_tuple("Ready").field(params).finish(),
            Self::SelectProtocolAck { sdp, dave_version } => f
                .debug_struct("SelectProtocolAck")
                .field("sdp_len", &sdp.len())
                .field("dave_version", dave_version)
                .finish(),
            Self::Resumed => f.write_str("Resumed"),
            Self::MlsCommitWelcome(payload) => f
                .debug_tuple("MlsCommitWelcome")
                .field(&payload.len())
                .finish(),
            Self::MediaFailed { reason } => f
                .debug_struct("MediaFailed")
                .field("reason", reason)
                .finish(),
            Self::Closed { code } => f.debug_struct("Closed").field("code", code).finish(),
        }
    }
}

/// How long the media path may stay down before `VoiceEvent::MediaFailed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaTimeouts {
    /// A peer connection `Disconnected` for this long is reported as failed.
    pub disconnected_grace: Duration,
    /// After `Failed`, how long the in-place restart (a new SELECT_PROTOCOL) may take to connect.
    pub restart_timeout: Duration,
}

impl Default for MediaTimeouts {
    fn default() -> Self {
        Self {
            disconnected_grace: Duration::from_secs(5),
            restart_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionKind {
    Voice,
    /// Go Live; `rtc_server_id` comes from STREAM_CREATE.
    Stream {
        rtc_server_id: String,
    },
}

enum Command {
    Send(Message),
    SetSession(String),
    SetTokens { server: String, token: String },
    PeerState(RTCPeerConnectionState),
    Stop,
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Send(_) => f.write_str("Send"),
            Self::SetSession(_) => f.write_str("SetSession"),
            Self::SetTokens { .. } => f.write_str("SetTokens"),
            Self::PeerState(state) => f.debug_tuple("PeerState").field(state).finish(),
            Self::Stop => f.write_str("Stop"),
        }
    }
}

struct Shared {
    kind: ConnectionKind,
    cmd_tx: mpsc::UnboundedSender<Command>,
    webrtc: Mutex<WebRtcWrapper>,
    params: StdMutex<Option<WebRtcParams>>,
    dave: Arc<StdMutex<DaveHandler>>,
    keyframe_requests: watch::Receiver<u64>,
    video_progress: watch::Receiver<VideoProgress>,
    video_gate: Arc<VideoGate>,
}

/// Cloneable handle to a running connection, like `WebRtcConnWrapper` plus its `mediaConnection` in JS.
#[derive(Clone)]
pub struct MediaConnection {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for MediaConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaConnection")
            .field("kind", &self.shared.kind)
            .finish()
    }
}

impl MediaConnection {
    pub fn kind(&self) -> &ConnectionKind {
        &self.shared.kind
    }

    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }

    pub fn webrtc_params(&self) -> Option<WebRtcParams> {
        lock(&self.shared.params).clone()
    }

    /// True once the peer connection is connected, like `WebRtcConnWrapper.ready`.
    pub async fn is_ready(&self) -> bool {
        self.shared.webrtc.lock().await.is_ready()
    }

    pub fn dave_ready(&self) -> bool {
        lock(&self.shared.dave).is_ready()
    }

    pub fn require_e2ee(&self) -> Result<(), DaveError> {
        lock(&self.shared.dave).require_e2ee()
    }

    pub fn e2ee_unavailable(&self) -> Option<u16> {
        lock(&self.shared.dave).e2ee_unavailable()
    }

    /// The negotiated DAVE protocol version; 0 means Discord runs this call without E2EE.
    pub fn dave_protocol_version(&self) -> u16 {
        lock(&self.shared.dave).protocol_version()
    }

    /// Bumped for every viewer PLI or FIR; the sender should emit a keyframe.
    pub fn keyframe_requests(&self) -> watch::Receiver<u64> {
        self.shared.keyframe_requests.clone()
    }

    /// `VideoGate::flush`; never waits on the connection's lock.
    pub fn flush_video(&self) -> Flushed {
        self.shared.video_gate.flush()
    }

    /// The video flush and generation, shared by every handle to this connection.
    pub fn video_gate(&self) -> Arc<VideoGate> {
        self.shared.video_gate.clone()
    }

    /// Pass to `send_video_frame_at_generation` so a flush after this point drops the frame.
    pub fn video_generation(&self) -> u64 {
        self.shared.video_gate.generation()
    }

    /// What became of video after `send_video_frame*` returned `Ok`: written or lost in the pacer.
    pub fn video_progress(&self) -> watch::Receiver<VideoProgress> {
        self.shared.video_progress.clone()
    }

    pub fn voice_privacy_code(&self) -> Option<String> {
        lock(&self.shared.dave)
            .voice_privacy_code()
            .map(str::to_owned)
    }

    /// Select the video codec and reset RTP state, like `setPacketizer(codec)`.
    pub async fn set_packetizer(&self, video_codec: &str) -> Result<(), ConnectionError> {
        let params = self.webrtc_params().ok_or(ConnectionError::NotConnected)?;
        self.shared
            .webrtc
            .lock()
            .await
            .set_packetizer(&params, video_codec)?;
        Ok(())
    }

    /// Send one Annex B access unit and advance the RTP clock by `frametime_ms`.
    pub async fn send_video_frame(
        &self,
        frame: &[u8],
        frametime_ms: f64,
    ) -> Result<(), WebRtcError> {
        self.shared
            .webrtc
            .lock()
            .await
            .send_video_frame(frame, frametime_ms)
            .await
    }

    /// Send one Opus frame and advance the RTP clock by `frametime_ms`.
    pub async fn send_audio_frame(
        &self,
        frame: &[u8],
        frametime_ms: f64,
    ) -> Result<(), WebRtcError> {
        self.shared
            .webrtc
            .lock()
            .await
            .send_audio_frame(frame, frametime_ms)
            .await
    }

    /// Send one Annex B access unit stamped with `pts` (relative to the first frame).
    pub async fn send_video_frame_at(
        &self,
        frame: &[u8],
        pts: Duration,
    ) -> Result<(), WebRtcError> {
        self.shared
            .webrtc
            .lock()
            .await
            .send_video_frame_at(frame, pts)
            .await
    }

    pub async fn send_video_frame_at_generation(
        &self,
        frame: &[u8],
        pts: Duration,
        generation: u64,
    ) -> Result<(), WebRtcError> {
        self.shared
            .webrtc
            .lock()
            .await
            .send_video_frame_at_generation(frame, pts, generation)
            .await
    }

    /// Send one Opus frame stamped with `pts` (relative to the first frame).
    pub async fn send_audio_frame_at(
        &self,
        frame: &[u8],
        pts: Duration,
    ) -> Result<(), WebRtcError> {
        self.shared
            .webrtc
            .lock()
            .await
            .send_audio_frame_at(frame, pts)
            .await
    }

    /// Op 12 VIDEO; `None` announces no video, like `setVideoAttributes(false)`.
    pub fn set_video_attributes(
        &self,
        attrs: Option<VideoAttributes>,
    ) -> Result<(), ConnectionError> {
        let params = self.webrtc_params().ok_or(ConnectionError::NotConnected)?;
        self.send_json(VoiceOpCode::Video, video_payload(&params, attrs))
    }

    /// Op 5 SPEAKING; stream connections use flag 2 (soundshare) like `StreamConnection.setSpeaking`.
    pub fn set_speaking(&self, speaking: bool) -> Result<(), ConnectionError> {
        let params = self.webrtc_params().ok_or(ConnectionError::NotConnected)?;
        let flag = match (&self.shared.kind, speaking) {
            (_, false) => 0,
            (ConnectionKind::Voice, true) => 1,
            (ConnectionKind::Stream { .. }, true) => 2,
        };
        self.send_json(
            VoiceOpCode::Speaking,
            json!({ "delay": 0, "speaking": flag, "ssrc": params.audio_ssrc }),
        )
    }

    pub fn stop(&self) {
        let _ = self.shared.cmd_tx.send(Command::Stop);
    }

    /// As if webrtc-rs reported the peer connection `Failed`.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn simulate_peer_failed(&self) {
        let _ = self
            .shared
            .cmd_tx
            .send(Command::PeerState(RTCPeerConnectionState::Failed));
    }

    /// As if webrtc-rs reported the peer connection `Disconnected`.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn simulate_peer_disconnected(&self) {
        let _ = self
            .shared
            .cmd_tx
            .send(Command::PeerState(RTCPeerConnectionState::Disconnected));
    }

    pub(crate) fn set_session(&self, session_id: String) {
        let _ = self.shared.cmd_tx.send(Command::SetSession(session_id));
    }

    pub(crate) fn set_tokens(&self, server: String, token: String) {
        let _ = self
            .shared
            .cmd_tx
            .send(Command::SetTokens { server, token });
    }

    fn send_json(&self, op: VoiceOpCode, d: Value) -> Result<(), ConnectionError> {
        self.shared
            .cmd_tx
            .send(Command::Send(text(op, d)))
            .map_err(|_| ConnectionError::Closed)
    }
}

/// Resolves when the connection is usable, like the promise of `joinVoice()`/`createStream()`.
pub type ReadyReceiver = oneshot::Receiver<Result<MediaConnection, ConnectionError>>;

/// A single voice gateway connection, corresponding to `BaseMediaConnection` in JS.
pub struct VoiceConnection {
    kind: ConnectionKind,
    guild_id: Option<String>,
    channel_id: String,
    bot_id: String,
    session_id: Option<String>,
    token: Option<String>,
    server: Option<String>,
    started: bool,
    closed: bool,
    seq: i64,

    dave: Arc<StdMutex<DaveHandler>>,
    connected_users: HashSet<String>,
    pending_transitions: HashMap<u64, u16>,
    dave_downgraded: bool,

    heartbeat_interval: Option<Duration>,
    unacked_heartbeats: u32,
    established: bool,
    params: Option<WebRtcParams>,
    webrtc: Option<WebRtcWrapper>,
    shared: Option<Arc<Shared>>,
    ready_tx: Option<oneshot::Sender<Result<MediaConnection, ConnectionError>>>,
    event_tx: mpsc::UnboundedSender<VoiceEvent>,

    media_timeouts: MediaTimeouts,
    /// When the media path counts as failed unless the peer connection connects first.
    media_deadline: Option<(Instant, String)>,
    media_failed: bool,
}

impl VoiceConnection {
    pub fn new(
        guild_id: Option<String>,
        channel_id: String,
        bot_id: String,
        event_tx: mpsc::UnboundedSender<VoiceEvent>,
    ) -> Result<Self, ConnectionError> {
        let dave_channel = channel_id
            .parse()
            .map_err(|_| ConnectionError::InvalidChannelId(channel_id.clone()))?;
        Ok(Self::with_kind(
            ConnectionKind::Voice,
            dave_channel,
            guild_id,
            channel_id,
            bot_id,
            event_tx,
        ))
    }

    pub fn new_stream(
        guild_id: Option<String>,
        channel_id: String,
        bot_id: String,
        rtc_server_id: String,
        event_tx: mpsc::UnboundedSender<VoiceEvent>,
    ) -> Result<Self, ConnectionError> {
        let dave_channel = dave_channel_id(&rtc_server_id)
            .ok_or_else(|| ConnectionError::InvalidChannelId(rtc_server_id.clone()))?;
        Ok(Self::with_kind(
            ConnectionKind::Stream { rtc_server_id },
            dave_channel,
            guild_id,
            channel_id,
            bot_id,
            event_tx,
        ))
    }

    fn with_kind(
        kind: ConnectionKind,
        dave_channel: u64,
        guild_id: Option<String>,
        channel_id: String,
        bot_id: String,
        event_tx: mpsc::UnboundedSender<VoiceEvent>,
    ) -> Self {
        let user_id: u64 = bot_id.parse().unwrap_or(0);
        let dave = Arc::new(StdMutex::new(DaveHandler::new(user_id, dave_channel)));
        Self {
            kind,
            guild_id,
            channel_id,
            bot_id,
            session_id: None,
            token: None,
            server: None,
            started: false,
            closed: false,
            seq: -1,
            webrtc: Some(WebRtcWrapper::new(dave.clone())),
            dave,
            connected_users: HashSet::new(),
            pending_transitions: HashMap::new(),
            dave_downgraded: false,
            heartbeat_interval: None,
            unacked_heartbeats: 0,
            established: false,
            params: None,
            shared: None,
            ready_tx: None,
            event_tx,
            media_timeouts: MediaTimeouts::default(),
            media_deadline: None,
            media_failed: false,
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn set_media_timeouts(&mut self, timeouts: MediaTimeouts) {
        self.media_timeouts = timeouts;
    }

    pub fn set_session(&mut self, session_id: String) {
        self.session_id = Some(session_id);
    }

    pub fn set_tokens(&mut self, server: String, token: String) {
        self.server = Some(server);
        self.token = Some(token);
    }

    /// Configure the WebRTC layer (ICE servers, setting engine) before `start()`.
    pub fn webrtc_mut(&mut self) -> Option<&mut WebRtcWrapper> {
        self.webrtc.as_mut()
    }

    /// The IDENTIFY server id: guild or DM channel for voice, `rtc_server_id` for streams.
    pub fn server_id(&self) -> &str {
        match &self.kind {
            ConnectionKind::Voice => self.guild_id.as_deref().unwrap_or(self.channel_id.as_str()),
            ConnectionKind::Stream { rtc_server_id } => rtc_server_id,
        }
    }

    /// True once session and tokens are known and the connection has not started.
    pub fn can_start(&self) -> bool {
        self.session_id.is_some() && self.token.is_some() && self.server.is_some() && !self.started
    }

    /// Spawn the task; the receiver resolves after SELECT_PROTOCOL_ACK or errors if the connection gives up.
    pub fn start(mut self) -> (MediaConnection, ReadyReceiver, JoinHandle<()>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let webrtc = self
            .webrtc
            .take()
            .unwrap_or_else(|| WebRtcWrapper::new(self.dave.clone()));
        let shared = Arc::new(Shared {
            kind: self.kind.clone(),
            cmd_tx,
            keyframe_requests: webrtc.keyframe_requests(),
            video_progress: webrtc.video_progress(),
            video_gate: webrtc.video_gate(),
            webrtc: Mutex::new(webrtc),
            params: StdMutex::new(None),
            dave: self.dave.clone(),
        });
        self.started = true;
        self.ready_tx = Some(ready_tx);
        self.shared = Some(shared.clone());
        let handle = MediaConnection { shared };
        let task = tokio::spawn(self.run(cmd_rx));
        (handle, ready_rx, task)
    }

    async fn run(mut self, mut cmd_rx: mpsc::UnboundedReceiver<Command>) {
        let mut resuming = false;
        let mut failed_connects = 0;
        let mut last_code = None;
        let mut last_error = None;
        let mut deferred = Vec::new();
        let mut sessions: VecDeque<Instant> = VecDeque::new();
        while !self.closed {
            let Some(server) = self.server.clone() else {
                break;
            };
            let url = match ws_url(&server) {
                Ok(url) => url,
                Err(e) => {
                    error!("{e}");
                    last_error = Some(e);
                    break;
                }
            };
            let now = Instant::now();
            while sessions
                .front()
                .is_some_and(|t| now.duration_since(*t) >= RECONNECT_WINDOW)
            {
                sessions.pop_front();
            }
            if sessions.len() >= MAX_SESSIONS_PER_WINDOW {
                warn!(
                    "Voice WS to {} dropped {} times within {:?}; giving up",
                    url,
                    sessions.len(),
                    RECONNECT_WINDOW
                );
                break;
            }
            let connecting = tokio::time::timeout(
                CONNECT_TIMEOUT,
                connect_async_tls_with_config(
                    &url,
                    None,
                    false,
                    Some(Connector::Rustls(TLS_CONFIG.clone())),
                ),
            );
            let connected = tokio::select! {
                result = connecting => result,
                () = self.park(&mut cmd_rx, &mut deferred) => break,
            };
            let ws = match connected {
                Ok(Ok((ws, _))) => ws,
                Ok(Err(e)) => {
                    warn!("Voice WS connect to {} failed: {}", url, e);
                    failed_connects += 1;
                    if failed_connects >= MAX_CONNECT_ATTEMPTS
                        || self.pause(&mut cmd_rx, &mut deferred).await
                    {
                        break;
                    }
                    continue;
                }
                Err(_) => {
                    warn!("Voice WS connect to {} timed out", url);
                    failed_connects += 1;
                    if failed_connects >= MAX_CONNECT_ATTEMPTS
                        || self.pause(&mut cmd_rx, &mut deferred).await
                    {
                        break;
                    }
                    continue;
                }
            };
            debug!("Voice WS connected to {}", url);
            sessions.push_back(Instant::now());

            let code = self.session(ws, resuming, &mut cmd_rx, &mut deferred).await;
            last_code = code;
            let can_resume = code.is_none_or(|c| c == 4015 || c < 4000);
            if self.closed || !can_resume {
                break;
            }
            if self.established {
                failed_connects = 0;
            } else {
                failed_connects += 1;
                if failed_connects >= MAX_CONNECT_ATTEMPTS {
                    break;
                }
            }
            // A session that never reached READY has nothing to resume; identify again.
            resuming |= self.established;
            if self.pause(&mut cmd_rx, &mut deferred).await {
                break;
            }
        }

        self.closed = true;
        if let Some(shared) = &self.shared {
            shared.webrtc.lock().await.close().await;
        }
        if let Some(tx) = self.ready_tx.take() {
            let _ = tx.send(Err(last_error.unwrap_or(ConnectionError::Closed)));
        }
        let _ = self.event_tx.send(VoiceEvent::Closed { code: last_code });
    }

    /// Handles commands while no socket is up; returns once the connection is stopped.
    async fn park(
        &mut self,
        cmd_rx: &mut mpsc::UnboundedReceiver<Command>,
        deferred: &mut Vec<Command>,
    ) {
        loop {
            match cmd_rx.recv().await {
                Some(Command::Stop) | None => {
                    self.closed = true;
                    return;
                }
                Some(Command::SetSession(session_id)) => self.session_id = Some(session_id),
                Some(Command::SetTokens { server, token }) => {
                    self.server = Some(server);
                    self.token = Some(token);
                }
                Some(cmd) => {
                    if deferred.len() < MAX_DEFERRED_COMMANDS {
                        deferred.push(cmd);
                    }
                }
            }
        }
    }

    /// Waits `RECONNECT_DELAY` unless stopped first; true when stopped.
    async fn pause(
        &mut self,
        cmd_rx: &mut mpsc::UnboundedReceiver<Command>,
        deferred: &mut Vec<Command>,
    ) -> bool {
        tokio::select! {
            () = tokio::time::sleep(RECONNECT_DELAY) => false,
            () = self.park(cmd_rx, deferred) => true,
        }
    }

    /// Drive one WebSocket until it closes; returns its close code.
    async fn session(
        &mut self,
        ws: Ws,
        resuming: bool,
        cmd_rx: &mut mpsc::UnboundedReceiver<Command>,
        deferred: &mut Vec<Command>,
    ) -> Option<u16> {
        let (mut sink, mut stream) = ws.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
        let first = if resuming {
            self.build_resume()
        } else {
            self.build_identify()
        };
        if sink.send(Message::Text(first)).await.is_err() {
            return None;
        }
        self.heartbeat_interval = None;
        self.unacked_heartbeats = 0;
        self.established = false;
        let mut heartbeat: Option<tokio::time::Interval> = None;
        for cmd in deferred.drain(..) {
            if let Err(code) = self.apply_command(cmd, &mut sink, &out_tx).await {
                return code;
            }
        }

        loop {
            let media_deadline = self.media_deadline.as_ref().map(|(at, _)| *at);
            tokio::select! {
                _ = tokio::time::sleep_until(media_deadline.unwrap_or_else(Instant::now)), if media_deadline.is_some() => {
                    self.on_media_deadline();
                }
                msg = stream.next() => match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Err(e) = self.handle_text(&text, &out_tx).await {
                            error!("Voice WS text error: {}", e);
                            // Before the caller has its connection, no message can still make this socket usable.
                            if let Some(ready) = self.ready_tx.take() {
                                let _ = ready.send(Err(e));
                                self.closed = true;
                                let _ = sink.send(Message::Close(None)).await;
                                return None;
                            }
                        }
                        if let Some(period) = self.heartbeat_interval.take() {
                            heartbeat = Some(tokio::time::interval_at(
                                tokio::time::Instant::now() + period,
                                period,
                            ));
                        }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        if let Err(e) = self.handle_binary(&data, &out_tx) {
                            error!("Voice WS binary error: {}", e);
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        debug!("Voice WS closed: {:?}", frame);
                        return frame.map(|f| u16::from(f.code));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        debug!("Voice WS error: {}", e);
                        return None;
                    }
                    None => return None,
                },
                Some(msg) = out_rx.recv() => {
                    if sink.send(msg).await.is_err() {
                        return None;
                    }
                }
                _ = tick(&mut heartbeat) => {
                    if self.unacked_heartbeats >= MAX_UNACKED_HEARTBEATS {
                        warn!("Voice WS missed {} heartbeat ACKs, reconnecting", self.unacked_heartbeats);
                        return None;
                    }
                    self.unacked_heartbeats += 1;
                    let msg = text(
                        VoiceOpCode::Heartbeat,
                        json!({ "t": now_ms(), "seq_ack": self.seq }),
                    );
                    if sink.send(msg).await.is_err() {
                        return None;
                    }
                }
                cmd = cmd_rx.recv() => {
                    let cmd = cmd.unwrap_or(Command::Stop);
                    if let Err(code) = self.apply_command(cmd, &mut sink, &out_tx).await {
                        return code;
                    }
                }
            }
        }
    }

    /// `Err(code)` ends the session with that close code.
    async fn apply_command(
        &mut self,
        cmd: Command,
        sink: &mut WsSink,
        out_tx: &mpsc::UnboundedSender<Message>,
    ) -> Result<(), Option<u16>> {
        match cmd {
            Command::Send(msg) => sink.send(msg).await.map_err(|_| None),
            Command::SetSession(session_id) => {
                self.session_id = Some(session_id);
                Ok(())
            }
            Command::SetTokens { server, token } => {
                self.server = Some(server);
                self.token = Some(token);
                Ok(())
            }
            Command::PeerState(state) => {
                self.on_peer_state(state, out_tx).await;
                Ok(())
            }
            Command::Stop => {
                self.closed = true;
                let _ = sink.send(Message::Close(None)).await;
                Err(Some(1000))
            }
        }
    }

    async fn on_peer_state(
        &mut self,
        state: RTCPeerConnectionState,
        out_tx: &mpsc::UnboundedSender<Message>,
    ) {
        debug!("peer connection state {state}");
        match state {
            RTCPeerConnectionState::Connected => {
                self.media_deadline = None;
                self.media_failed = false;
            }
            RTCPeerConnectionState::Disconnected => {
                if self.media_deadline.is_none() && !self.media_failed {
                    let grace = self.media_timeouts.disconnected_grace;
                    let at = Instant::now() + grace;
                    let reason = format!("peer connection disconnected for {grace:?}");
                    self.media_deadline = Some((at, reason));
                }
            }
            RTCPeerConnectionState::Failed => {
                if self.media_failed {
                    return;
                }
                // Whether Discord accepts a second SELECT_PROTOCOL on a live session is unverified; the deadline covers a refusal.
                let limit = self.media_timeouts.restart_timeout;
                let reason = format!(
                    "peer connection failed and its restart did not connect within {limit:?}"
                );
                self.media_deadline = Some((Instant::now() + limit, reason));
                warn!("peer connection failed; restarting it with a new SELECT_PROTOCOL");
                if let Err(e) = self.start_webrtc(out_tx).await {
                    error!("WebRTC restart failed: {}", e);
                    self.on_media_deadline();
                }
            }
            _ => {}
        }
    }

    fn on_media_deadline(&mut self) {
        let Some((_, reason)) = self.media_deadline.take() else {
            return;
        };
        if self.media_failed {
            return;
        }
        self.media_failed = true;
        warn!("media path dead: {reason}");
        let _ = self.event_tx.send(VoiceEvent::MediaFailed { reason });
    }

    fn build_identify(&self) -> String {
        json!({
            "op": VoiceOpCode::Identify as u8,
            "d": {
                "server_id": self.server_id(),
                "user_id": self.bot_id,
                "session_id": self.session_id,
                "token": self.token,
                "video": true,
                "streams": [{ "type": "screen", "rid": "100", "quality": 100 }],
                "max_dave_protocol_version": davey::DAVE_PROTOCOL_VERSION
            }
        })
        .to_string()
    }

    fn build_resume(&self) -> String {
        json!({
            "op": VoiceOpCode::Resume as u8,
            "d": {
                "server_id": self.server_id(),
                "session_id": self.session_id,
                "token": self.token,
                "seq_ack": self.seq
            }
        })
        .to_string()
    }

    fn dave(&self) -> MutexGuard<'_, DaveHandler> {
        lock(&self.dave)
    }

    fn send_opcode(&self, tx: &mpsc::UnboundedSender<Message>, op: VoiceOpCode, d: Value) {
        let _ = tx.send(text(op, d));
    }

    fn send_binary(
        &self,
        tx: &mpsc::UnboundedSender<Message>,
        op: VoiceOpCodeBinary,
        payload: &[u8],
    ) {
        let frame = BinaryMessage::encode(op.as_u8(), payload);
        let _ = tx.send(Message::Binary(frame));
    }

    /// `initDave()`: (re)initialise for the current protocol version and send a key package.
    fn init_dave(&self, protocol_version: u16, tx: &mpsc::UnboundedSender<Message>) {
        match self.dave().init(protocol_version) {
            Ok(Some(kp)) => self.send_binary(tx, VoiceOpCodeBinary::MlsKeyPackage, &kp),
            Ok(None) => {}
            Err(e) => error!("DAVE init failed: {}", e),
        }
    }

    /// `setProtocols()`: new peer connection, offer, SELECT_PROTOCOL.
    async fn start_webrtc(
        &self,
        tx: &mpsc::UnboundedSender<Message>,
    ) -> Result<(), ConnectionError> {
        let (Some(shared), Some(params)) = (&self.shared, &self.params) else {
            return Err(ConnectionError::NotConnected);
        };
        let mut webrtc = shared.webrtc.lock().await;
        let pc = webrtc.init(params).await?;
        let cmd_tx = shared.cmd_tx.clone();
        pc.on_peer_connection_state_change(Box::new(move |state| {
            let _ = cmd_tx.send(Command::PeerState(state));
            Box::pin(async {})
        }));
        let sdp = webrtc.create_offer().await?;
        drop(webrtc);
        self.send_opcode(
            tx,
            VoiceOpCode::SelectProtocol,
            json!({
                "protocol": "webrtc",
                "codecs": codecs_payload(),
                "data": sdp,
                "sdp": sdp,
                "rtc_connection_id": uuid::Uuid::new_v4().to_string(),
            }),
        );
        Ok(())
    }

    async fn handle_text(
        &mut self,
        raw: &str,
        tx: &mpsc::UnboundedSender<Message>,
    ) -> Result<(), ConnectionError> {
        let msg: GatewayMessage = serde_json::from_str(raw)?;
        if let Some(seq) = msg.seq {
            self.seq = seq;
        }

        let op = VoiceOpCode::from_u8(msg.op);
        debug!("Voice WS op={:?}", op);

        match op {
            Some(VoiceOpCode::Hello) => {
                let hello: Hello = serde_json::from_value(msg.d)?;
                let interval = Duration::from_millis(hello.heartbeat_interval.max(0.0) as u64);
                self.heartbeat_interval =
                    Some(interval.clamp(MIN_HEARTBEAT_INTERVAL, MAX_HEARTBEAT_INTERVAL));
            }
            Some(VoiceOpCode::Ready) => {
                self.established = true;
                let ready: Ready = serde_json::from_value(msg.d)?;
                let stream = ready
                    .streams
                    .first()
                    .ok_or(ConnectionError::Protocol("READY names no video stream"))?;
                let params = WebRtcParams {
                    address: ready.ip.clone(),
                    port: ready.port,
                    audio_ssrc: ready.ssrc,
                    video_ssrc: stream.ssrc,
                    rtx_ssrc: stream.rtx_ssrc,
                    supported_encryption_modes: ready.modes.clone(),
                };
                if let Some(shared) = &self.shared {
                    *lock(&shared.params) = Some(params.clone());
                }
                self.params = Some(params.clone());
                let _ = self.event_tx.send(VoiceEvent::Ready(params.clone()));
                self.start_webrtc(tx).await?;
                self.send_opcode(tx, VoiceOpCode::Video, video_payload(&params, None));
            }
            Some(VoiceOpCode::SelectProtocolAck) => {
                let ack: SelectProtocolAck = serde_json::from_value(msg.d)?;
                let Some(sdp) = ack.sdp else {
                    return Err(ConnectionError::Protocol(
                        "SELECT_PROTOCOL_ACK without SDP; only WebRTC is supported",
                    ));
                };
                let dave_version = ack.dave_protocol_version.unwrap_or(0);
                self.init_dave(dave_version, tx);
                if let Some(shared) = &self.shared
                    && let Err(e) = shared.webrtc.lock().await.set_remote_answer(&sdp).await
                {
                    if let Some(ready) = self.ready_tx.take() {
                        let _ = ready.send(Err(ConnectionError::WebRtc(WebRtcError::Webrtc(
                            e.to_string(),
                        ))));
                    }
                    return Err(e.into());
                }
                let _ = self
                    .event_tx
                    .send(VoiceEvent::SelectProtocolAck { sdp, dave_version });
                if let (Some(ready), Some(shared)) = (self.ready_tx.take(), &self.shared) {
                    let _ = ready.send(Ok(MediaConnection {
                        shared: shared.clone(),
                    }));
                }
            }
            Some(VoiceOpCode::Resumed) => {
                self.established = true;
                let _ = self.event_tx.send(VoiceEvent::Resumed);
            }
            Some(VoiceOpCode::ClientsConnect) => {
                if let Ok(cc) = serde_json::from_value::<ClientsConnect>(msg.d) {
                    let room = MAX_CONNECTED_USERS.saturating_sub(self.connected_users.len());
                    self.connected_users
                        .extend(cc.user_ids.into_iter().take(room));
                }
            }
            Some(VoiceOpCode::ClientDisconnect) => {
                if let Ok(cd) = serde_json::from_value::<ClientDisconnect>(msg.d) {
                    self.connected_users.remove(&cd.user_id);
                }
            }
            Some(VoiceOpCode::DavePrepareTransition) => {
                if let Ok(d) = serde_json::from_value::<DavePrepareTransition>(msg.d) {
                    debug!("DAVE prepare transition {:?}", d);
                    self.remember_transition(d.transition_id, d.protocol_version);
                    if d.transition_id == 0 {
                        self.execute_pending_transition(0);
                    } else {
                        if d.protocol_version == 0 {
                            self.dave().set_passthrough_mode(true, Some(120));
                        }
                        self.send_opcode(
                            tx,
                            VoiceOpCode::DaveTransitionReady,
                            json!({ "transition_id": d.transition_id }),
                        );
                    }
                }
            }
            Some(VoiceOpCode::DaveExecuteTransition) => {
                if let Ok(d) = serde_json::from_value::<DaveExecuteTransition>(msg.d) {
                    self.execute_pending_transition(d.transition_id);
                }
            }
            Some(VoiceOpCode::DavePrepareEpoch) => {
                if let Ok(d) = serde_json::from_value::<DavePrepareEpoch>(msg.d) {
                    debug!("DAVE prepare epoch {:?}", d);
                    if d.epoch == 1 {
                        self.init_dave(d.protocol_version, tx);
                    }
                }
            }
            Some(VoiceOpCode::HeartbeatAck) => self.unacked_heartbeats = 0,
            Some(VoiceOpCode::Speaking) => {}
            _ => {
                debug!("Unhandled voice op {}", msg.op);
            }
        }

        Ok(())
    }

    fn handle_binary(
        &mut self,
        data: &[u8],
        tx: &mpsc::UnboundedSender<Message>,
    ) -> Result<(), ConnectionError> {
        let frame = match BinaryMessage::parse(data) {
            Some(f) => f,
            None => return Ok(()),
        };
        self.seq = frame.seq as i64;

        match VoiceOpCodeBinary::from_u8(frame.op) {
            Some(VoiceOpCodeBinary::MlsExternalSender) => {
                self.dave().set_external_sender(&frame.payload)?;
            }
            Some(VoiceOpCodeBinary::MlsProposals) => {
                if frame.payload.is_empty() {
                    return Ok(());
                }
                let op_type = frame.payload[0];
                let proposals = &frame.payload[1..];
                let user_ids: Vec<u64> = self
                    .connected_users
                    .iter()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                let result = self
                    .dave()
                    .process_proposals(op_type, proposals, &user_ids)?;
                if let Some(cw) = result {
                    let mut payload = cw.commit.clone();
                    if let Some(w) = cw.welcome {
                        payload.extend_from_slice(&w);
                    }
                    self.send_binary(tx, VoiceOpCodeBinary::MlsCommitWelcome, &payload);
                    let _ = self.event_tx.send(VoiceEvent::MlsCommitWelcome(payload));
                }
            }
            Some(
                op @ (VoiceOpCodeBinary::MlsAnnounceCommitTransition
                | VoiceOpCodeBinary::MlsWelcome),
            ) => {
                if frame.payload.len() < 2 {
                    return Ok(());
                }
                let transition_id = u16::from_be_bytes([frame.payload[0], frame.payload[1]]) as u64;
                let body = &frame.payload[2..];
                let result = if op == VoiceOpCodeBinary::MlsWelcome {
                    self.dave().process_welcome(body)
                } else {
                    self.dave().process_commit(body)
                };
                match result {
                    Ok(()) => {
                        if transition_id > 0 {
                            let version = self.dave().protocol_version();
                            self.remember_transition(transition_id, version);
                            self.send_opcode(
                                tx,
                                VoiceOpCode::DaveTransitionReady,
                                json!({ "transition_id": transition_id }),
                            );
                        }
                        debug!("DAVE {:?} processed (transition {})", op, transition_id);
                    }
                    Err(e) => {
                        warn!("DAVE {:?} error: {}", op, e);
                        self.process_invalid_commit(transition_id, tx);
                    }
                }
            }
            _ => {
                debug!("Unhandled binary voice op {}", frame.op);
            }
        }

        Ok(())
    }

    fn remember_transition(&mut self, transition_id: u64, protocol_version: u16) {
        self.pending_transitions
            .insert(transition_id, protocol_version);
        while self.pending_transitions.len() > MAX_PENDING_TRANSITIONS {
            let Some(oldest) = self.pending_transitions.keys().min().copied() else {
                break;
            };
            self.pending_transitions.remove(&oldest);
        }
    }

    fn execute_pending_transition(&mut self, transition_id: u64) {
        if let Some(new_version) = self.pending_transitions.remove(&transition_id) {
            let mut dave = lock(&self.dave);
            let old_version = dave.protocol_version();
            dave.set_protocol_version(new_version);
            if old_version != new_version && new_version == 0 {
                self.dave_downgraded = true;
                debug!(
                    "DAVE: Downgraded to non-E2EE (transition {})",
                    transition_id
                );
            } else if transition_id > 0 && self.dave_downgraded {
                self.dave_downgraded = false;
                dave.set_passthrough_mode(true, Some(10));
                debug!("DAVE: Upgraded to E2EE (transition {})", transition_id);
            }
            debug!("DAVE: Executed pending transition {}", transition_id);
        } else {
            warn!("DAVE: Unrecognized transition ID {}", transition_id);
        }
    }

    fn process_invalid_commit(&mut self, transition_id: u64, tx: &mpsc::UnboundedSender<Message>) {
        warn!(
            "DAVE: Invalid commit (transition {}), reinitializing",
            transition_id
        );
        self.send_opcode(
            tx,
            VoiceOpCode::MlsInvalidCommitWelcome,
            json!({ "transition_id": transition_id }),
        );
        let version = self.dave().protocol_version();
        self.init_dave(version, tx);
    }
}

// Lock poisoning only means another thread panicked mid-call; the protected state stays usable.
fn lock<T>(m: &StdMutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn text(op: VoiceOpCode, d: Value) -> Message {
    Message::Text(json!({ "op": op as u8, "d": d }).to_string())
}

async fn tick(interval: &mut Option<tokio::time::Interval>) {
    match interval {
        Some(iv) => {
            iv.tick().await;
        }
        None => std::future::pending().await,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// `wss://{host}/?v=8` for a Discord voice host; a scheme in `endpoint` is dropped.
pub fn ws_url(endpoint: &str) -> Result<String, ConnectionError> {
    #[cfg(feature = "test-hooks")]
    if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
        return Ok(format!("{}/?v=8", endpoint.trim_end_matches('/')));
    }
    discord_ws_url(endpoint)
}

fn discord_ws_url(endpoint: &str) -> Result<String, ConnectionError> {
    discord_voice_authority(endpoint)
        .map(|authority| format!("wss://{authority}/?v=8"))
        .ok_or_else(|| ConnectionError::UntrustedEndpoint(endpoint.to_owned()))
}

/// The lower-cased `host[:port]` of `endpoint` when the host is under `discord.media` or `discord.gg`.
fn discord_voice_authority(endpoint: &str) -> Option<String> {
    let rest = endpoint
        .strip_prefix("wss://")
        .or_else(|| endpoint.strip_prefix("ws://"))
        .unwrap_or(endpoint);
    let authority = rest.split(['/', '?', '#']).next()?.to_ascii_lowercase();
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority.as_str(), None),
    };
    if port.is_some_and(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let dns_name = !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-');
    let discord = ["discord.media", "discord.gg"]
        .iter()
        .any(|d| host == *d || host.strip_suffix(d).is_some_and(|h| h.ends_with('.')));
    (dns_name && discord).then_some(authority)
}

/// `Object.values(CodecPayloadType)` as sent in SELECT_PROTOCOL.
pub fn codecs_payload() -> Value {
    let opus = codec_payload::OPUS;
    let mut codecs = vec![json!({
        "name": opus.name,
        "type": "audio",
        "clockRate": opus.clock_rate,
        "priority": 1000,
        "payload_type": opus.payload_type,
    })];
    codecs.extend(ALL_VIDEO_CODECS.iter().map(|c| {
        json!({
            "name": c.name,
            "type": "video",
            "clockRate": c.clock_rate,
            "priority": c.priority,
            "payload_type": c.payload_type,
            "rtx_payload_type": c.rtx_payload_type,
            "encode": true,
            "decode": true,
        })
    }));
    Value::Array(codecs)
}

/// Op 12 body of `setVideoAttributes()`.
pub fn video_payload(params: &WebRtcParams, attrs: Option<VideoAttributes>) -> Value {
    match attrs {
        None => json!({
            "audio_ssrc": params.audio_ssrc,
            "video_ssrc": 0,
            "rtx_ssrc": 0,
            "streams": [],
        }),
        Some(a) => json!({
            "audio_ssrc": params.audio_ssrc,
            "video_ssrc": params.video_ssrc,
            "rtx_ssrc": params.rtx_ssrc,
            "streams": [{
                "type": "video",
                "rid": "100",
                "ssrc": params.video_ssrc,
                "active": true,
                "quality": 100,
                "rtx_ssrc": params.rtx_ssrc,
                "max_bitrate": 10_000 * 1000,
                "max_framerate": a.fps,
                "max_resolution": { "type": "fixed", "width": a.width, "height": a.height },
            }],
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> VoiceConnection {
        let (tx, _rx) = mpsc::unbounded_channel();
        VoiceConnection::new(Some("1".into()), "2".into(), "3".into(), tx).unwrap()
    }

    #[test]
    fn identify_advertises_dave() {
        let v: Value = serde_json::from_str(&conn().build_identify()).unwrap();
        assert_eq!(v["d"]["max_dave_protocol_version"], 1);
        assert_eq!(v["d"]["server_id"], "1");
    }

    #[test]
    fn stream_identifies_with_rtc_server_id() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let c =
            VoiceConnection::new_stream(Some("1".into()), "2".into(), "3".into(), "999".into(), tx)
                .unwrap();
        let v: Value = serde_json::from_str(&c.build_identify()).unwrap();
        assert_eq!(v["d"]["server_id"], "999");
    }

    #[test]
    fn unusable_dave_channel_ids_are_rejected() {
        for id in ["0", "x"] {
            let (tx, _rx) = mpsc::unbounded_channel();
            let c = VoiceConnection::new_stream(None, "2".into(), "3".into(), id.into(), tx);
            assert!(matches!(c, Err(ConnectionError::InvalidChannelId(_))));
        }
        let (tx, _rx) = mpsc::unbounded_channel();
        let c = VoiceConnection::new(None, "x".into(), "3".into(), tx);
        assert!(matches!(c, Err(ConnectionError::InvalidChannelId(_))));
    }

    #[test]
    fn executed_transitions_switch_protocol_version() {
        let mut conn = conn();
        conn.dave().set_protocol_version(1);

        conn.pending_transitions.insert(5, 0);
        conn.execute_pending_transition(5);
        assert_eq!(conn.dave().protocol_version(), 0);
        assert!(conn.dave_downgraded);

        conn.pending_transitions.insert(6, 1);
        conn.execute_pending_transition(6);
        assert_eq!(conn.dave().protocol_version(), 1);
        assert!(!conn.dave_downgraded);
    }

    #[test]
    fn debug_output_never_carries_the_answer_sdp() {
        let event = VoiceEvent::SelectProtocolAck {
            sdp: "a=ice-pwd:secret".into(),
            dave_version: 1,
        };
        let shown = format!("{event:?}");
        assert!(!shown.contains("secret"), "{shown}");
        assert!(shown.contains("sdp_len"));
    }

    #[test]
    fn pending_transitions_keep_the_newest_ids_only() {
        let mut conn = conn();
        for id in 1..=(MAX_PENDING_TRANSITIONS as u64 + 4) {
            conn.remember_transition(id, 1);
        }
        assert_eq!(conn.pending_transitions.len(), MAX_PENDING_TRANSITIONS);
        assert!(!conn.pending_transitions.contains_key(&1));
        assert!(
            conn.pending_transitions
                .contains_key(&(MAX_PENDING_TRANSITIONS as u64 + 4))
        );
    }

    #[test]
    fn select_protocol_codecs_match_js_payload_types() {
        let codecs = codecs_payload();
        let pts: Vec<(String, u64)> = codecs
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c["name"].as_str().unwrap().to_owned(),
                    c["payload_type"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            pts,
            [
                ("opus", 120),
                ("H264", 101),
                ("H265", 103),
                ("VP8", 105),
                ("VP9", 107),
                ("AV1", 109)
            ]
            .map(|(n, p)| (n.to_owned(), p))
        );
        assert_eq!(codecs[1]["rtx_payload_type"], 102);
        assert_eq!(codecs[0]["clockRate"], 48_000);
    }

    #[test]
    fn video_attributes_payload() {
        let params = WebRtcParams {
            address: String::new(),
            port: 0,
            audio_ssrc: 10,
            video_ssrc: 11,
            rtx_ssrc: 12,
            supported_encryption_modes: vec![],
        };
        let off = video_payload(&params, None);
        assert_eq!(off["video_ssrc"], 0);
        assert_eq!(off["streams"], json!([]));
        let on = video_payload(
            &params,
            Some(VideoAttributes {
                width: 1920,
                height: 1080,
                fps: 60,
            }),
        );
        assert_eq!(on["streams"][0]["ssrc"], 11);
        assert_eq!(on["streams"][0]["max_framerate"], 60);
        assert_eq!(on["streams"][0]["max_resolution"]["width"], 1920);
    }

    #[test]
    fn discord_hosts_get_wss_whatever_scheme_the_gateway_sent() {
        for endpoint in [
            "c-ams.discord.media:443",
            "wss://c-ams.discord.media:443",
            "ws://c-ams.discord.media:443",
            "C-AMS.Discord.Media:443/",
        ] {
            assert_eq!(
                discord_ws_url(endpoint).unwrap(),
                "wss://c-ams.discord.media:443/?v=8",
                "{endpoint}"
            );
        }
        assert_eq!(
            discord_ws_url("rtc.discord.gg").unwrap(),
            "wss://rtc.discord.gg/?v=8"
        );
    }

    #[test]
    fn hosts_outside_discord_are_refused() {
        for endpoint in [
            "attacker.example",
            "ws://attacker.example:80",
            "wss://attacker.example",
            "discord.media.attacker.example",
            "notdiscord.media",
            "c-ams.discord.media@attacker.example",
            "attacker.example@c-ams.discord.media",
            "c-ams.discord.media:443x",
            "127.0.0.1:9000",
            "",
        ] {
            assert!(
                matches!(
                    discord_ws_url(endpoint),
                    Err(ConnectionError::UntrustedEndpoint(_))
                ),
                "{endpoint}"
            );
        }
    }

    #[cfg(feature = "test-hooks")]
    #[test]
    fn test_builds_keep_explicit_local_endpoints() {
        assert_eq!(
            ws_url("ws://127.0.0.1:9000/").unwrap(),
            "ws://127.0.0.1:9000/?v=8"
        );
        assert!(ws_url("127.0.0.1:9000").is_err());
    }

    #[test]
    fn tls_config_builds_with_ring() {
        assert!(!TLS_CONFIG.crypto_provider().cipher_suites.is_empty());
    }
}
