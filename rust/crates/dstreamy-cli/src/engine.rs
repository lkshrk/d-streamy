//! The command engine behind `dstreamy daemon` and `dstreamy serve`: one Discord connection, at most one stream.

use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use dstreamy_core::{Codec, CropRect, Fps, IdleSetting, Resolution, ServeConfig, StreamConfig};
use dstreamy_discord::{
    DiscordClient, GatewayError, Session, SessionError, SessionEvent, SessionState, StreamStats,
};
use dstreamy_media::{
    CaptureConfig, CaptureSource, EXTERNAL_TARGET_ID, ExternalHandle, ExternalOptions,
    ExternalSource, IdleMode, InterruptReason, OptionsError, SourceState,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::capture::{self, PortalBook, PortalToken, TargetRow};
use crate::discord::{self, Persistence};
use crate::lock::{LockError, SessionLock};
use crate::stream::{self, parse_codec};
use crate::{CliError, Ctx};

pub const PROTOCOL: u32 = 1;
/// Discord closes the gateway with 4004 when the token is not valid.
const AUTH_FAILED: u16 = 4004;
const FORWARDER_DRAIN: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub include_platform: bool,
    /// Capacity of the `events()` broadcast; lossless subscribers are unbounded.
    pub event_capacity: usize,
    /// Accepts `start` with target `external`; protocol 1 never does.
    pub allow_external: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            include_platform: true,
            event_capacity: 256,
            allow_external: false,
        }
    }
}

// Session events dominate, as in `SessionEvent`; boxing them would add an allocation per event.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum EngineEvent {
    Session(SessionEvent),
    Source(SourceState),
}

/// The `[serve]` settings as options for an external source; `stall_ms` below the first-frames wait is refused.
pub fn external_options(serve: &ServeConfig) -> Result<ExternalOptions, OptionsError> {
    let options = ExternalOptions {
        grace: Duration::from_secs(serve.grace_secs),
        stall: Duration::from_millis(serve.stall_ms),
        idle: match serve.idle {
            IdleSetting::Documented => IdleMode::Documented,
            IdleSetting::None => IdleMode::None,
            IdleSetting::Continuous => IdleMode::Continuous,
        },
    };
    options.validate()?;
    Ok(options)
}

/// An external source's state on the wire: `state`, plus `attachment` or `reason` and `for_ms`.
#[derive(Debug, Clone, Serialize)]
pub struct SourceView {
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub for_ms: Option<u64>,
}

impl From<&SourceState> for SourceView {
    fn from(state: &SourceState) -> Self {
        let view = |state| Self {
            state,
            attachment: None,
            reason: None,
            for_ms: None,
        };
        match state {
            SourceState::Detached => view("detached"),
            SourceState::Producing { attachment } => Self {
                attachment: Some(*attachment),
                ..view("producing")
            },
            SourceState::Interrupted { since, reason } => Self {
                reason: Some(match reason {
                    InterruptReason::Detached => "detached",
                    InterruptReason::Stalled => "stalled",
                }),
                for_ms: Some(u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)),
                ..view("interrupted")
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub session: Option<SessionState>,
    pub source: Option<SourceView>,
    pub stats: Option<StreamStats>,
    pub started_by: Option<u64>,
}

#[derive(Debug)]
pub struct Failure {
    pub code: &'static str,
    pub message: String,
}

impl Failure {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<CliError> for Failure {
    fn from(e: CliError) -> Self {
        let code = match &e {
            CliError::Config(_) | CliError::ServeConfig(_) => "config",
            CliError::InvalidStreamConfig(_) | CliError::EmptyToken => "invalid_params",
            CliError::NotLoggedIn => {
                return Self::new("not_logged_in", "not logged in; send set_token first");
            }
            CliError::Gateway(GatewayError::Rejected(AUTH_FAILED)) => {
                return Self::new("invalid_token", "Discord rejected the token");
            }
            CliError::Gateway(_) => "gateway",
            CliError::UnknownGuild(guild) => {
                return Self::new("unknown_guild", format!("unknown guild {guild}"));
            }
            CliError::GuildUnavailable(_) => "guild_unavailable",
            CliError::UnknownChannel { guild, channel } => {
                return Self::new(
                    "unknown_channel",
                    format!("no voice channel {channel} in guild {guild}"),
                );
            }
            CliError::NoPlatformCapture => {
                return Self::new(
                    "no_platform_capture",
                    "screen capture is not supported on this platform",
                );
            }
            CliError::Media(_) => "capture",
            CliError::AlreadyStreaming | CliError::Lock(LockError::Held { .. }) => {
                "already_running"
            }
            CliError::ReadToken(_)
            | CliError::Session(_)
            | CliError::Stopped(_)
            | CliError::NoTerminal
            | CliError::Terminal(_)
            | CliError::Ipc(_)
            | CliError::Lock(LockError::Io { .. }) => "internal",
        };
        Self::new(code, e.to_string())
    }
}

pub fn invalid_params(e: impl std::fmt::Display) -> Failure {
    Failure::new("invalid_params", e.to_string())
}

fn not_running() -> Failure {
    Failure::new("not_running", "no stream is running")
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Hello,
    SetToken { token: String },
    LoggedIn,
    Guilds,
    Channels { guild_id: String },
    Targets,
    Start(StartParams),
    Update(UpdateParams),
    Stop,
}

#[derive(Deserialize)]
pub struct StartParams {
    pub guild_id: String,
    pub channel_id: String,
    pub target: String,
    pub crop: Option<CropRect>,
    pub codec: Option<String>,
    pub fps: Option<Fps>,
    pub resolution: Option<Resolution>,
    pub bitrate_kbps: Option<u32>,
    pub gain: Option<f32>,
}

#[derive(Deserialize)]
pub struct UpdateParams {
    pub crop: Option<CropRect>,
    pub fps: Option<Fps>,
    pub resolution: Option<Resolution>,
    pub bitrate_kbps: Option<u32>,
}

struct Conn {
    client: Arc<DiscordClient>,
    session: Session,
    forwarder: JoinHandle<()>,
}

impl Conn {
    fn live(&self) -> bool {
        matches!(
            *self.session.state().borrow(),
            SessionState::Connecting | SessionState::Streaming | SessionState::Reconnecting { .. }
        )
    }
}

struct Running {
    target: String,
    stream: StreamConfig,
    crop: Option<CropRect>,
    external: bool,
}

impl Running {
    fn updated(&self, p: &UpdateParams) -> Result<(Self, CaptureConfig), Failure> {
        let stream = StreamConfig {
            resolution: p.resolution.unwrap_or(self.stream.resolution),
            fps: p.fps.unwrap_or(self.stream.fps),
            bitrate_kbps: p.bitrate_kbps.unwrap_or(self.stream.bitrate_kbps),
            ..self.stream
        };
        stream.validate().map_err(invalid_params)?;
        let next = Self {
            target: self.target.clone(),
            stream,
            crop: p.crop.or(self.crop),
            external: self.external,
        };
        let capture = stream::capture_config(&next.target, &next.stream, next.crop);
        Ok((next, capture))
    }
}

pub struct Engine {
    me: Weak<Engine>,
    ctx: Ctx,
    opts: Options,
    events: broadcast::Sender<EngineEvent>,
    lossless: Mutex<Vec<mpsc::UnboundedSender<EngineEvent>>>,
    conn: Mutex<Option<Arc<Conn>>>,
    /// Held while the connection is created, replaced or torn down, and while a stream starts.
    lifecycle: tokio::sync::Mutex<()>,
    updating: tokio::sync::Mutex<()>,
    last_stats: Mutex<Option<StreamStats>>,
    running: Mutex<Option<Running>>,
    started_by: Mutex<Option<u64>>,
    external: Mutex<Option<External>>,
    portal: Mutex<Option<PortalBook>>,
    session_lock: Mutex<Hold>,
    #[cfg(test)]
    start_gate: tokio::sync::Mutex<()>,
    #[cfg(test)]
    stop_gate: tokio::sync::Mutex<()>,
    #[cfg(test)]
    stop_entered: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    shutdown_entered: std::sync::atomic::AtomicBool,
}

/// While `starting`, a `Stopped` state belongs to the previous run and must not release the lock.
#[derive(Default)]
struct Hold {
    lock: Option<SessionLock>,
    starting: bool,
}

impl Hold {
    fn release_if(&mut self, stopped: bool) {
        if stopped && !self.starting {
            self.lock = None;
        }
    }
}

struct External {
    handle: ExternalHandle,
    watcher: JoinHandle<()>,
}

impl Drop for External {
    fn drop(&mut self) {
        self.watcher.abort();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Engine {
    pub fn new(ctx: &Ctx, opts: Options) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            ctx: ctx.clone(),
            opts,
            events: broadcast::channel(opts.event_capacity).0,
            lossless: Mutex::new(Vec::new()),
            conn: Mutex::new(None),
            lifecycle: tokio::sync::Mutex::new(()),
            updating: tokio::sync::Mutex::new(()),
            last_stats: Mutex::new(None),
            running: Mutex::new(None),
            started_by: Mutex::new(None),
            external: Mutex::new(None),
            portal: Mutex::new(None),
            session_lock: Mutex::new(Hold::default()),
            #[cfg(test)]
            start_gate: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            stop_gate: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            stop_entered: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            shutdown_entered: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// May lag and skip events once `event_capacity` is exceeded.
    pub fn events(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }

    /// Every event in order; closes once the engine and its event forwarder are gone.
    pub fn subscribe_lossless(&self) -> mpsc::UnboundedReceiver<EngineEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        lock(&self.lossless).push(tx);
        rx
    }

    pub async fn handle(&self, command: Command) -> Result<Value, Failure> {
        self.handle_as(command, None).await
    }

    /// As [`Engine::handle`]; a successful `start` records `caller` as `started_by`.
    pub async fn handle_as(&self, command: Command, caller: Option<u64>) -> Result<Value, Failure> {
        match command {
            Command::Hello => Ok(json!({
                "protocol": PROTOCOL,
                "version": env!("CARGO_PKG_VERSION"),
                "platform": std::env::consts::OS,
            })),
            Command::SetToken { token } => self.set_token(token).await,
            Command::LoggedIn => {
                let logged_in = self
                    .ctx
                    .paths
                    .load_token()
                    .map_err(CliError::from)?
                    .is_some();
                Ok(json!({ "logged_in": logged_in }))
            }
            Command::Guilds => {
                let conn = self.conn().await?;
                let guilds = discord::guilds(&conn.client).await;
                Ok(guilds
                    .into_iter()
                    .map(|g| json!({ "id": g.id, "name": g.name }))
                    .collect())
            }
            Command::Channels { guild_id } => {
                let conn = self.conn().await?;
                let channels = discord::voice_channels(&conn.client, &guild_id).await?;
                Ok(channels
                    .into_iter()
                    .map(|c| json!({ "id": c.id, "name": c.name }))
                    .collect())
            }
            Command::Targets => {
                let (targets, failure) = capture::targets(self.opts.include_platform).await;
                if let Some(e) = failure {
                    return Err(CliError::from(e).into());
                }
                let rows: Vec<TargetRow> = targets.into_iter().map(Into::into).collect();
                serde_json::to_value(rows).map_err(|e| Failure::new("internal", e.to_string()))
            }
            Command::Start(params) => self.start(params, caller).await,
            Command::Update(params) => self.update(params).await,
            Command::Stop => self.stop().await,
        }
    }

    pub fn status(&self) -> Status {
        let session = self.current().map(|c| c.session.state().borrow().clone());
        let source = lock(&self.external)
            .as_ref()
            .map(|e| SourceView::from(&*e.handle.state().borrow()));
        Status {
            started_by: session.as_ref().and(*lock(&self.started_by)),
            session,
            source,
            stats: lock(&self.last_stats).clone(),
        }
    }

    /// Announces a producer's video (op 12) for the live session.
    pub async fn apply_producer_video(
        &self,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<(), Failure> {
        let conn = self
            .current()
            .filter(|c| c.live())
            .ok_or_else(not_running)?;
        conn.session
            .update_video(width, height, fps)
            .await
            .map_err(session_failure)
    }

    /// Sends `event` to every subscriber as if the session had emitted it.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn inject_for_tests(&self, event: SessionEvent) {
        self.publish(EngineEvent::Session(event));
    }

    /// A share of the Discord connection, as a command in flight holds one; dropping it takes `release`.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn hold_connection_for_tests(&self, release: Duration) -> Option<impl Send + use<>> {
        struct Held(#[allow(dead_code)] Arc<Conn>, Duration);
        impl Drop for Held {
            fn drop(&mut self) {
                std::thread::sleep(self.1);
            }
        }
        self.current().map(|conn| Held(conn, release))
    }

    fn publish(&self, event: EngineEvent) {
        lock(&self.lossless).retain(|tx| tx.send(event.clone()).is_ok());
        let _ = self.events.send(event);
    }

    fn current(&self) -> Option<Arc<Conn>> {
        lock(&self.conn).clone()
    }

    async fn conn(&self) -> Result<Arc<Conn>, CliError> {
        if let Some(conn) = self.current() {
            return Ok(conn);
        }
        let _lifecycle = self.lifecycle.lock().await;
        self.ensure_conn().await
    }

    /// Needs `lifecycle` held.
    async fn ensure_conn(&self) -> Result<Arc<Conn>, CliError> {
        if let Some(conn) = self.current() {
            return Ok(conn);
        }
        let engine = self
            .me
            .upgrade()
            .expect("an engine is only reachable through its Arc");
        let client = Arc::new(discord::connect(&self.ctx, Persistence::Long).await?);
        let session = Session::new(client.clone());
        let forwarder = tokio::spawn(engine.forward(session.subscribe()));
        let conn = Arc::new(Conn {
            client,
            session,
            forwarder,
        });
        *lock(&self.conn) = Some(conn.clone());
        Ok(conn)
    }

    async fn forward(self: Arc<Self>, mut events: broadcast::Receiver<SessionEvent>) {
        loop {
            match events.recv().await {
                Ok(event) => {
                    match &event {
                        SessionEvent::Health(h) => *lock(&self.last_stats) = Some(h.stats.clone()),
                        SessionEvent::State { .. } => {
                            self.remember_portal_token();
                            self.release_if_stopped();
                        }
                        _ => {}
                    }
                    self.publish(EngineEvent::Session(event));
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "session events lagged");
                    self.release_if_stopped();
                }
                Err(RecvError::Closed) => break,
            }
        }
    }

    fn release_if_stopped(&self) {
        let mut hold = lock(&self.session_lock);
        let stopped = self
            .current()
            .is_none_or(|c| stream::has_stopped(&c.session));
        hold.release_if(stopped);
        if stopped && !hold.starting {
            *lock(&self.started_by) = None;
            *lock(&self.external) = None;
        }
    }

    fn remember_portal_token(&self) {
        if let Some(book) = lock(&self.portal).as_mut()
            && let Err(e) = book.save(&self.ctx.paths)
        {
            tracing::warn!("saving the portal restore token: {e}");
        }
    }

    async fn set_token(&self, token: String) -> Result<Value, Failure> {
        let token = token.trim().to_owned();
        if token.is_empty() {
            return Err(CliError::EmptyToken.into());
        }
        let _lifecycle = self.lifecycle.lock().await;
        if self.current().is_some_and(|c| c.live()) {
            return Err(Failure::new(
                "streaming",
                "stop the stream before changing the token",
            ));
        }
        let client = discord::connect_with(&self.ctx, token.clone(), Persistence::Brief).await?;
        client.close().await;
        self.ctx.paths.save_token(&token).map_err(CliError::from)?;
        self.disconnect_locked().await;
        Ok(Value::Null)
    }

    async fn start(&self, p: StartParams, caller: Option<u64>) -> Result<Value, Failure> {
        let config =
            dstreamy_core::Config::load(&self.ctx.paths.config_file).map_err(CliError::from)?;
        let external = self.opts.allow_external && p.target == EXTERNAL_TARGET_ID;
        let codec = match &p.codec {
            Some(codec) => parse_codec(codec)
                .map_err(|e| invalid_params(format!("invalid codec {codec:?}: {e}")))?,
            None if external => Codec::H264,
            None => config.stream.codec,
        };
        if external && codec != Codec::H264 {
            return Err(invalid_params("an external session always streams h264"));
        }
        let external_options = external
            .then(|| external_options(&config.serve))
            .transpose()
            .map_err(CliError::from)?;
        let settings = StreamConfig {
            resolution: p.resolution.unwrap_or(config.stream.resolution),
            fps: p.fps.unwrap_or(config.stream.fps),
            bitrate_kbps: p.bitrate_kbps.unwrap_or(config.stream.bitrate_kbps),
            codec,
            audio_gain: p.gain.unwrap_or(config.stream.audio_gain),
        };
        settings.validate().map_err(invalid_params)?;
        if let Some(crop) = p.crop {
            crop.validate().map_err(invalid_params)?;
        }
        let _lifecycle = self.lifecycle.lock().await;
        let conn = self.ensure_conn().await?;
        if conn.live() {
            return Err(CliError::AlreadyStreaming.into());
        }
        let channels = discord::voice_channels(&conn.client, &p.guild_id).await?;
        if !channels.iter().any(|c| c.id == p.channel_id) {
            return Err(CliError::UnknownChannel {
                guild: p.guild_id,
                channel: p.channel_id,
            }
            .into());
        }
        let restore_token = capture::restore_token(&config);
        let (source, portal_token, handle): (Box<dyn CaptureSource>, _, _) = match external_options
        {
            Some(options) => {
                let source = ExternalSource::new(options).map_err(CliError::from)?;
                let handle = source.handle();
                (Box::new(source), PortalToken::default(), Some(handle))
            }
            None => {
                let picked = capture::source_for(&p.target, restore_token.clone())?;
                (picked.source, picked.portal_token, None)
            }
        };
        let capture = stream::capture_config(&p.target, &settings, p.crop);
        {
            let mut hold = lock(&self.session_lock);
            if hold.lock.is_none() {
                hold.lock = Some(SessionLock::acquire(&self.ctx.paths).map_err(CliError::from)?);
            }
            hold.starting = true;
        }
        #[cfg(test)]
        drop(self.start_gate.lock().await);
        let started = stream::start(&conn.session, p.guild_id, p.channel_id, source, capture).await;
        {
            let mut hold = lock(&self.session_lock);
            hold.starting = false;
            hold.release_if(started.is_err() || stream::has_stopped(&conn.session));
        }
        started?;
        *lock(&self.last_stats) = None;
        *lock(&self.portal) = Some(PortalBook::new(restore_token, portal_token));
        *lock(&self.started_by) = caller;
        *lock(&self.external) = handle.map(|handle| self.watch_source(handle));
        self.release_if_stopped();
        *lock(&self.running) = Some(Running {
            target: p.target,
            stream: settings,
            crop: p.crop,
            external,
        });
        Ok(Value::Null)
    }

    fn watch_source(&self, handle: ExternalHandle) -> External {
        let engine = self.me.clone();
        let mut states = handle.state();
        let watcher = tokio::spawn(async move {
            loop {
                let state = *states.borrow_and_update();
                let Some(engine) = engine.upgrade() else {
                    return;
                };
                engine.publish(EngineEvent::Source(state));
                drop(engine);
                if states.changed().await.is_err() {
                    return;
                }
            }
        });
        External { handle, watcher }
    }

    async fn update(&self, p: UpdateParams) -> Result<Value, Failure> {
        if let Some(crop) = p.crop {
            crop.validate().map_err(invalid_params)?;
        }
        let conn = self
            .current()
            .filter(|c| c.live())
            .ok_or_else(not_running)?;
        let _updating = self.updating.lock().await;
        let (next, capture) = {
            let running = lock(&self.running);
            let running = running.as_ref().ok_or_else(not_running)?;
            if running.external {
                return Err(invalid_params(
                    "the producer owns the encoder settings of an external session",
                ));
            }
            running.updated(&p)?
        };
        conn.session
            .reconfigure(capture)
            .await
            .map_err(session_failure)?;
        *lock(&self.running) = Some(next);
        Ok(Value::Null)
    }

    async fn stop(&self) -> Result<Value, Failure> {
        #[cfg(test)]
        self.stop_entered
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _lifecycle = self.lifecycle.lock().await;
        let conn = self
            .current()
            .filter(|c| c.live())
            .ok_or_else(not_running)?;
        #[cfg(test)]
        drop(self.stop_gate.lock().await);
        let stopped = conn.session.stop().await.map_err(session_failure)?;
        self.release_if_stopped();
        if stopped.is_none() {
            return Err(not_running());
        }
        *lock(&self.running) = None;
        *lock(&self.started_by) = None;
        *lock(&self.external) = None;
        self.release_if_stopped();
        let stats = lock(&self.last_stats).take().unwrap_or_default();
        serde_json::to_value(stats).map_err(|e| Failure::new("internal", e.to_string()))
    }

    /// Stops the stream and disconnects; the session's last events are sent before it returns.
    pub async fn shutdown(&self) {
        #[cfg(test)]
        self.shutdown_entered
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _lifecycle = self.lifecycle.lock().await;
        self.disconnect_locked().await;
    }

    /// Needs `lifecycle` held; the session's last events are sent before it returns.
    async fn disconnect_locked(&self) {
        let Some(conn) = lock(&self.conn).take() else {
            return;
        };
        match Arc::try_unwrap(conn) {
            Ok(Conn {
                client,
                session,
                forwarder,
            }) => {
                stream::teardown(session, client).await;
                if timeout(FORWARDER_DRAIN, forwarder).await.is_err() {
                    tracing::debug!("session events still open; not waiting for the rest");
                }
            }
            Err(shared) => {
                tracing::warn!(
                    "the Discord connection is still in use; stopping without closing it"
                );
                let _ = shared.session.stop().await;
            }
        }
        *lock(&self.running) = None;
        *lock(&self.started_by) = None;
        *lock(&self.external) = None;
        *lock(&self.session_lock) = Hold::default();
        self.remember_portal_token();
    }
}

fn session_failure(e: SessionError) -> Failure {
    match e {
        SessionError::NotRunning => not_running(),
        SessionError::CodecChange => invalid_params(e),
        SessionError::Stream(_) => Failure::new("capture", e.to_string()),
        _ => Failure::new("internal", e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_update_carries_the_crop_and_keeps_the_rest() {
        let running = Running {
            target: "display:1".into(),
            stream: StreamConfig::default(),
            crop: None,
            external: false,
        };
        let crop = CropRect::new(10, 20, 640, 360);
        let update = |line: &str| match serde_json::from_str(line) {
            Ok(Command::Update(p)) => p,
            _ => panic!("{line}"),
        };

        let (next, capture) = running
            .updated(&update(
                r#"{"id":"u","cmd":"update","crop":{"x":10,"y":20,"width":640,"height":360}}"#,
            ))
            .unwrap();
        assert_eq!(capture.crop, Some(crop));
        assert_eq!(capture.target_id.as_deref(), Some("display:1"));
        assert_eq!(capture.fps, running.stream.fps.get());
        assert_eq!(next.crop, Some(crop));

        let (next, capture) = next
            .updated(&update(r#"{"id":"u","cmd":"update","fps":30}"#))
            .unwrap();
        assert_eq!(capture.crop, Some(crop));
        assert_eq!(capture.fps, 30);
        assert_eq!(next.stream.fps.get(), 30);
    }

    #[test]
    fn a_stopped_state_keeps_the_lock_while_a_start_is_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let paths = dstreamy_core::ConfigPaths::in_dir(dir.path());
        let mut hold = Hold {
            lock: Some(SessionLock::acquire(&paths).unwrap()),
            starting: true,
        };
        hold.release_if(true);
        assert!(hold.lock.is_some());
        hold.starting = false;
        hold.release_if(false);
        assert!(hold.lock.is_some());
        hold.release_if(true);
        assert!(hold.lock.is_none());
    }

    mod session_lock {
        use std::time::Duration;

        use dstreamy_core::ConfigPaths;
        use dstreamy_mock_discord::{
            MOCK_GUILD_ID, MOCK_TOKEN, MOCK_VOICE_CHANNEL_ID, MockDiscord,
        };
        use tempfile::TempDir;
        use tokio::time::timeout;

        use super::*;

        const T: Duration = Duration::from_secs(20);

        struct Fixture {
            mock: MockDiscord,
            dir: TempDir,
            engine: Arc<Engine>,
        }

        impl Fixture {
            async fn new() -> Self {
                let mock = MockDiscord::start().await.unwrap();
                let dir = tempfile::tempdir().unwrap();
                let paths = ConfigPaths::in_dir(dir.path());
                paths.save_token(MOCK_TOKEN).unwrap();
                let ctx = Ctx {
                    paths,
                    gateway_url: mock.gateway_url(),
                };
                let opts = Options {
                    include_platform: false,
                    ..Options::default()
                };
                let engine = Engine::new(&ctx, opts);
                Self { mock, dir, engine }
            }

            fn lock_is_free(&self) -> bool {
                SessionLock::acquire(&ConfigPaths::in_dir(self.dir.path())).is_ok()
            }

            /// Spawns a start and returns once it is parked on `start_gate` with `starting` set.
            async fn start_in_flight(
                &self,
            ) -> (
                JoinHandle<Result<Value, Failure>>,
                tokio::sync::MutexGuard<'_, ()>,
            ) {
                let gate = self.engine.start_gate.lock().await;
                let engine = self.engine.clone();
                let task = tokio::spawn(async move { engine.start(params(), None).await });
                timeout(T, async {
                    while !lock(&self.engine.session_lock).starting {
                        assert!(!task.is_finished(), "the start ended early");
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                })
                .await
                .expect("the start never reached the session");
                (task, gate)
            }

            async fn wait_state(&self, f: impl Fn(&SessionState) -> bool) {
                let mut state = self.engine.current().unwrap().session.state();
                timeout(T, state.wait_for(|s| f(s))).await.unwrap().unwrap();
            }
        }

        fn params() -> StartParams {
            StartParams {
                guild_id: MOCK_GUILD_ID.into(),
                channel_id: MOCK_VOICE_CHANNEL_ID.into(),
                target: "test-pattern".into(),
                crop: None,
                codec: None,
                fps: Some(Fps::Fps15),
                resolution: Some(Resolution::new(320, 180).unwrap()),
                bitrate_kbps: Some(1000),
                gain: None,
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_stop_sent_during_a_start_waits_for_it_and_releases_before_replying() {
            let f = Fixture::new().await;
            let (start, gate) = f.start_in_flight().await;
            let engine = f.engine.clone();
            let stop = tokio::spawn(async move { engine.stop().await });
            timeout(T, async {
                while !f
                    .engine
                    .stop_entered
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            assert!(!stop.is_finished(), "stop ran past the in-flight start");
            drop(gate);
            let stopped = timeout(T, stop).await.unwrap().unwrap();
            assert!(stopped.is_ok(), "{stopped:?}");
            assert!(f.lock_is_free(), "the lock outlived the stop reply");
            assert!(start.await.unwrap().is_ok());
            f.engine.shutdown().await;
        }

        #[tokio::test(flavor = "current_thread")]
        async fn a_stop_that_finds_the_session_ended_still_releases_the_lock() {
            let f = Fixture::new().await;
            f.engine.start(params(), None).await.unwrap();
            f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
            let conn = f.engine.current().unwrap();
            conn.forwarder.abort();
            let gate = f.engine.stop_gate.lock().await;
            let engine = f.engine.clone();
            let stop = tokio::spawn(async move { engine.stop().await });
            timeout(T, async {
                while f.engine.lifecycle.try_lock().is_ok() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("stop never took lifecycle");
            assert!(
                f.mock
                    .voice_state_from_other_session(MOCK_GUILD_ID, Some(MOCK_VOICE_CHANNEL_ID))
            );
            f.wait_state(|s| matches!(s, SessionState::Stopped { .. }))
                .await;
            let _ = conn.session.stop().await;
            drop(conn);
            drop(gate);
            let stopped = timeout(T, stop).await.unwrap().unwrap();
            assert_eq!(stopped.unwrap_err().code, "not_running");
            assert!(f.lock_is_free(), "not_running kept the lock");
            f.engine.shutdown().await;
        }

        #[tokio::test(flavor = "current_thread")]
        async fn a_lagged_forwarder_releases_the_lock_of_a_stopped_session() {
            let f = Fixture::new().await;
            f.engine.start(params(), None).await.unwrap();
            f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
            f.engine.current().unwrap().forwarder.abort();
            assert!(
                f.mock
                    .voice_state_from_other_session(MOCK_GUILD_ID, Some(MOCK_VOICE_CHANNEL_ID))
            );
            f.wait_state(|s| matches!(s, SessionState::Stopped { .. }))
                .await;
            assert!(!f.lock_is_free());

            let (tx, rx) = broadcast::channel(1);
            for _ in 0..3 {
                tx.send(SessionEvent::Resumed).unwrap();
            }
            drop(tx);
            timeout(T, f.engine.clone().forward(rx)).await.unwrap();
            assert!(f.lock_is_free(), "a lagged forwarder kept the lock");
            f.engine.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn an_aborted_start_keeps_the_lock_until_teardown() {
            let f = Fixture::new().await;
            let (start, _gate) = f.start_in_flight().await;
            start.abort();
            assert!(start.await.unwrap_err().is_cancelled());
            assert!(
                !f.lock_is_free(),
                "the lock was dropped with the aborted start"
            );
            f.engine.shutdown().await;
            assert!(f.lock_is_free());
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn the_previous_runs_stop_does_not_release_a_starting_runs_lock() {
            let f = Fixture::new().await;
            f.engine.start(params(), None).await.unwrap();
            f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
            assert!(
                f.mock
                    .voice_state_from_other_session(MOCK_GUILD_ID, Some(MOCK_VOICE_CHANNEL_ID))
            );
            f.wait_state(|s| matches!(s, SessionState::Stopped { .. }))
                .await;
            f.engine.release_if_stopped();
            assert!(f.lock_is_free());

            let (start, gate) = f.start_in_flight().await;
            f.engine.release_if_stopped();
            assert!(
                !f.lock_is_free(),
                "a stale Stopped released the new run's lock"
            );
            drop(gate);
            assert!(start.await.unwrap().is_ok());
            assert!(!f.lock_is_free());
            f.engine.shutdown().await;
        }

        async fn guilds(engine: &Arc<Engine>) -> Value {
            let engine = engine.clone();
            let guilds = tokio::spawn(async move { engine.handle(Command::Guilds).await });
            timeout(T, guilds)
                .await
                .expect("guilds was blocked")
                .unwrap()
                .unwrap()
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn guilds_and_update_do_not_wait_for_a_stop_or_each_other() {
            let f = Fixture::new().await;
            f.engine.start(params(), None).await.unwrap();
            f.wait_state(|s| matches!(s, SessionState::Streaming)).await;

            let updating = f.engine.updating.lock().await;
            assert_eq!(guilds(&f.engine).await.as_array().unwrap().len(), 1);
            drop(updating);

            let gate = f.engine.stop_gate.lock().await;
            let engine = f.engine.clone();
            let stop = tokio::spawn(async move { engine.handle(Command::Stop).await });
            timeout(T, async {
                while f.engine.lifecycle.try_lock().is_ok() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("stop never took lifecycle");
            assert_eq!(guilds(&f.engine).await.as_array().unwrap().len(), 1);
            let update = UpdateParams {
                crop: None,
                fps: Some(Fps::Fps30),
                resolution: None,
                bitrate_kbps: None,
            };
            let updated = timeout(T, f.engine.handle(Command::Update(update)))
                .await
                .expect("update waited for the stop");
            assert!(updated.is_ok(), "{updated:?}");
            assert!(!stop.is_finished());
            drop(gate);
            let stopped = timeout(T, stop).await.unwrap().unwrap();
            assert!(stopped.is_ok(), "{stopped:?}");
            f.engine.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_shutdown_during_a_start_waits_for_it_and_tears_it_down() {
            let f = Fixture::new().await;
            let (start, gate) = f.start_in_flight().await;
            let engine = f.engine.clone();
            let shutdown = tokio::spawn(async move { engine.shutdown().await });
            timeout(T, async {
                while !f
                    .engine
                    .shutdown_entered
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            assert!(!shutdown.is_finished(), "shutdown ran past the start");
            drop(gate);
            assert!(timeout(T, start).await.unwrap().unwrap().is_ok());
            timeout(T, shutdown).await.unwrap().unwrap();
            assert!(f.engine.current().is_none());
            assert!(f.lock_is_free());
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn producer_video_is_announced_with_op_12() {
            use dstreamy_mock_discord::VoiceKind;
            use dstreamy_mock_discord::opcodes::voice::OP_VIDEO;

            let f = Fixture::new().await;
            let idle = f.engine.apply_producer_video(160, 90, 15).await;
            assert_eq!(idle.unwrap_err().code, "not_running");
            f.engine.start(params(), None).await.unwrap();
            f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
            f.engine.apply_producer_video(160, 90, 15).await.unwrap();
            let announced = f
                .mock
                .wait_for(T, |r| {
                    r.ops
                        .iter()
                        .filter(|o| o.is(Some(VoiceKind::Stream), OP_VIDEO))
                        .filter_map(|o| o.json())
                        .any(|d| {
                            d["streams"][0]["max_resolution"]["width"] == 160
                                && d["streams"][0]["max_resolution"]["height"] == 90
                                && d["streams"][0]["max_framerate"] == 15
                        })
                        .then_some(())
                })
                .await;
            assert!(announced.is_some(), "no op 12 with the producer's video");
            f.engine.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn every_receiver_gets_every_event() {
            let f = Fixture::new().await;
            let receivers = [f.engine.events(), f.engine.events()];
            f.engine.start(params(), None).await.unwrap();
            f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
            f.engine.handle(Command::Stop).await.unwrap();
            f.engine.shutdown().await;
            for mut events in receivers {
                let mut states = Vec::new();
                while let Ok(EngineEvent::Session(event)) = events.try_recv() {
                    if let SessionEvent::State { state } = event {
                        states.push(state);
                    }
                }
                assert!(
                    matches!(
                        states.as_slice(),
                        [
                            SessionState::Connecting,
                            SessionState::Streaming,
                            ..,
                            SessionState::Stopped { .. }
                        ]
                    ),
                    "{states:?}"
                );
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_lossless_subscriber_gets_every_event_the_broadcast_drops() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx {
            paths: dstreamy_core::ConfigPaths::in_dir(dir.path()),
            gateway_url: String::new(),
        };
        let opts = Options {
            event_capacity: 4,
            ..Options::default()
        };
        let engine = Engine::new(&ctx, opts);
        let mut lossless = engine.subscribe_lossless();
        let mut lossy = engine.events();
        let (tx, rx) = broadcast::channel(128);
        for _ in 0..100 {
            tx.send(SessionEvent::Resumed).unwrap();
        }
        drop(tx);
        engine.clone().forward(rx).await;
        let mut n = 0;
        while let Ok(EngineEvent::Session(SessionEvent::Resumed)) = lossless.try_recv() {
            n += 1;
        }
        assert_eq!(n, 100);
        assert!(matches!(
            lossy.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(96))
        ));
        drop(engine);
        assert!(lossless.recv().await.is_none(), "closes with the engine");
    }

    #[test]
    fn serve_options_come_from_the_config_and_a_short_stall_is_refused() {
        let options = external_options(&ServeConfig::default()).unwrap();
        assert_eq!(options, ExternalOptions::default());
        let serve = ServeConfig {
            grace_secs: 5,
            idle: IdleSetting::Continuous,
            stall_ms: 500,
        };
        let options = external_options(&serve).unwrap();
        assert_eq!(options.grace, Duration::from_secs(5));
        assert_eq!(options.stall, dstreamy_media::FIRST_FRAMES_WAIT);
        assert_eq!(options.idle, IdleMode::Continuous);
        let short = ServeConfig {
            stall_ms: 499,
            ..serve
        };
        assert_eq!(
            external_options(&short),
            Err(OptionsError::StallTooShort(Duration::from_millis(499)))
        );
        let huge = ServeConfig {
            grace_secs: u64::MAX,
            ..serve
        };
        assert_eq!(
            external_options(&huge),
            Err(OptionsError::GraceTooLong(Duration::from_secs(u64::MAX)))
        );
    }

    #[test]
    fn a_rejected_token_maps_to_invalid_token() {
        let f = Failure::from(CliError::Gateway(GatewayError::Rejected(AUTH_FAILED)));
        assert_eq!(f.code, "invalid_token");
        let f = Failure::from(CliError::Gateway(GatewayError::Exhausted(3)));
        assert_eq!(f.code, "gateway");
    }
}
