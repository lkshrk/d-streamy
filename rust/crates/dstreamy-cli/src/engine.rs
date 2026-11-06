//! The command engine behind `dstreamy daemon` and `dstreamy serve`: one Discord connection, at most one stream.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use dstreamy_core::{
    Codec, ConfigError, CropRect, Fps, HealthLevel, IdleSetting, Resolution, ServeConfig,
    StreamConfig, StreamMetrics, TOKEN_ENV, TokenSource,
};
use dstreamy_discord::{
    DiscordClient, GatewayError, HealthSnapshot, Session, SessionError, SessionEvent, SessionState,
    StreamStats,
};
use dstreamy_media::{
    CaptureConfig, CaptureSource, EXTERNAL_TARGET_ID, ExternalHandle, ExternalOptions,
    ExternalSource, IdleMode, InterruptReason, MAX_GRACE, OptionsError, ProducerInfo, SourceState,
    SourceStats,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
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

/// The live stream's health for producers, from its start until it stops.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LiveHealth {
    pub reconnecting: bool,
    /// Times the session went reconnecting.
    pub reconnect_episodes: u64,
    /// Voice or stream connection resumes, which need no reconnect.
    pub resumes: u64,
    /// The last interval's metrics; `None` until the first one.
    pub metrics: Option<StreamMetrics>,
    pub audio_level: Option<HealthLevel>,
    pub video_level: Option<HealthLevel>,
    /// Video frames dropped on the way to Discord.
    pub dropped_video: u64,
    /// Intervals in which fewer than [`AUDIO_DROPOUT_BELOW`] Opus frames reached Discord.
    pub audio_dropouts: u64,
}

/// Half of what a 20 ms Opus track sends a second.
pub const AUDIO_DROPOUT_BELOW: u64 = dstreamy_core::health::AUDIO_FRAMES_PER_SEC as u64 / 2;

impl LiveHealth {
    pub fn reconnects(&self) -> u64 {
        self.reconnect_episodes + self.resumes
    }

    /// `current` is `None` before the stream starts or after it stops.
    fn on_state(current: Option<Self>, state: &SessionState) -> Option<Self> {
        match state {
            SessionState::Connecting => Some(Self::default()),
            SessionState::Streaming => Some(Self {
                reconnecting: false,
                ..current.unwrap_or_default()
            }),
            SessionState::Reconnecting { .. } => current.map(|h| Self {
                reconnecting: true,
                reconnect_episodes: h.reconnect_episodes + u64::from(!h.reconnecting),
                ..h
            }),
            SessionState::Idle | SessionState::Stopped { .. } => None,
        }
    }

    fn on_health(current: Option<Self>, snapshot: &HealthSnapshot) -> Option<Self> {
        let current = current.unwrap_or_default();
        let dropout = snapshot.metrics.a_sent < AUDIO_DROPOUT_BELOW;
        Some(Self {
            audio_dropouts: current.audio_dropouts + u64::from(dropout),
            resumes: snapshot.stats.resumes,
            metrics: Some(snapshot.metrics),
            audio_level: Some(snapshot.health.audio_level),
            video_level: Some(snapshot.health.video_level),
            dropped_video: snapshot.stats.video_frames_dropped,
            ..current
        })
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<SourceCounters>,
}

/// What every producer lost so far, and the source queue now and at its deepest.
#[derive(Debug, Clone, Serialize)]
pub struct SourceCounters {
    pub skipped: u64,
    pub dropped_video: u64,
    pub dropped_audio: u64,
    pub queued: usize,
    pub max_queued: usize,
    pub overflow_keyframe_requests: u64,
}

impl From<SourceStats> for SourceCounters {
    fn from(s: SourceStats) -> Self {
        Self {
            skipped: s.lost.skipped,
            dropped_video: s.lost.dropped_video,
            dropped_audio: s.lost.dropped_audio,
            queued: s.queued,
            max_queued: s.max_queued,
            overflow_keyframe_requests: s.overflow_keyframe_requests,
        }
    }
}

impl From<&SourceState> for SourceView {
    fn from(state: &SourceState) -> Self {
        let view = |state| Self {
            state,
            attachment: None,
            reason: None,
            for_ms: None,
            stats: None,
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

/// Where the session streams, as of its `start` or its last channel move; omitted before a start and once it stops.
#[derive(Debug, Clone, Serialize)]
pub struct Place {
    pub guild_id: String,
    pub channel_id: String,
    /// `None` after a move to a channel the gateway's guild cache does not list.
    pub channel_name: Option<String>,
    pub target: String,
}

/// A session on the wire: its `SessionState` fields, plus its `Place` while it has one.
#[derive(Debug, Clone, Serialize)]
pub struct SessionView {
    #[serde(flatten)]
    pub state: SessionState,
    #[serde(flatten)]
    pub place: Option<Place>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub session: Option<SessionView>,
    pub source: Option<SourceView>,
    pub stats: Option<StreamStats>,
    pub started_by: Option<u64>,
    /// The live session's account, else the active one; `None` without accounts.
    pub account: Option<AccountView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccountView {
    pub name: String,
    pub username: Option<String>,
    pub global_name: Option<String>,
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
            CliError::Config(ConfigError::UnknownAccount { .. }) => "unknown_account",
            CliError::Config(ConfigError::InvalidAccountName(_)) => "invalid_params",
            CliError::Config(ConfigError::AccountWithoutToken(_)) => "not_logged_in",
            CliError::Config(ConfigError::AccountWithEnvToken) => "accounts_unavailable",
            CliError::Config(_) | CliError::ServeConfig(_) => "config",
            CliError::InvalidStreamConfig(_)
            | CliError::EmptyToken
            | CliError::AccountOfAnotherUser { .. }
            | CliError::UnknownAudioSource(_) => "invalid_params",
            CliError::NotLoggedIn => {
                return Self::new("not_logged_in", "not logged in; send set_token first");
            }
            CliError::Gateway(GatewayError::Rejected(AUTH_FAILED)) => {
                return Self::new("invalid_token", "Discord rejected the token");
            }
            CliError::Gateway(_) => "gateway",
            CliError::RemoteAuth(_)
            | CliError::BrowserLogin(_)
            | CliError::BrowserLoginCancelled
            | CliError::BrowserLoginUnavailable => "remote_auth",
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
            CliError::Service(e) => e.code(),
            CliError::AlreadyStreaming
            | CliError::AccountStreaming { .. }
            | CliError::Lock(LockError::Held { .. }) => "already_running",
            CliError::Lock(LockError::AccountGone(_)) => "unknown_account",
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
    /// Replaces the config file's `audio_source` for this session.
    pub audio_source: Option<String>,
    /// Replaces `[serve] grace_secs` for this external session only.
    pub grace_secs: Option<u64>,
}

#[derive(Deserialize)]
pub struct UpdateParams {
    pub crop: Option<CropRect>,
    pub fps: Option<Fps>,
    pub resolution: Option<Resolution>,
    pub bitrate_kbps: Option<u32>,
}

struct Conn {
    /// The token this connection was made with.
    source: TokenSource,
    client: Arc<DiscordClient>,
    session: Session,
    forwarder: JoinHandle<()>,
    catch_up: mpsc::UnboundedSender<oneshot::Sender<()>>,
    /// Tells this connection's forwarder from one an older connection left running.
    seq: u64,
}

/// A start's `Place`, and the connection whose forwarder may move it.
struct Placed {
    place: Place,
    conn: u64,
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
    audio_source: Option<String>,
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
            audio_source: self.audio_source.clone(),
            external: self.external,
        };
        let capture = stream::capture_config(
            &next.target,
            &next.stream,
            next.crop,
            next.audio_source.clone(),
        );
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
    viewers: watch::Sender<Option<u32>>,
    health: watch::Sender<Option<LiveHealth>>,
    running: Mutex<Option<Running>>,
    /// Set before the session starts, so a session that stops at once never leaves it behind.
    place: Mutex<Option<Placed>>,
    conn_seq: std::sync::atomic::AtomicU64,
    /// The token changed while a session streamed; its connection is replaced once the session is over.
    stale_token: std::sync::atomic::AtomicBool,
    /// The account chosen for the next connection; `None` follows the default account.
    account: Mutex<Option<String>>,
    started_by: Mutex<Option<u64>>,
    external: Mutex<Option<External>>,
    portal: Mutex<Option<PortalBook>>,
    session_lock: Mutex<Hold>,
    #[cfg(test)]
    start_gate: tokio::sync::Mutex<()>,
    #[cfg(test)]
    stop_gate: tokio::sync::Mutex<()>,
    /// Holding it parks every forwarder before it handles its next event, as a forwarder far behind would be.
    #[cfg(test)]
    forward_gate: tokio::sync::Mutex<()>,
    #[cfg(test)]
    moves_applied: Mutex<Vec<Place>>,
    #[cfg(test)]
    catch_ups_requested: std::sync::atomic::AtomicUsize,
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

/// Width, height and fps as op 12 announces them.
pub type VideoSize = (u32, u32, u32);

struct External {
    handle: ExternalHandle,
    announced: watch::Receiver<VideoSize>,
    watcher: JoinHandle<()>,
    announcer: JoinHandle<()>,
}

impl Drop for External {
    fn drop(&mut self) {
        self.watcher.abort();
        self.announcer.abort();
        // A stopped session keeps its source for a restart; no producer may feed it meanwhile.
        self.handle.end();
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
            viewers: watch::Sender::new(None),
            health: watch::Sender::new(None),
            running: Mutex::new(None),
            place: Mutex::new(None),
            conn_seq: std::sync::atomic::AtomicU64::new(0),
            stale_token: std::sync::atomic::AtomicBool::new(false),
            account: Mutex::new(ctx.account.clone()),
            started_by: Mutex::new(None),
            external: Mutex::new(None),
            portal: Mutex::new(None),
            session_lock: Mutex::new(Hold::default()),
            #[cfg(test)]
            start_gate: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            stop_gate: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            forward_gate: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            moves_applied: Mutex::new(Vec::new()),
            #[cfg(test)]
            catch_ups_requested: std::sync::atomic::AtomicUsize::new(0),
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
                let logged_in = match crate::accounts::resolve(&self.selected_ctx()) {
                    Ok(_) => true,
                    Err(
                        CliError::NotLoggedIn
                        | CliError::Config(ConfigError::AccountWithoutToken(_)),
                    ) => false,
                    Err(e) => return Err(e.into()),
                };
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
        let session = self.current().map(|c| {
            let state = c.session.state().borrow().clone();
            let place = lock(&self.place)
                .as_ref()
                .map(|p| p.place.clone())
                .filter(|_| !matches!(state, SessionState::Stopped { .. }));
            SessionView { state, place }
        });
        let source = lock(&self.external).as_ref().map(|e| SourceView {
            stats: e.handle.stats().map(SourceCounters::from),
            ..SourceView::from(&*e.handle.state().borrow())
        });
        Status {
            started_by: session.as_ref().and(*lock(&self.started_by)),
            session,
            source,
            stats: lock(&self.last_stats).clone(),
            account: self.active_account().map(|name| self.account_view(name)),
        }
    }

    /// The context with the account the next connection uses.
    fn selected_ctx(&self) -> Ctx {
        Ctx {
            account: lock(&self.account).clone(),
            ..self.ctx.clone()
        }
    }

    fn selected_source(&self) -> Option<TokenSource> {
        crate::accounts::resolve(&self.selected_ctx())
            .ok()
            .map(|r| r.source)
    }

    /// The live session's account, else the account the next connection uses.
    pub fn active_account(&self) -> Option<String> {
        let source = match self.current().filter(|c| c.live()) {
            Some(conn) => Some(conn.source.clone()),
            None => self.selected_source(),
        };
        source.and_then(|s| s.account().map(str::to_owned))
    }

    fn account_view(&self, name: String) -> AccountView {
        let user = self.ctx.paths.accounts().user(&name);
        AccountView {
            name,
            username: user.as_ref().map(|u| u.username.clone()),
            global_name: user.and_then(|u| u.global_name),
        }
    }

    /// Whether the token comes from `DSTREAMY_TOKEN`, which leaves no accounts to manage.
    pub fn env_token() -> bool {
        std::env::var(TOKEN_ENV).is_ok_and(|t| !t.trim().is_empty())
    }

    fn accounts_available() -> Result<(), Failure> {
        if Self::env_token() {
            return Err(Failure::new(
                "accounts_unavailable",
                format!(
                    "the service takes its token from {TOKEN_ENV}, so it has no accounts: unset it for the service and restart it"
                ),
            ));
        }
        Ok(())
    }

    /// The saved accounts and the active one.
    pub fn accounts(&self) -> Result<Value, Failure> {
        Self::accounts_available()?;
        let active = self.active_account();
        let list = self.ctx.paths.accounts().list().map_err(CliError::from)?;
        let rows: Vec<Value> = list
            .into_iter()
            .map(|a| {
                json!({
                    "active": active.as_deref() == Some(a.name.as_str()),
                    "username": a.user.as_ref().map(|u| &u.username),
                    "global_name": a.user.as_ref().and_then(|u| u.global_name.as_ref()),
                    "has_token": a.has_token,
                    "name": a.name,
                })
            })
            .collect();
        Ok(json!({ "active": active, "accounts": rows }))
    }

    /// Makes `name` the default account and the next connection's; refused while a session is live.
    pub async fn select_account(&self, name: &str) -> Result<Value, Failure> {
        Self::accounts_available()?;
        let _lifecycle = self.lifecycle.lock().await;
        self.ctx
            .paths
            .accounts()
            .require(name)
            .map_err(CliError::from)?;
        if self.is_live() {
            return Err(Failure::new(
                "streaming",
                "stop the stream before switching accounts",
            ));
        }
        let has_token = self
            .ctx
            .paths
            .accounts()
            .load_token(name)
            .ok()
            .flatten()
            .is_some();
        if !has_token {
            return Err(Failure::new(
                "not_logged_in",
                format!(
                    "account {name} has no saved login: log in to {name} first (`dstreamy login --account {name}`), or log it out"
                ),
            ));
        }
        self.make_default(name)?;
        if self
            .current()
            .is_some_and(|c| c.source.account() != Some(name))
        {
            self.disconnect_locked().await;
        }
        drop(_lifecycle);
        self.accounts()
    }

    /// Saves `name` as the default; serve then follows it, unless it was started with `--account`.
    fn make_default(&self, name: &str) -> Result<(), Failure> {
        self.ctx
            .paths
            .accounts()
            .set_default_locked(name, crate::accounts::STORE_LOCK_WAIT)
            .map_err(CliError::from)?;
        *lock(&self.account) = self.ctx.account.is_some().then(|| name.to_owned());
        Ok(())
    }

    /// Deletes a saved account; refused while it streams here or in another process.
    pub async fn logout_account(&self, name: &str) -> Result<Value, Failure> {
        Self::accounts_available()?;
        let _lifecycle = self.lifecycle.lock().await;
        let accounts = self.ctx.paths.accounts();
        accounts.require(name).map_err(CliError::from)?;
        let source = TokenSource::Account(name.to_owned());
        let conn = self.current();
        if conn
            .as_ref()
            .is_some_and(|c| c.live() && c.source == source)
        {
            return Err(Failure::new(
                "streaming",
                "stop the stream before logging this account out",
            ));
        }
        let store = crate::accounts::store_lock(&self.ctx)?;
        let held = crate::accounts::removal_lock(&self.ctx, name).map_err(CliError::from)?;
        accounts.remove(name).map_err(CliError::from)?;
        drop(store);
        drop(held);
        {
            let mut selected = lock(&self.account);
            if selected.as_deref() == Some(name) {
                *selected = None;
            }
        }
        if conn.is_some_and(|c| c.source == source) {
            self.disconnect_locked().await;
        }
        drop(_lifecycle);
        tracing::info!("logged out account {name}");
        self.accounts()
    }

    /// A login saved into `account` makes it the default; an older connection goes once idle.
    pub async fn logged_in_as(&self, account: Option<String>) {
        if let Some(name) = account
            && let Err(e) = self.make_default(&name)
        {
            tracing::warn!("making account {name} the default: {}", e.message);
            *lock(&self.account) = Some(name);
        }
        self.token_changed().await;
    }

    /// The live session's viewer count, `None` while unknown.
    pub fn viewers(&self) -> watch::Receiver<Option<u32>> {
        self.viewers.subscribe()
    }

    /// The live stream's health, `None` while no stream runs.
    pub fn health(&self) -> watch::Receiver<Option<LiveHealth>> {
        self.health.subscribe()
    }

    /// Health stops while not streaming, so the last count would go stale.
    fn forget_viewers(&self) {
        if let Some(stats) = lock(&self.last_stats).as_mut() {
            stats.viewers = None;
        }
        self.viewers.send_if_modified(|v| v.take().is_some());
    }

    /// The live external session's source, for attaching producers.
    /// `None` unless the session is live, so a producer never attaches to the source of a stopped run.
    pub fn external(&self) -> Option<(ExternalHandle, watch::Receiver<VideoSize>)> {
        self.current().filter(|c| c.live())?;
        lock(&self.external)
            .as_ref()
            .map(|e| (e.handle.clone(), e.announced.clone()))
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

    pub fn is_live(&self) -> bool {
        self.current().is_some_and(|c| c.live())
    }

    /// The saved token changed: the Discord connection made with the old one is closed once no session streams on it.
    pub async fn token_changed(&self) {
        let _lifecycle = self.lifecycle.lock().await;
        if self.is_live() {
            self.stale_token.store(true, Ordering::Release);
        } else {
            self.stale_token.store(false, Ordering::Release);
            self.disconnect_locked().await;
        }
    }

    fn reusable(&self, conn: &Conn) -> bool {
        conn.live()
            || (!self.stale_token.load(Ordering::Acquire)
                && self.selected_source().as_ref() == Some(&conn.source))
    }

    async fn conn(&self) -> Result<Arc<Conn>, CliError> {
        if let Some(conn) = self.current()
            && self.reusable(&conn)
        {
            return Ok(conn);
        }
        let _lifecycle = self.lifecycle.lock().await;
        self.ensure_conn().await
    }

    /// Needs `lifecycle` held.
    async fn ensure_conn(&self) -> Result<Arc<Conn>, CliError> {
        if let Some(conn) = self.current() {
            if self.reusable(&conn) {
                return Ok(conn);
            }
            drop(conn);
            self.disconnect_locked().await;
        }
        let engine = self
            .me
            .upgrade()
            .expect("an engine is only reachable through its Arc");
        let ctx = self.selected_ctx();
        let resolved = crate::accounts::resolve(&ctx)?;
        let source = resolved.source.clone();
        let client = Arc::new(discord::connect_resolved(&ctx, resolved, Persistence::Long).await?);
        let session = Session::new(client.clone());
        let seq = self
            .conn_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let (catch_up, catch_ups) = mpsc::unbounded_channel();
        let forwarder = tokio::spawn(engine.forward(session.subscribe(), catch_ups, seq));
        let conn = Arc::new(Conn {
            source,
            client,
            session,
            forwarder,
            catch_up,
            seq,
        });
        *lock(&self.conn) = Some(conn.clone());
        self.stale_token.store(false, Ordering::Release);
        Ok(conn)
    }

    /// Answers each catch-up request once it has handled every event the session sent before it.
    async fn forward(
        self: Arc<Self>,
        mut events: broadcast::Receiver<SessionEvent>,
        mut catch_ups: mpsc::UnboundedReceiver<oneshot::Sender<()>>,
        conn: u64,
    ) {
        let mut waiting: Vec<oneshot::Sender<()>> = Vec::new();
        loop {
            if events.is_empty() {
                for caught_up in waiting.drain(..) {
                    let _ = caught_up.send(());
                }
            }
            let received = tokio::select! {
                biased;
                received = events.recv() => received,
                Some(caught_up) = catch_ups.recv() => {
                    waiting.push(caught_up);
                    continue;
                }
            };
            #[cfg(test)]
            drop(self.forward_gate.lock().await);
            match received {
                Ok(event) => {
                    match &event {
                        SessionEvent::Health(h) => {
                            *lock(&self.last_stats) = Some(h.stats.clone());
                            self.viewers.send_if_modified(|v| {
                                std::mem::replace(v, h.stats.viewers) != h.stats.viewers
                            });
                            self.health.send_if_modified(|v| {
                                let next = LiveHealth::on_health(*v, h);
                                std::mem::replace(v, next) != next
                            });
                        }
                        SessionEvent::State { state } => {
                            self.health.send_if_modified(|v| {
                                let next = LiveHealth::on_state(*v, state);
                                std::mem::replace(v, next) != next
                            });
                            if *state != SessionState::Streaming {
                                self.forget_viewers();
                            }
                            self.remember_portal_token();
                            self.release_if_stopped();
                        }
                        SessionEvent::ChannelMoved {
                            guild_id,
                            channel_id,
                        } => self.moved(conn, guild_id.as_deref(), channel_id),
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

    fn moved(&self, from: u64, guild_id: Option<&str>, channel_id: &str) {
        let Some(conn) = self.current() else { return };
        let mut placed = lock(&self.place);
        let Some(Placed { place, .. }) = placed.as_mut().filter(|p| p.conn == from) else {
            return;
        };
        if let Some(guild_id) = guild_id {
            place.guild_id = guild_id.to_owned();
        }
        place.channel_id = channel_id.to_owned();
        place.channel_name = conn
            .client
            .voice_channels(&place.guild_id)
            .into_iter()
            .find(|c| c.id == channel_id)
            .map(|c| c.name);
        #[cfg(test)]
        lock(&self.moves_applied).push(place.clone());
    }

    /// Waits, bounded, until `conn`'s forwarder has handled every event its session sent so far.
    async fn catch_up(&self, conn: &Conn) -> bool {
        let (caught_up, handled) = oneshot::channel();
        // A forwarder that is gone has nothing left to handle.
        if conn.catch_up.send(caught_up).is_err() {
            return true;
        }
        #[cfg(test)]
        self.catch_ups_requested
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let behind = timeout(FORWARDER_DRAIN, handled).await.is_err();
        if behind {
            tracing::warn!("session events are still queued after {FORWARDER_DRAIN:?}");
        }
        !behind
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
            *lock(&self.place) = None;
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
        let user = crate::accounts::account_user(client.user());
        client.close().await;
        let accounts = self.ctx.paths.accounts();
        let selected = lock(&self.account).clone();
        let name = match selected {
            Some(name) => name,
            None => match accounts.default_name().map_err(CliError::from)? {
                Some(name) => name,
                None => accounts.name_for(&user).map_err(CliError::from)?,
            },
        };
        accounts
            .save(&name, &token, Some(&user))
            .map_err(CliError::from)?;
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
        let mut external_options = external
            .then(|| external_options(&config.serve))
            .transpose()
            .map_err(CliError::from)?;
        // The protocol 1 daemon never had grace_secs and ignores fields it does not know.
        let grace_secs = p.grace_secs.filter(|_| self.opts.allow_external);
        match (grace_secs, external_options.as_mut()) {
            (None, _) => {}
            (Some(_), None) => {
                return Err(invalid_params(
                    "grace_secs applies only to an external session",
                ));
            }
            (Some(secs), Some(options)) => {
                if Duration::from_secs(secs) > MAX_GRACE {
                    return Err(invalid_params(format!(
                        "grace_secs {secs} is longer than {} s",
                        MAX_GRACE.as_secs()
                    )));
                }
                options.grace = Duration::from_secs(secs);
            }
        }
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
        // The last run may have stopped itself; its queued events must not touch this start's place.
        let caught_up = self.catch_up(&conn).await;
        let channels = discord::voice_channels(&conn.client, &p.guild_id).await?;
        let Some(channel) = channels.into_iter().find(|c| c.id == p.channel_id) else {
            return Err(CliError::UnknownChannel {
                guild: p.guild_id,
                channel: p.channel_id,
            }
            .into());
        };
        let place = Place {
            guild_id: p.guild_id.clone(),
            channel_id: p.channel_id.clone(),
            channel_name: Some(channel.name),
            target: p.target.clone(),
        };
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
        let audio_source = p.audio_source.or(config.audio_source);
        if let Some(id) = &audio_source
            && handle.is_none()
            && let Some(warning) = capture::verify_audio_source(&p.target, id, &*source).await?
        {
            tracing::warn!("{warning}");
        }
        let capture = stream::capture_config(&p.target, &settings, p.crop, audio_source.clone());
        {
            let mut hold = lock(&self.session_lock);
            if hold.lock.is_none() {
                hold.lock = Some(
                    crate::accounts::session_lock(&self.ctx, &conn.source)
                        .map_err(CliError::from)?,
                );
            }
            hold.starting = true;
        }
        *lock(&self.place) = caught_up.then(|| Placed {
            place,
            conn: conn.seq,
        });
        #[cfg(test)]
        drop(self.start_gate.lock().await);
        let started = stream::start(&conn.session, p.guild_id, p.channel_id, source, capture).await;
        {
            let mut hold = lock(&self.session_lock);
            hold.starting = false;
            hold.release_if(started.is_err() || stream::has_stopped(&conn.session));
        }
        if started.is_err() {
            *lock(&self.place) = None;
        }
        started?;
        *lock(&self.last_stats) = None;
        self.viewers.send_replace(None);
        *lock(&self.portal) = Some(PortalBook::new(restore_token, portal_token));
        *lock(&self.started_by) = caller;
        let announced = (
            settings.resolution.width(),
            settings.resolution.height(),
            settings.fps.get(),
        );
        *lock(&self.external) = handle.map(|handle| self.watch_source(handle, announced));
        self.release_if_stopped();
        *lock(&self.running) = Some(Running {
            target: p.target,
            stream: settings,
            crop: p.crop,
            audio_source,
            external,
        });
        Ok(Value::Null)
    }

    fn watch_source(&self, handle: ExternalHandle, announced: VideoSize) -> External {
        let (announced_tx, announced_rx) = watch::channel(announced);
        let announcer = tokio::spawn(announce(self.me.clone(), handle.producer(), announced_tx));
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
        External {
            handle,
            announced: announced_rx,
            watcher,
            announcer,
        }
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
        self.catch_up(&conn).await;
        self.release_if_stopped();
        if stopped.is_none() {
            return Err(not_running());
        }
        *lock(&self.running) = None;
        *lock(&self.place) = None;
        *lock(&self.started_by) = None;
        *lock(&self.external) = None;
        self.release_if_stopped();
        let stats = lock(&self.last_stats).take().unwrap_or_default();
        self.viewers.send_replace(None);
        self.health.send_replace(None);
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
        self.stale_token.store(false, Ordering::Release);
        match Arc::try_unwrap(conn) {
            Ok(Conn {
                client,
                session,
                forwarder,
                ..
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
        *lock(&self.place) = None;
        *lock(&self.started_by) = None;
        *lock(&self.external) = None;
        *lock(&self.session_lock) = Hold::default();
        self.remember_portal_token();
    }
}

/// At most one op 12 per this interval; producers attaching faster get the latest size once it passes.
pub const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(1);

/// Sends op 12 whenever an attached producer's size or fps differs from what the session last announced.
async fn announce(
    engine: Weak<Engine>,
    mut producers: watch::Receiver<Option<ProducerInfo>>,
    announced: watch::Sender<VideoSize>,
) {
    let mut last_sent: Option<tokio::time::Instant> = None;
    loop {
        let declared = producers
            .borrow_and_update()
            .as_ref()
            .map(|p| (p.width, p.height, p.fps));
        if let Some(declared) = declared.filter(|d| *d != *announced.borrow()) {
            if let Some(at) = last_sent {
                tokio::time::sleep_until(at + ANNOUNCE_INTERVAL).await;
                if producers.has_changed().unwrap_or(false) {
                    continue;
                }
            }
            let Some(engine) = engine.upgrade() else {
                return;
            };
            let (width, height, fps) = declared;
            last_sent = Some(tokio::time::Instant::now());
            match engine.apply_producer_video(width, height, fps).await {
                Ok(()) => {
                    announced.send_replace(declared);
                }
                Err(e) => tracing::warn!("announcing the producer's video: {}", e.message),
            }
        }
        if producers.changed().await.is_err() {
            return;
        }
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

    fn snapshot(resumes: u64, dropped: u64, a_sent: u64) -> HealthSnapshot {
        let metrics = StreamMetrics {
            fps: 60,
            a_sent,
            ..StreamMetrics::default()
        };
        HealthSnapshot {
            metrics,
            health: dstreamy_core::StreamHealth::compute(&[metrics], 60),
            stats: StreamStats {
                resumes,
                video_frames_dropped: dropped,
                ..StreamStats::default()
            },
        }
    }

    #[test]
    fn live_health_counts_reconnects_and_resumes_per_stream() {
        let reconnecting = SessionState::Reconnecting { attempt: 1 };
        let mut h = LiveHealth::on_state(None, &SessionState::Connecting);
        assert_eq!(h, Some(LiveHealth::default()));
        h = LiveHealth::on_state(h, &SessionState::Streaming);
        h = LiveHealth::on_health(h, &snapshot(1, 4, 50));
        let got = h.unwrap();
        assert_eq!((got.reconnects(), got.dropped_video), (1, 4));
        assert_eq!(got.metrics.map(|m| m.a_sent), Some(50));
        assert_eq!(got.audio_level, Some(HealthLevel::Good));
        h = LiveHealth::on_state(h, &reconnecting);
        h = LiveHealth::on_state(h, &SessionState::Reconnecting { attempt: 2 });
        let got = h.unwrap();
        assert!(got.reconnecting);
        assert_eq!(got.reconnects(), 2, "one episode, however many attempts");
        assert_eq!(
            got.metrics.map(|m| m.a_sent),
            Some(50),
            "the last figures stay"
        );
        h = LiveHealth::on_state(h, &SessionState::Streaming);
        h = LiveHealth::on_state(h, &reconnecting);
        assert_eq!(h.unwrap().reconnects(), 3);
        h = LiveHealth::on_state(h, &SessionState::Streaming);
        assert!(!h.unwrap().reconnecting);
        let stopped = SessionState::Stopped {
            reason: dstreamy_discord::StopReason::Requested,
        };
        assert_eq!(LiveHealth::on_state(h, &stopped), None);
        assert_eq!(
            LiveHealth::on_state(h, &SessionState::Connecting),
            Some(LiveHealth::default()),
            "a new stream counts from zero"
        );
        assert_eq!(LiveHealth::on_state(None, &reconnecting), None);
    }

    #[test]
    fn live_health_counts_intervals_with_too_little_audio_per_stream() {
        let mut h = LiveHealth::on_state(None, &SessionState::Connecting);
        h = LiveHealth::on_state(h, &SessionState::Streaming);
        h = LiveHealth::on_health(h, &snapshot(0, 0, 50));
        assert_eq!(h.unwrap().audio_dropouts, 0);
        h = LiveHealth::on_health(h, &snapshot(0, 0, AUDIO_DROPOUT_BELOW));
        assert_eq!(
            h.unwrap().audio_dropouts,
            0,
            "half the frames still count as flowing"
        );
        h = LiveHealth::on_health(h, &snapshot(0, 0, AUDIO_DROPOUT_BELOW - 1));
        h = LiveHealth::on_health(h, &snapshot(0, 0, 0));
        assert_eq!(h.unwrap().audio_dropouts, 2);
        h = LiveHealth::on_state(h, &SessionState::Reconnecting { attempt: 1 });
        h = LiveHealth::on_state(h, &SessionState::Streaming);
        assert_eq!(h.unwrap().audio_dropouts, 2, "kept across a reconnect");
        assert_eq!(
            LiveHealth::on_state(h, &SessionState::Connecting).map(|h| h.audio_dropouts),
            Some(0),
            "a new stream counts from zero"
        );
    }

    #[test]
    fn service_errors_keep_their_own_codes() {
        use crate::service::ServiceError;
        let code = |e| Failure::from(CliError::Service(e)).code;
        assert_eq!(code(ServiceError::Unsupported), "unsupported_platform");
        assert_eq!(code(ServiceError::NoHome), "config");
        let spawn = |kind| ServiceError::Spawn {
            command: "systemctl".into(),
            source: std::io::Error::from(kind),
        };
        assert_eq!(code(spawn(std::io::ErrorKind::NotFound)), "service_manager");
        assert_eq!(
            code(spawn(std::io::ErrorKind::PermissionDenied)),
            "internal"
        );
        assert_eq!(
            code(ServiceError::Timeout {
                command: "systemctl".into(),
                after: std::time::Duration::from_secs(5),
            }),
            "service_manager"
        );
    }

    #[test]
    fn an_update_carries_the_crop_and_keeps_the_rest() {
        let running = Running {
            target: "display:1".into(),
            stream: StreamConfig::default(),
            crop: None,
            audio_source: Some("pipeweaver_stream".into()),
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
        assert_eq!(capture.audio_source.as_deref(), Some("pipeweaver_stream"));
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
        let mut hold = Hold {
            lock: Some(SessionLock::acquire(dir.path()).unwrap()),
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
            MOCK_GUILD_ID, MOCK_OTHER_VOICE_CHANNEL_ID, MOCK_TOKEN, MOCK_VOICE_CHANNEL_ID,
            MockDiscord,
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
                Self::with_external(true).await
            }

            async fn with_external(allow_external: bool) -> Self {
                let mock = MockDiscord::start().await.unwrap();
                let dir = tempfile::tempdir().unwrap();
                let paths = ConfigPaths::in_dir(dir.path());
                paths.save_token(MOCK_TOKEN).unwrap();
                let ctx = Ctx {
                    paths,
                    gateway_url: mock.gateway_url(),
                    account: None,
                };
                let opts = Options {
                    include_platform: false,
                    allow_external,
                    ..Options::default()
                };
                let engine = Engine::new(&ctx, opts);
                Self { mock, dir, engine }
            }

            fn lock_is_free(&self) -> bool {
                SessionLock::acquire(&self.dir.path().join("accounts/default")).is_ok()
            }

            /// Spawns a start and returns once it is parked on `start_gate` with `starting` set.
            async fn start_in_flight(
                &self,
            ) -> (
                JoinHandle<Result<Value, Failure>>,
                tokio::sync::MutexGuard<'_, ()>,
            ) {
                self.start_in_flight_with(params()).await
            }

            async fn start_in_flight_with(
                &self,
                p: StartParams,
            ) -> (
                JoinHandle<Result<Value, Failure>>,
                tokio::sync::MutexGuard<'_, ()>,
            ) {
                let gate = self.engine.start_gate.lock().await;
                let engine = self.engine.clone();
                let task = tokio::spawn(async move { engine.start(p, None).await });
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
                audio_source: None,
                grace_secs: None,
            }
        }

        fn external(grace_secs: Option<u64>) -> StartParams {
            StartParams {
                target: EXTERNAL_TARGET_ID.into(),
                grace_secs,
                ..params()
            }
        }

        /// How long an external session with no producer streams before its grace period ends it.
        async fn streams_without_a_producer_for(f: &Fixture, p: StartParams) -> Duration {
            f.engine.start(p, None).await.unwrap();
            f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
            let streaming = std::time::Instant::now();
            f.wait_state(|s| matches!(s, SessionState::Stopped { .. }))
                .await;
            streaming.elapsed()
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_start_grace_secs_replaces_the_configured_grace_for_that_session() {
            let f = Fixture::new().await;
            std::fs::write(
                f.dir.path().join("config.toml"),
                "[serve]\ngrace_secs = 30\n",
            )
            .unwrap();
            let lasted = streams_without_a_producer_for(&f, external(Some(1))).await;
            assert!(lasted < Duration::from_secs(10), "{lasted:?}");
            f.engine.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_start_without_grace_secs_keeps_the_configured_grace() {
            let f = Fixture::new().await;
            std::fs::write(
                f.dir.path().join("config.toml"),
                "[serve]\ngrace_secs = 2\n",
            )
            .unwrap();
            let lasted = streams_without_a_producer_for(&f, external(None)).await;
            assert!(
                lasted >= Duration::from_millis(1_500) && lasted < Duration::from_secs(10),
                "{lasted:?}"
            );
            f.engine.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_grace_secs_over_the_limit_or_for_a_captured_target_is_invalid() {
            let f = Fixture::new().await;
            let over = f
                .engine
                .start(external(Some(3601)), None)
                .await
                .unwrap_err();
            assert_eq!(over.code, "invalid_params", "{}", over.message);
            let captured = StartParams {
                grace_secs: Some(5),
                ..params()
            };
            let captured = f.engine.start(captured, None).await.unwrap_err();
            assert_eq!(captured.code, "invalid_params", "{}", captured.message);
            assert!(f.engine.current().is_none_or(|c| !c.live()));
            f.engine.start(external(Some(3600)), None).await.unwrap();
            f.engine.shutdown().await;
        }

        fn status_place(f: &Fixture) -> [Value; 4] {
            let status = serde_json::to_value(f.engine.status()).unwrap();
            ["guild_id", "channel_id", "channel_name", "target"]
                .map(|k| status["session"][k].clone())
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_grace_secs_of_zero_is_accepted() {
            let f = Fixture::new().await;
            f.engine.start(external(Some(0)), None).await.unwrap();
            f.engine.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn protocol_1_ignores_grace_secs() {
            let f = Fixture::with_external(false).await;
            let p = StartParams {
                grace_secs: Some(5),
                ..params()
            };
            f.engine.start(p, None).await.unwrap();
            f.engine.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn status_follows_a_move_to_another_channel() {
            let f = Fixture::new().await;
            f.engine.start(params(), None).await.unwrap();
            f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
            assert!(
                f.mock
                    .move_to_channel(MOCK_GUILD_ID, MOCK_OTHER_VOICE_CHANNEL_ID)
            );
            timeout(T, async {
                while status_place(&f)[1] != json!(MOCK_OTHER_VOICE_CHANNEL_ID) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("status still names {:?}", status_place(&f)));
            assert_eq!(
                status_place(&f),
                [
                    json!(MOCK_GUILD_ID),
                    json!(MOCK_OTHER_VOICE_CHANNEL_ID),
                    json!("Gaming"),
                    json!("test-pattern")
                ]
            );
            f.engine.shutdown().await;
        }

        async fn wait_viewers(rx: &mut watch::Receiver<Option<u32>>, want: Option<u32>) {
            let waited = timeout(T, rx.wait_for(|v| *v == want))
                .await
                .map(|r| r.is_ok());
            assert_eq!(waited, Ok(true), "viewers stuck at {:?}", *rx.borrow());
        }

        fn status_viewers(f: &Fixture) -> Value {
            serde_json::to_value(f.engine.status()).unwrap()["stats"]["viewers"].clone()
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn viewers_turn_unknown_while_reconnecting_or_moved() {
            use dstreamy_mock_discord::VoiceKind;
            use dstreamy_mock_discord::opcodes::voice::CLOSE_SESSION_NO_LONGER_VALID;
            let triggers: [fn(&MockDiscord) -> bool; 2] = [
                |m| m.drop_voice(VoiceKind::Stream, Some(CLOSE_SESSION_NO_LONGER_VALID)),
                |m| m.move_to_channel(MOCK_GUILD_ID, MOCK_OTHER_VOICE_CHANNEL_ID),
            ];
            for trigger in triggers {
                let f = Fixture::new().await;
                f.engine.start(params(), None).await.unwrap();
                f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
                let mut producer = f.engine.viewers();
                assert!(
                    f.mock
                        .set_viewers(&["400000000000000001", "400000000000000002"])
                );
                wait_viewers(&mut producer, Some(2)).await;
                assert_eq!(status_viewers(&f), json!(2));

                f.mock.ignore_stream_creates(usize::MAX);
                assert!(trigger(&f.mock));
                f.wait_state(|s| !matches!(s, SessionState::Streaming))
                    .await;
                wait_viewers(&mut producer, None).await;
                assert_eq!(status_viewers(&f), Value::Null);
                f.engine.shutdown().await;
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn start_and_stop_reset_the_viewers_a_producer_sees() {
            let f = Fixture::new().await;
            let mut producer = f.engine.viewers();
            f.engine.viewers.send_replace(Some(5));
            f.engine.start(params(), None).await.unwrap();
            assert_ne!(
                *producer.borrow_and_update(),
                Some(5),
                "start kept the old count"
            );
            f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
            assert!(f.mock.set_viewers(&["400000000000000001"]));
            wait_viewers(&mut producer, Some(1)).await;
            f.engine.stop().await.unwrap();
            assert_eq!(*producer.borrow_and_update(), None, "stop kept the count");
            f.engine.shutdown().await;
        }

        /// Runs `events` through a forwarder of connection `conn`, as a leftover one would.
        async fn forward_events(f: &Fixture, conn: u64, events: &[SessionEvent]) {
            let (tx, rx) = broadcast::channel(16);
            for e in events {
                tx.send(e.clone()).unwrap();
            }
            drop(tx);
            let (_catch_up, catch_ups) = mpsc::unbounded_channel();
            timeout(T, f.engine.clone().forward(rx, catch_ups, conn))
                .await
                .unwrap();
        }

        fn moved_to_gaming() -> SessionEvent {
            SessionEvent::ChannelMoved {
                guild_id: Some(MOCK_GUILD_ID.into()),
                channel_id: MOCK_OTHER_VOICE_CHANNEL_ID.into(),
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_move_from_another_connection_leaves_the_place_alone() {
            let f = Fixture::new().await;
            f.engine.start(params(), None).await.unwrap();
            f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
            let conn = f.engine.current().unwrap().seq;
            f.engine.current().unwrap().forwarder.abort();
            forward_events(&f, conn + 1, &[moved_to_gaming()]).await;
            assert_eq!(
                status_place(&f)[1],
                json!(MOCK_VOICE_CHANNEL_ID),
                "another connection's move"
            );
            forward_events(&f, conn, &[moved_to_gaming()]).await;
            assert_eq!(status_place(&f)[1], json!(MOCK_OTHER_VOICE_CHANNEL_ID));
            f.engine.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_lagged_forwarder_catches_up_without_waiting_out_the_bound() {
            let f = Fixture::new().await;
            let (tx, rx) = broadcast::channel(4);
            for _ in 0..20 {
                tx.send(moved_to_gaming()).unwrap();
            }
            let (catch_up, catch_ups) = mpsc::unbounded_channel();
            let forwarder = tokio::spawn(f.engine.clone().forward(rx, catch_ups, u64::MAX));
            let (caught_up, handled) = oneshot::channel();
            catch_up.send(caught_up).unwrap();
            timeout(FORWARDER_DRAIN / 4, handled)
                .await
                .expect("the lagged forwarder never caught up")
                .unwrap();
            drop(tx);
            timeout(T, forwarder).await.unwrap().unwrap();
        }

        fn gaming(p: StartParams) -> StartParams {
            StartParams {
                channel_id: MOCK_OTHER_VOICE_CHANNEL_ID.into(),
                ..p
            }
        }

        /// Runs `first` in Gaming with the forwarder parked behind its whole run, moves it to General, then starts `second`.
        /// Returns the moves applied to `second`'s place.
        async fn moves_after_a_move_queued_behind(
            f: &Fixture,
            first: StartParams,
            second: StartParams,
        ) -> Vec<Place> {
            let conn = f.engine.conn().await.unwrap();
            let mut probe = conn.session.subscribe();
            assert!(f.engine.catch_up(&conn).await);
            let gate = f.engine.forward_gate.lock().await;
            let self_stops = first.target == EXTERNAL_TARGET_ID;
            let second_target = second.target.clone();
            f.engine.start(first, None).await.unwrap();
            f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
            assert!(f.mock.move_to_channel(MOCK_GUILD_ID, MOCK_VOICE_CHANNEL_ID));
            timeout(T, async {
                while !matches!(probe.recv().await, Ok(SessionEvent::ChannelMoved { .. })) {}
            })
            .await
            .expect("the first run never moved");
            if self_stops {
                f.wait_state(|s| matches!(s, SessionState::Stopped { .. }))
                    .await;
            }
            let requested = || {
                f.engine
                    .catch_ups_requested
                    .load(std::sync::atomic::Ordering::SeqCst)
            };
            let before = requested();
            let engine = f.engine.clone();
            let restart = tokio::spawn(async move {
                if !self_stops {
                    engine.stop().await.unwrap();
                }
                engine.start(second, None).await.unwrap();
            });
            timeout(T, async {
                while requested() == before && !restart.is_finished() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            let waited = !restart.is_finished();
            drop(gate);
            timeout(T, restart).await.unwrap().unwrap();
            assert!(
                waited,
                "the restart went ahead of the first run's queued events"
            );
            assert!(f.engine.catch_up(&f.engine.current().unwrap()).await);
            assert_eq!(status_place(f)[1], json!(MOCK_OTHER_VOICE_CHANNEL_ID));
            lock(&f.engine.moves_applied)
                .iter()
                .filter(|p| p.target == second_target)
                .cloned()
                .collect()
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_stopped_runs_queued_move_never_moves_the_next_start() {
            let f = Fixture::new().await;
            let moves =
                moves_after_a_move_queued_behind(&f, gaming(params()), gaming(external(None)))
                    .await;
            assert!(moves.is_empty(), "{moves:?}");
            f.engine.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_self_stopped_runs_queued_move_never_moves_the_next_start() {
            let f = Fixture::new().await;
            let moves =
                moves_after_a_move_queued_behind(&f, gaming(external(Some(1))), gaming(params()))
                    .await;
            assert!(moves.is_empty(), "{moves:?}");
            f.engine.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_session_that_stops_itself_leaves_no_place_behind() {
            let f = Fixture::new().await;
            streams_without_a_producer_for(&f, external(Some(1))).await;
            timeout(T, async {
                while !f.lock_is_free() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the stopped session kept the lock");
            assert!(lock(&f.engine.place).is_none());
            assert!(status_place(&f).iter().all(Value::is_null));
            f.engine.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_stopped_session_hides_its_place_before_the_stop_is_handled() {
            let f = Fixture::new().await;
            f.engine.start(external(Some(1)), None).await.unwrap();
            f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
            f.engine.current().unwrap().forwarder.abort();
            f.wait_state(|s| matches!(s, SessionState::Stopped { .. }))
                .await;
            assert!(lock(&f.engine.place).is_some());
            assert!(
                status_place(&f).iter().all(Value::is_null),
                "{:?}",
                status_place(&f)
            );
            f.engine.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn status_names_the_guild_channel_and_target_of_the_live_session() {
            let f = Fixture::new().await;
            let place = |f: &Fixture| {
                let status = serde_json::to_value(f.engine.status()).unwrap();
                ["guild_id", "channel_id", "channel_name", "target"]
                    .map(|k| status["session"][k].clone())
            };
            f.engine.start(params(), None).await.unwrap();
            f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
            assert_eq!(
                place(&f),
                [
                    json!(MOCK_GUILD_ID),
                    json!(MOCK_VOICE_CHANNEL_ID),
                    json!("General"),
                    json!("test-pattern")
                ]
            );
            assert_eq!(
                serde_json::to_value(f.engine.status()).unwrap()["session"]["state"],
                "streaming"
            );
            f.engine.stop().await.unwrap();
            let gone = serde_json::to_value(f.engine.status()).unwrap();
            assert!(
                place(&f).iter().all(Value::is_null),
                "a stopped session still names its place: {gone}"
            );
            f.engine.shutdown().await;
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
            let seq = f.engine.current().unwrap().seq;
            timeout(
                T,
                f.engine
                    .clone()
                    .forward(rx, mpsc::unbounded_channel().1, seq),
            )
            .await
            .unwrap();
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
        async fn a_stopped_external_session_is_not_offered_while_a_new_start_runs() {
            let external = || StartParams {
                target: EXTERNAL_TARGET_ID.into(),
                ..params()
            };
            let f = Fixture::new().await;
            f.engine.start(external(), None).await.unwrap();
            f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
            assert!(f.engine.external().is_some());
            // Without the forwarder nothing reacts to the stop, as when its events lag behind a new start.
            f.engine.current().unwrap().forwarder.abort();
            assert!(
                f.mock
                    .voice_state_from_other_session(MOCK_GUILD_ID, Some(MOCK_VOICE_CHANNEL_ID))
            );
            f.wait_state(|s| matches!(s, SessionState::Stopped { .. }))
                .await;
            let (start, gate) = f.start_in_flight_with(external()).await;
            assert!(
                f.engine.external().is_none(),
                "a producer could attach to the stopped run's source"
            );
            drop(gate);
            assert!(start.await.unwrap().is_ok());
            f.engine.shutdown().await;
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
            account: None,
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
        engine
            .clone()
            .forward(rx, mpsc::unbounded_channel().1, 0)
            .await;
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
