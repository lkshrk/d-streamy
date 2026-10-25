use std::sync::{Arc, PoisonError};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use secrecy::ExposeSecret;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::{Instant, sleep, sleep_until, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

use super::event::{CurrentUser, Dispatch, GatewayEvent, VoiceState};
use super::inflate::ZlibStream;
use super::options::{GatewayOptions, connect_url};
use super::{GatewayError, Shared};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Sink = futures_util::stream::SplitSink<Socket, Message>;
type Stream = futures_util::stream::SplitStream<Socket>;

const OP_DISPATCH: u8 = 0;
const OP_HEARTBEAT: u8 = 1;
const OP_IDENTIFY: u8 = 2;
const OP_RESUME: u8 = 6;
const OP_RECONNECT: u8 = 7;
const OP_INVALID_SESSION: u8 = 9;
const OP_HELLO: u8 = 10;
const OP_HEARTBEAT_ACK: u8 = 11;

/// Close code that keeps the session resumable (1000/1001 end it).
const CLOSE_RESUMABLE: u16 = 4000;
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_HANDSHAKE: Duration = Duration::from_secs(2);
const RATE_LIMIT: u32 = 120;
const RATE_RESERVED: u32 = 10;
const RATE_WINDOW: Duration = Duration::from_secs(60);
/// Caps HELLO's interval so `Instant` arithmetic cannot overflow.
const MAX_HEARTBEAT_INTERVAL_MS: u64 = 10 * 60 * 1000;

#[derive(Deserialize)]
struct Packet {
    op: u8,
    #[serde(default)]
    d: Value,
    #[serde(default)]
    s: Option<u64>,
    #[serde(default)]
    t: Option<String>,
}

#[derive(Deserialize)]
struct ReadyData {
    user: CurrentUser,
    session_id: String,
    resume_gateway_url: Option<String>,
}

struct Session {
    id: String,
    resume_url: Option<String>,
}

enum End {
    Shutdown,
    Fatal(GatewayError),
    Reconnect {
        code: Option<u16>,
        resume: bool,
        healthy: bool,
    },
}

struct Heartbeat {
    interval: Duration,
    next: Instant,
    acked: bool,
    sent_at: Option<Instant>,
}

struct RateLimit {
    window_start: Instant,
    sent: u32,
}

impl RateLimit {
    fn new() -> Self {
        Self {
            window_start: Instant::now(),
            sent: 0,
        }
    }

    fn window_end(&self) -> Instant {
        self.window_start + RATE_WINDOW
    }

    fn roll(&mut self) {
        if Instant::now() >= self.window_end() {
            self.window_start = Instant::now();
            self.sent = 0;
        }
    }

    fn commands_allowed(&self) -> bool {
        self.sent < RATE_LIMIT - RATE_RESERVED
    }
}

pub(crate) struct Runner {
    pub(crate) opts: GatewayOptions,
    pub(crate) connector: Connector,
    pub(crate) shared: Arc<Shared>,
    pub(crate) events: broadcast::Sender<GatewayEvent>,
    pub(crate) commands: mpsc::UnboundedReceiver<Value>,
    pub(crate) shutdown: watch::Receiver<()>,
    pub(crate) ready_tx: Option<oneshot::Sender<Result<CurrentUser, GatewayError>>>,
    session: Option<Session>,
    seq: Option<u64>,
    unsent: Option<Value>,
}

impl Runner {
    pub(crate) fn new(
        opts: GatewayOptions,
        connector: Connector,
        shared: Arc<Shared>,
        events: broadcast::Sender<GatewayEvent>,
        commands: mpsc::UnboundedReceiver<Value>,
        shutdown: watch::Receiver<()>,
        ready_tx: oneshot::Sender<Result<CurrentUser, GatewayError>>,
    ) -> Self {
        Self {
            opts,
            connector,
            shared,
            events,
            commands,
            shutdown,
            ready_tx: Some(ready_tx),
            session: None,
            seq: None,
            unsent: None,
        }
    }

    pub(crate) async fn run(mut self) {
        let mut failures = 0u32;
        let error = loop {
            let base = self
                .session
                .as_ref()
                .and_then(|s| s.resume_url.clone())
                .filter(|u| u.starts_with("wss://") || !self.opts.url.starts_with("wss://"))
                .unwrap_or_else(|| self.opts.url.clone());
            let url = connect_url(&base, self.opts.compress);
            let opened = tokio::select! {
                _ = self.shutdown.changed() => None,
                socket = open(&url, &self.connector, self.opts.handshake_timeout) => Some(socket),
            };
            let end = match opened {
                None => End::Shutdown,
                Some(Ok(socket)) => self.session_loop(socket).await,
                Some(Err(e)) => {
                    warn!("gateway connect failed: {e}");
                    End::Reconnect {
                        code: None,
                        resume: true,
                        healthy: false,
                    }
                }
            };
            match end {
                End::Shutdown => break GatewayError::Closed,
                End::Fatal(e) => break e,
                End::Reconnect {
                    code,
                    resume,
                    healthy,
                } => {
                    if !resume {
                        self.session = None;
                        self.seq = None;
                    }
                    let resume = self.session.is_some();
                    self.emit(GatewayEvent::Disconnected { code, resume });
                    failures = if healthy { 0 } else { failures + 1 };
                    if failures >= self.opts.max_failed_attempts {
                        break GatewayError::Exhausted(failures);
                    }
                    let delay = self.backoff(failures.max(1));
                    info!(?code, resume, ?delay, "gateway reconnecting");
                    tokio::select! {
                        _ = self.shutdown.changed() => break GatewayError::Closed,
                        _ = sleep(delay) => {}
                    }
                }
            }
        };
        let dropped = usize::from(self.unsent.is_some()) + self.commands.len();
        if dropped > 0 {
            warn!(dropped, "gateway closed with commands unsent");
        }
        if let Some(tx) = self.ready_tx.take() {
            let _ = tx.send(Err(error.clone()));
        }
        self.emit(GatewayEvent::Closed(error));
    }

    fn backoff(&self, failures: u32) -> Duration {
        let exp = self
            .opts
            .reconnect_delay_min
            .saturating_mul(1 << (failures - 1).min(16));
        exp.min(self.opts.reconnect_delay_max)
            .mul_f64(rand::rng().random_range(0.5..=1.0))
    }

    async fn session_loop(&mut self, socket: Socket) -> End {
        let (mut sink, mut stream) = socket.split();
        let mut inflate = self.opts.compress.then(ZlibStream::new);
        let hello_deadline = Instant::now() + self.opts.hello_timeout;
        let ready_deadline = Instant::now() + self.opts.ready_timeout;
        let mut heartbeat: Option<Heartbeat> = None;
        let mut identify_at: Option<Instant> = None;
        let mut rate = RateLimit::new();
        let mut ready = false;
        let mut ever_ready = false;
        loop {
            ever_ready |= ready;
            rate.roll();
            let commands_open = ready && rate.commands_allowed();
            let heartbeat_at = heartbeat.as_ref().map(|h| h.next);
            let step = tokio::select! {
                _ = self.shutdown.changed() => {
                    if ready {
                        self.flush(&mut sink, &mut rate).await;
                    }
                    close_handshake(&mut sink, &mut stream).await;
                    return End::Shutdown;
                }
                msg = stream.next() => match msg {
                    None => Step::End(self.lost(None, ready)),
                    Some(Err(e)) => {
                        debug!("gateway read failed: {e}");
                        Step::End(self.lost(None, ready))
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let code = frame.map(|f| u16::from(f.code));
                        Step::End(self.lost(code, ready))
                    }
                    Some(Ok(Message::Text(text))) => Step::Packet(text.into_bytes()),
                    Some(Ok(Message::Binary(bytes))) => match inflate.as_mut() {
                        Some(z) => match z.push(&bytes) {
                            Ok(Some(raw)) => Step::Packet(raw),
                            Ok(None) => Step::Idle,
                            Err(e) => {
                                warn!("gateway zlib-stream error: {e}");
                                close(&mut sink, CLOSE_RESUMABLE).await;
                                Step::End(self.lost(None, ready))
                            }
                        },
                        None => Step::Packet(bytes),
                    },
                    Some(Ok(_)) => Step::Idle,
                },
                _ = sleep_until(heartbeat_at.unwrap_or_else(Instant::now)), if heartbeat_at.is_some() => {
                    Step::HeartbeatDue
                }
                _ = sleep_until(hello_deadline), if heartbeat.is_none() => {
                    warn!("gateway sent no HELLO in time");
                    close(&mut sink, CLOSE_RESUMABLE).await;
                    Step::End(self.lost(None, false))
                }
                _ = sleep_until(ready_deadline), if !ever_ready => {
                    warn!("gateway sent no READY or RESUMED in time");
                    close(&mut sink, CLOSE_RESUMABLE).await;
                    Step::End(self.lost(None, false))
                }
                _ = sleep_until(identify_at.unwrap_or_else(Instant::now)), if identify_at.is_some() => {
                    identify_at = None;
                    Step::Identify
                }
                _ = sleep_until(rate.window_end()), if ready && !rate.commands_allowed() => Step::Idle,
                _ = std::future::ready(()), if commands_open && self.unsent.is_some() => {
                    Step::Send(self.unsent.take().unwrap_or_default())
                }
                cmd = self.commands.recv(), if commands_open && self.unsent.is_none() => match cmd {
                    Some(payload) => Step::Send(payload),
                    None => {
                        close_handshake(&mut sink, &mut stream).await;
                        return End::Shutdown;
                    }
                },
            };
            let result = match step {
                Step::Idle => Ok(()),
                Step::End(end) => return end,
                Step::Send(payload) => {
                    let sent = send(&mut sink, &mut rate, &payload).await;
                    if sent.is_err() {
                        self.unsent = Some(payload);
                    }
                    sent
                }
                Step::Identify => self.identify(&mut sink, &mut rate).await,
                Step::HeartbeatDue => {
                    let Some(hb) = heartbeat.as_mut() else {
                        continue;
                    };
                    if !hb.acked {
                        warn!("gateway heartbeat not acknowledged, reconnecting");
                        close(&mut sink, CLOSE_RESUMABLE).await;
                        return End::Reconnect {
                            code: None,
                            resume: true,
                            healthy: ready,
                        };
                    }
                    hb.acked = false;
                    hb.sent_at = Some(Instant::now());
                    hb.next = Instant::now() + hb.interval;
                    send(&mut sink, &mut rate, &self.heartbeat_payload()).await
                }
                Step::Packet(raw) => {
                    let packet = match serde_json::from_slice::<Packet>(&raw) {
                        Ok(p) => p,
                        Err(e) => {
                            warn!("gateway sent an unreadable packet: {e}");
                            continue;
                        }
                    };
                    match self
                        .on_packet(
                            packet,
                            &mut sink,
                            &mut rate,
                            &mut heartbeat,
                            &mut identify_at,
                            &mut ready,
                        )
                        .await
                    {
                        Ok(Some(end)) => return end,
                        Ok(None) => Ok(()),
                        Err(e) => Err(e),
                    }
                }
            };
            if let Err(e) = result {
                debug!("gateway write failed: {e}");
                return End::Reconnect {
                    code: None,
                    resume: true,
                    healthy: ready,
                };
            }
        }
    }

    fn lost(&self, code: Option<u16>, healthy: bool) -> End {
        match code {
            Some(code @ (4004 | 4010..=4014)) => End::Fatal(GatewayError::Rejected(code)),
            Some(4007 | 4009) => End::Reconnect {
                code,
                resume: false,
                healthy,
            },
            _ => End::Reconnect {
                code,
                resume: true,
                healthy,
            },
        }
    }

    async fn on_packet(
        &mut self,
        packet: Packet,
        sink: &mut Sink,
        rate: &mut RateLimit,
        heartbeat: &mut Option<Heartbeat>,
        identify_at: &mut Option<Instant>,
        ready: &mut bool,
    ) -> Result<Option<End>, String> {
        if let Some(s) = packet.s {
            self.seq = Some(self.seq.map_or(s, |cur| cur.max(s)));
        }
        match packet.op {
            OP_HELLO => {
                let ms = packet
                    .d
                    .get("heartbeat_interval")
                    .and_then(Value::as_u64)
                    .unwrap_or(41_250)
                    .clamp(1, MAX_HEARTBEAT_INTERVAL_MS);
                let interval = Duration::from_millis(ms);
                let jitter = rand::rng().random_range(0.0..1.0);
                *heartbeat = Some(Heartbeat {
                    interval,
                    next: Instant::now() + interval.mul_f64(jitter),
                    acked: true,
                    sent_at: None,
                });
                if self.session.is_some() {
                    self.resume(sink, rate).await?;
                } else {
                    self.identify(sink, rate).await?;
                }
            }
            OP_HEARTBEAT_ACK => {
                if let Some(hb) = heartbeat.as_mut() {
                    hb.acked = true;
                    if let Some(sent) = hb.sent_at.take() {
                        *self
                            .shared
                            .latency
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner) = Some(sent.elapsed());
                    }
                }
            }
            OP_HEARTBEAT => send(sink, rate, &self.heartbeat_payload()).await?,
            OP_RECONNECT => {
                info!("gateway asked to reconnect");
                close(sink, CLOSE_RESUMABLE).await;
                return Ok(Some(End::Reconnect {
                    code: None,
                    resume: true,
                    healthy: *ready,
                }));
            }
            OP_INVALID_SESSION => {
                let resumable = packet.d.as_bool().unwrap_or(false);
                info!(resumable, "gateway invalidated the session");
                self.emit(GatewayEvent::InvalidSession { resumable });
                *ready = false;
                if resumable && self.session.is_some() {
                    self.resume(sink, rate).await?;
                } else {
                    self.session = None;
                    self.seq = None;
                    let max = self.opts.invalid_session_delay;
                    let delay = max.mul_f64(rand::rng().random_range(0.2..=1.0));
                    *identify_at = Some(Instant::now() + delay);
                }
            }
            OP_DISPATCH => {
                if let Some(name) = packet.t {
                    self.on_dispatch(name, packet.s, packet.d, ready);
                }
            }
            op => debug!(op, "gateway opcode ignored"),
        }
        Ok(None)
    }

    fn on_dispatch(&mut self, name: String, seq: Option<u64>, data: Value, ready: &mut bool) {
        match name.as_str() {
            "READY" => match ReadyData::deserialize(&data) {
                Ok(r) => {
                    *ready = true;
                    info!(user = %r.user.id, "gateway ready");
                    self.session = Some(Session {
                        id: r.session_id.clone(),
                        resume_url: r.resume_gateway_url,
                    });
                    self.shared.set_session_id(r.session_id.clone());
                    *self
                        .shared
                        .user
                        .write()
                        .unwrap_or_else(PoisonError::into_inner) = Some(r.user.clone());
                    self.apply_cache(&name, &data);
                    // Broadcast before `connect` returns, so later subscribers never see the first READY.
                    self.emit(GatewayEvent::Ready {
                        user: r.user.clone(),
                        session_id: r.session_id,
                    });
                    if let Some(tx) = self.ready_tx.take() {
                        let _ = tx.send(Ok(r.user));
                    }
                }
                Err(e) => warn!("gateway READY unreadable: {e}"),
            },
            "RESUMED" => {
                *ready = true;
                info!("gateway resumed");
                self.emit(GatewayEvent::Resumed);
            }
            "VOICE_STATE_UPDATE" => {
                if let Ok(state) = VoiceState::deserialize(&data) {
                    self.emit(GatewayEvent::VoiceStateUpdate(state));
                }
            }
            _ => self.apply_cache(&name, &data),
        }
        let dispatch = Dispatch {
            name,
            seq,
            data: Arc::new(data),
        };
        if let Some(tx) = &self.opts.dispatch_tx {
            let _ = tx.send(dispatch.clone());
        }
        self.emit(GatewayEvent::Dispatch(dispatch));
    }

    fn apply_cache(&self, name: &str, data: &Value) {
        let changed = self
            .shared
            .cache
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .apply(name, data);
        if changed {
            self.emit(GatewayEvent::GuildsChanged);
        }
    }

    async fn identify(&self, sink: &mut Sink, rate: &mut RateLimit) -> Result<(), String> {
        debug!("gateway identify");
        let payload = json!({
            "op": OP_IDENTIFY,
            "d": {
                "token": self.opts.token.expose_secret(),
                "capabilities": self.opts.capabilities,
                "properties": self.opts.properties,
                "presence": {"status": "online", "since": 0, "activities": [], "afk": true},
                "compress": false,
                "client_state": {"guild_versions": {}},
            }
        });
        send(sink, rate, &payload).await
    }

    async fn resume(&self, sink: &mut Sink, rate: &mut RateLimit) -> Result<(), String> {
        let Some(session) = &self.session else {
            return self.identify(sink, rate).await;
        };
        debug!(seq = ?self.seq, "gateway resume");
        let payload = json!({
            "op": OP_RESUME,
            "d": {
                "token": self.opts.token.expose_secret(),
                "session_id": session.id,
                "seq": self.seq,
            }
        });
        send(sink, rate, &payload).await
    }

    fn heartbeat_payload(&self) -> Value {
        json!({"op": OP_HEARTBEAT, "d": self.seq})
    }

    fn emit(&self, event: GatewayEvent) {
        self.shared
            .lossless
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|tx| tx.send(event.clone()).is_ok());
        let _ = self.events.send(event);
    }
}

enum Step {
    Idle,
    End(End),
    Packet(Vec<u8>),
    Send(Value),
    Identify,
    HeartbeatDue,
}

impl Runner {
    /// Closing needs no heartbeat, so the slots reserved for one may carry queued commands.
    async fn flush(&mut self, sink: &mut Sink, rate: &mut RateLimit) {
        while rate.sent < RATE_LIMIT {
            let Some(payload) = self.unsent.take().or_else(|| self.commands.try_recv().ok()) else {
                return;
            };
            if send(sink, rate, &payload).await.is_err() {
                return;
            }
        }
    }
}

async fn send(sink: &mut Sink, rate: &mut RateLimit, payload: &Value) -> Result<(), String> {
    rate.roll();
    rate.sent += 1;
    match timeout(WRITE_TIMEOUT, sink.send(Message::Text(payload.to_string()))).await {
        Ok(result) => result.map_err(|e| e.to_string()),
        Err(_) => Err("write timed out".into()),
    }
}

async fn open(url: &str, connector: &Connector, limit: Duration) -> Result<Socket, String> {
    debug!("gateway connecting");
    let connect =
        tokio_tungstenite::connect_async_tls_with_config(url, None, false, Some(connector.clone()));
    match timeout(limit, connect).await {
        Ok(Ok((socket, _))) => Ok(socket),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("handshake timed out".into()),
    }
}

/// Closing TCP with frames unread sends RST, which can discard our last frames (RFC 6455 §1.4).
async fn close_handshake(sink: &mut Sink, stream: &mut Stream) {
    let _ = timeout(CLOSE_HANDSHAKE, async {
        close(sink, 1000).await;
        while let Some(Ok(msg)) = stream.next().await {
            if matches!(msg, Message::Close(_)) {
                return;
            }
        }
    })
    .await;
}

async fn close(sink: &mut Sink, code: u16) {
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: "".into(),
    };
    // The socket is being abandoned either way; a failed close frame changes nothing.
    let _ = timeout(WRITE_TIMEOUT, sink.send(Message::Close(Some(frame)))).await;
}
