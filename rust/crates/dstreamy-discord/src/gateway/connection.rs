use std::sync::{Arc, PoisonError};
use std::time::Duration;

use discord_stream_rs::GatewayOpCode;
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

const OP_DISPATCH: u8 = GatewayOpCode::Dispatch as u8;
const OP_HEARTBEAT: u8 = GatewayOpCode::Heartbeat as u8;
const OP_IDENTIFY: u8 = GatewayOpCode::Identify as u8;
const OP_RESUME: u8 = GatewayOpCode::Resume as u8;
const OP_RECONNECT: u8 = GatewayOpCode::Reconnect as u8;
const OP_INVALID_SESSION: u8 = GatewayOpCode::InvalidSession as u8;
const OP_HELLO: u8 = GatewayOpCode::Hello as u8;
const OP_HEARTBEAT_ACK: u8 = GatewayOpCode::HeartbeatAck as u8;

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
    Reconnect { code: Option<u16>, resume: bool },
}

/// A run of invalid sessions shares one deadline, which excuses only the run's first IDENTIFY wait.
#[derive(Clone, Copy)]
struct AuthDeadline {
    at: Instant,
    excused: bool,
}

impl AuthDeadline {
    fn new(at: Instant) -> Self {
        Self { at, excused: false }
    }
}

struct Heartbeat {
    interval: Duration,
    next: Instant,
    acked: bool,
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
    opts: GatewayOptions,
    connector: Connector,
    shared: Arc<Shared>,
    events: broadcast::Sender<GatewayEvent>,
    commands: mpsc::UnboundedReceiver<Value>,
    shutdown: watch::Receiver<()>,
    ready_tx: Option<oneshot::Sender<Result<CurrentUser, GatewayError>>>,
    session: Option<Session>,
    seq: Option<u64>,
    unsent: Option<Value>,
    authenticated_since: Option<Instant>,
    authenticated_for: Duration,
    reidentify_allowed: bool,
    /// Why the current attempt failed before READY, for the error if the client gives up.
    failure: Option<String>,
    /// Where `initial_give_up_after` ends; attempts before the first READY must fail by then.
    first_login_ends: Option<Instant>,
    resume_in_place_pending: bool,
    resume_in_place_allowed: bool,
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
            authenticated_since: None,
            authenticated_for: Duration::ZERO,
            reidentify_allowed: true,
            failure: None,
            first_login_ends: None,
            resume_in_place_pending: false,
            resume_in_place_allowed: true,
        }
    }

    pub(crate) async fn run(mut self) {
        let mut failures = 0u32;
        let began = Instant::now();
        self.first_login_ends = self.opts.initial_give_up_after.map(|after| began + after);
        let error = loop {
            let base = self
                .session
                .as_ref()
                .and_then(|s| s.resume_url.clone())
                .filter(|u| resume_url_allowed(u, &self.opts.url))
                .unwrap_or_else(|| self.opts.url.clone());
            let url = connect_url(&base, self.opts.compress);
            self.authenticated_since = None;
            self.authenticated_for = Duration::ZERO;
            self.reidentify_allowed = true;
            self.resume_in_place_pending = false;
            self.resume_in_place_allowed = true;
            if self.session.is_none()
                && let Err(wait) = self.opts.identify_ledger.allows(self.identify_cap()).await
            {
                break self.identify_limit(wait);
            }
            let handshake_limit = self.attempt_limit(self.opts.handshake_timeout);
            let opened = tokio::select! {
                _ = self.shutdown.changed() => None,
                socket = open(&url, &self.connector, handshake_limit) => Some(socket),
            };
            self.failure = None;
            let end = match opened {
                None => End::Shutdown,
                Some(Ok(socket)) => self.session_loop(socket).await,
                Some(Err(e)) => {
                    warn!("gateway connect failed: {e}");
                    self.failure = Some(e);
                    End::Reconnect {
                        code: None,
                        resume: true,
                    }
                }
            };
            match end {
                End::Shutdown => break GatewayError::Closed,
                End::Fatal(e) => break e,
                End::Reconnect {
                    code,
                    resume: resumable,
                } => {
                    if !resumable {
                        self.session = None;
                        self.seq = None;
                    }
                    let resume = self.session.is_some();
                    if self.ready_tx.is_some() && self.failure.is_none() {
                        self.failure = Some(match code {
                            Some(code) => format!("Discord closed the connection with code {code}"),
                            None => {
                                "the connection ended before Discord confirmed the login".to_owned()
                            }
                        });
                    }
                    self.emit(GatewayEvent::Disconnected { code, resume });
                    self.deauthenticated();
                    let healthy = self.authenticated_for >= self.opts.healthy_session_min;
                    failures = if healthy { 0 } else { failures + 1 };
                    let before_ready = self.ready_tx.is_some();
                    let mut delay = self.backoff(failures.max(1));
                    if !resumable {
                        delay += self.identify_delay();
                    }
                    // Another attempt only if it can start inside the window.
                    let out_of_time = before_ready
                        && self
                            .first_login_ends
                            .is_some_and(|ends| Instant::now() + delay >= ends);
                    if failures >= self.opts.max_failed_attempts || out_of_time {
                        break match self.failure.take().filter(|_| before_ready) {
                            Some(cause) => GatewayError::Unreachable {
                                attempts: failures,
                                cause,
                            },
                            None => GatewayError::Exhausted(failures),
                        };
                    }
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

    /// `limit`, cut to what is left of the first-login window before the first READY.
    fn attempt_limit(&self, limit: Duration) -> Duration {
        match self.first_login_ends.filter(|_| self.ready_tx.is_some()) {
            Some(ends) => limit.min(ends.saturating_duration_since(Instant::now())),
            None => limit,
        }
    }

    fn authenticated(&mut self) {
        self.authenticated_since.get_or_insert_with(Instant::now);
    }

    fn deauthenticated(&mut self) {
        if let Some(since) = self.authenticated_since.take() {
            let stretch = since.elapsed();
            self.authenticated_for = self.authenticated_for.max(stretch);
            let healthy = stretch >= self.opts.healthy_session_min;
            self.reidentify_allowed |= healthy;
            self.resume_in_place_allowed |= healthy;
        }
    }

    fn identify_cap(&self) -> u32 {
        self.opts.max_identifies_per_day
    }

    fn identify_limit(&self, wait: Duration) -> GatewayError {
        GatewayError::IdentifyLimit {
            limit: self.identify_cap(),
            retry_after_secs: wait.as_secs(),
        }
    }

    fn backoff(&self, failures: u32) -> Duration {
        crate::session::jittered_backoff(
            self.opts.reconnect_delay_min,
            self.opts.reconnect_delay_max,
            failures,
        )
    }

    /// The wait before a fresh IDENTIFY, as Discord asks after an invalid session.
    fn identify_delay(&self) -> Duration {
        self.opts
            .invalid_session_delay
            .mul_f64(rand::rng().random_range(0.2..=1.0))
    }

    async fn session_loop(&mut self, socket: Socket) -> End {
        let (mut sink, mut stream) = socket.split();
        let mut inflate = self.opts.compress.then(ZlibStream::new);
        let hello_deadline = Instant::now() + self.attempt_limit(self.opts.hello_timeout);
        let mut auth_deadline = Some(AuthDeadline::new(
            Instant::now() + self.attempt_limit(self.opts.ready_timeout),
        ));
        let mut heartbeat: Option<Heartbeat> = None;
        let mut identify_at: Option<Instant> = None;
        let mut rate = RateLimit::new();
        let mut ready = false;
        loop {
            if ready {
                auth_deadline = None;
            } else if auth_deadline.is_none() && identify_at.is_none() {
                auth_deadline = Some(AuthDeadline::new(
                    Instant::now() + self.attempt_limit(self.opts.ready_timeout),
                ));
            }
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
                    None => Step::End(self.lost(None)),
                    Some(Err(e)) => {
                        debug!("gateway read failed: {e}");
                        Step::End(self.lost(None))
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let code = frame.map(|f| u16::from(f.code));
                        Step::End(self.lost(code))
                    }
                    Some(Ok(Message::Text(text))) => Step::Packet(text.into_bytes()),
                    Some(Ok(Message::Binary(bytes))) => match inflate.as_mut() {
                        Some(z) => match z.push(&bytes) {
                            Ok(Some(raw)) => Step::Packet(raw),
                            Ok(None) => Step::Idle,
                            Err(e) => {
                                warn!("gateway zlib-stream error: {e}");
                                close(&mut sink, CLOSE_RESUMABLE).await;
                                Step::End(self.lost(None))
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
                    self.failure = Some("Discord sent no HELLO in time".to_owned());
                    close(&mut sink, CLOSE_RESUMABLE).await;
                    Step::End(self.lost(None))
                }
                _ = sleep_until(auth_deadline.map_or_else(Instant::now, |d| d.at)), if auth_deadline.is_some() => {
                    warn!("gateway sent no READY or RESUMED in time");
                    // Both deadlines can be cut to the same first-login window end; without HELLO, that is the cause.
                    let missing = if heartbeat.is_none() { "HELLO" } else { "READY" };
                    self.failure = Some(format!("Discord sent no {missing} in time"));
                    close(&mut sink, CLOSE_RESUMABLE).await;
                    Step::End(self.lost(None))
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
                Step::Identify => match self.identify(&mut sink, &mut rate).await {
                    Ok(Some(end)) => return end,
                    Ok(None) => Ok(()),
                    Err(e) => Err(e),
                },
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
                        };
                    }
                    hb.acked = false;
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
                    let waiting = identify_at.is_some();
                    let outcome = self
                        .on_packet(
                            packet,
                            &mut sink,
                            &mut rate,
                            &mut heartbeat,
                            &mut identify_at,
                            &mut ready,
                        )
                        .await;
                    if !waiting && let Some(at) = identify_at {
                        auth_deadline = Some(match auth_deadline {
                            Some(d) if !d.excused => AuthDeadline {
                                at: d.at + at.saturating_duration_since(Instant::now()),
                                excused: true,
                            },
                            Some(d) => d,
                            None => AuthDeadline {
                                at: at + self.opts.ready_timeout,
                                excused: true,
                            },
                        });
                    }
                    match outcome {
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
                };
            }
        }
    }

    fn lost(&self, code: Option<u16>) -> End {
        match code {
            Some(code @ (4004 | 4010..=4014)) => End::Fatal(GatewayError::Rejected(code)),
            Some(4007 | 4009) => End::Reconnect {
                code,
                resume: false,
            },
            _ => End::Reconnect { code, resume: true },
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
            OP_HELLO if heartbeat.is_some() => debug!("gateway sent HELLO again; ignored"),
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
                });
                let end = if self.session.is_some() {
                    self.resume(sink, rate).await?
                } else {
                    self.identify(sink, rate).await?
                };
                if end.is_some() {
                    return Ok(end);
                }
            }
            OP_HEARTBEAT_ACK => {
                if let Some(hb) = heartbeat.as_mut() {
                    hb.acked = true;
                }
            }
            OP_HEARTBEAT => send(sink, rate, &self.heartbeat_payload()).await?,
            OP_RECONNECT => {
                info!("gateway asked to reconnect");
                close(sink, CLOSE_RESUMABLE).await;
                return Ok(Some(End::Reconnect {
                    code: None,
                    resume: true,
                }));
            }
            OP_INVALID_SESSION => {
                let resumable = packet.d.as_bool().unwrap_or(false);
                info!(resumable, "gateway invalidated the session");
                self.emit(GatewayEvent::InvalidSession { resumable });
                self.deauthenticated();
                *ready = false;
                if resumable && self.session.is_some() {
                    if !self.resume_in_place_allowed {
                        warn!(
                            "gateway invalidated a resumed session again without a healthy session between"
                        );
                        close(sink, CLOSE_RESUMABLE).await;
                        return Ok(Some(End::Reconnect {
                            code: None,
                            resume: true,
                        }));
                    }
                    self.resume_in_place_pending = true;
                    if let Some(end) = self.resume(sink, rate).await? {
                        return Ok(Some(end));
                    }
                } else if !std::mem::take(&mut self.reidentify_allowed) {
                    warn!(
                        "gateway invalidated the session again without a healthy session between"
                    );
                    close(sink, CLOSE_RESUMABLE).await;
                    return Ok(Some(End::Reconnect {
                        code: None,
                        resume: false,
                    }));
                } else {
                    self.session = None;
                    self.seq = None;
                    *identify_at = Some(Instant::now() + self.identify_delay());
                }
            }
            OP_DISPATCH => {
                if let Some(name) = packet.t {
                    self.on_dispatch(name, packet.d, ready);
                }
            }
            op => debug!(op, "gateway opcode ignored"),
        }
        Ok(None)
    }

    fn on_dispatch(&mut self, name: String, data: Value, ready: &mut bool) {
        match name.as_str() {
            "READY" => match ReadyData::deserialize(&data) {
                Ok(r) => {
                    *ready = true;
                    self.authenticated();
                    info!(user = %r.user.id, "gateway ready");
                    self.session = Some(Session {
                        id: r.session_id.clone(),
                        resume_url: r.resume_gateway_url,
                    });
                    self.shared.set_session_id(r.session_id.clone());
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
                if std::mem::take(&mut self.resume_in_place_pending) {
                    self.resume_in_place_allowed = false;
                }
                *ready = true;
                self.authenticated();
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
            data: Arc::new(data),
        };
        if let Some(tx) = &self.opts.dispatch_tx {
            let _ = tx.send(dispatch.clone());
        }
        self.emit(GatewayEvent::Dispatch(dispatch));
    }

    fn apply_cache(&self, name: &str, data: &Value) {
        self.shared
            .cache
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .apply(name, data);
    }

    async fn identify(
        &mut self,
        sink: &mut Sink,
        rate: &mut RateLimit,
    ) -> Result<Option<End>, String> {
        if let Err(wait) = self
            .opts
            .identify_ledger
            .record_within(self.identify_cap())
            .await
        {
            warn!("gateway IDENTIFY limit reached");
            close(sink, 1000).await;
            return Ok(Some(End::Fatal(self.identify_limit(wait))));
        }
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
        send(sink, rate, &payload).await.map(|()| None)
    }

    async fn resume(
        &mut self,
        sink: &mut Sink,
        rate: &mut RateLimit,
    ) -> Result<Option<End>, String> {
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
        send(sink, rate, &payload).await.map(|()| None)
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

enum Step {
    Idle,
    End(End),
    Packet(Vec<u8>),
    Send(Value),
    Identify,
    HeartbeatDue,
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

/// RESUME carries the token, so a `resume_gateway_url` from READY is followed only to a Discord host over wss.
fn resume_url_allowed(resume_url: &str, configured_url: &str) -> bool {
    if [resume_url, configured_url].iter().any(|url| {
        url.bytes()
            .any(|b| b.is_ascii_control() || b == b'\\' || b == b'#')
    }) {
        return false;
    }
    let Ok(uri) = resume_url.parse::<tokio_tungstenite::tungstenite::http::Uri>() else {
        return false;
    };
    let Some(authority) = uri.authority().filter(|a| !a.as_str().contains('@')) else {
        return false;
    };
    #[cfg(any(test, feature = "test-hooks"))]
    if uri.scheme_str() == Some("ws")
        && let Ok(configured) = configured_url.parse::<tokio_tungstenite::tungstenite::http::Uri>()
        && configured.scheme_str() == Some("ws")
        && configured.authority() == uri.authority()
        && (authority.host().eq_ignore_ascii_case("localhost")
            || authority
                .host()
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback()))
    {
        return true;
    }
    if uri.scheme_str() != Some("wss") {
        return false;
    }
    if authority.as_str().contains(':')
        && authority.as_str().rsplit_once(':').is_none_or(|(_, port)| {
            port.is_empty()
                || !port.bytes().all(|b| b.is_ascii_digit())
                || port.parse::<u16>().is_err()
        })
    {
        return false;
    }
    let host = authority.host().to_ascii_lowercase();
    host == "discord.gg" || host.ends_with(".discord.gg")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_urls_outside_discord_fall_back_to_the_configured_gateway() {
        let live = "wss://gateway.discord.gg";
        for url in [
            "wss://gateway.discord.gg",
            "wss://gateway-us-east1-b.discord.gg/",
            "wss://Gateway.Discord.GG:443/?v=9",
        ] {
            assert!(resume_url_allowed(url, live), "{url}");
        }
        for url in [
            "ws://gateway.discord.gg",
            "wss://attacker.example",
            "wss://discord.gg.attacker.example",
            "wss://gateway.discord.gg@attacker.example",
            "wss://notdiscord.gg",
            "wss://discord.gg:@attacker.example",
            "wss://attacker.example:443@discord.gg",
            "wss://discord.gg:bad",
            "wss://discord.gg:65536",
            "wss://discord.gg:",
            "wss://discord.gg\\attacker.example",
            "wss://discord.gg./",
            "wss://discord.gg/\n",
            "wss://discord.gg#fragment",
            "",
        ] {
            assert!(!resume_url_allowed(url, live), "{url}");
        }
        assert!(resume_url_allowed("ws://127.0.0.1:5/x", "ws://127.0.0.1:5"));
        assert!(!resume_url_allowed(
            "wss://attacker.example",
            "ws://127.0.0.1:5"
        ));
        assert!(!resume_url_allowed(
            "ws://127.0.0.1:6/x",
            "ws://127.0.0.1:5"
        ));
        assert!(!resume_url_allowed(
            "ws://attacker.example",
            "ws://attacker.example"
        ));
    }
}
