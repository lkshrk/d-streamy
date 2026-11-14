//! The command engine behind `dstreamy daemon` and `dstreamy serve`: one Discord connection, at most one stream.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use dstreamy_core::{
    Codec, ConfigError, CropRect, Fps, HealthLevel, IdleSetting, Resolution, ServeConfig,
    StreamConfig, StreamMetrics, TOKEN_ENV, TokenSource, lock,
};
use dstreamy_discord::{
    DiscordClient, E2ee, GatewayError, HealthSnapshot, IdentifyLedger, Session, SessionError,
    SessionEvent, SessionState, StopReason, StreamStats,
};
use dstreamy_media::{
    CaptureConfig, CaptureSource, CloseReason, EXTERNAL_TARGET_ID, ExternalHandle, ExternalOptions,
    ExternalSource, IdleMode, InterruptReason, MAX_GRACE, OptionsError, ProducerInfo, SourceState,
    SourceStats,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{OwnedMutexGuard, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::capture::{self, PortalBook, PortalToken, TargetRow};
use crate::discord::{self, Persistence};
use crate::lock::{LockError, SessionLock};
use crate::stream::parse_codec;
use crate::{CliError, Ctx};

pub const PROTOCOL: u32 = 1;
/// Discord closes the gateway with 4004 when the token is not valid.
const AUTH_FAILED: u16 = 4004;
const FORWARDER_DRAIN: Duration = Duration::from_secs(2);
/// How long a command waits for a new Discord login before reporting that Discord is unreachable.
const CONNECT_BUDGET: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub include_platform: bool,
    /// Capacity of event subscriptions; lossless subscribers disconnect on overflow.
    pub event_capacity: usize,
    /// Accepts `start` with target `external`; protocol 1 never does.
    pub allow_external: bool,
    pub connect_budget: Duration,
    /// How long a new long-lived Discord login keeps retrying before its first READY.
    pub first_login_window: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            include_platform: true,
            event_capacity: 256,
            allow_external: false,
            connect_budget: CONNECT_BUDGET,
            first_login_window: discord::LONG_FIRST_LOGIN_RETRY,
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
    pub e2ee: E2ee,
    pub dave_protocol_version: Option<u16>,
    /// Times the stream went reconnecting after it was live.
    pub reconnect_episodes: u64,
    /// Voice or stream connection resumes, which need no reconnect.
    pub resumes: u64,
    /// The last interval's metrics; `None` until the first one.
    pub metrics: Option<StreamMetrics>,
    pub audio_level: Option<HealthLevel>,
    pub video_level: Option<HealthLevel>,
    /// Video frames dropped on the way to Discord.
    pub dropped_video: u64,
    pub source_dropped_video: u64,
    pub source_dropped_audio: u64,
    /// Intervals in which fewer than [`AUDIO_DROPOUT_BELOW`] Opus frames reached Discord.
    pub audio_dropouts: u64,
    /// Reconnects before the first `Streaming` show as reconnecting but are not counted.
    pub streamed: bool,
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
            SessionState::Streaming => Some(Self {
                reconnecting: false,
                streamed: true,
                ..current.unwrap_or_default()
            }),
            // Also before the first Streaming, so a first connect that keeps failing never looks healthy.
            SessionState::Reconnecting { .. } => {
                let h = current.unwrap_or_default();
                Some(Self {
                    reconnecting: true,
                    e2ee: E2ee::Unknown,
                    dave_protocol_version: None,
                    reconnect_episodes: h.reconnect_episodes
                        + u64::from(h.streamed && !h.reconnecting),
                    ..h
                })
            }
            SessionState::Connecting | SessionState::Idle | SessionState::Stopped { .. } => None,
        }
    }

    /// `source_interrupted` is true when the source produced nothing for part of the interval, which is not a dropout.
    fn on_health(
        current: Option<Self>,
        snapshot: &HealthSnapshot,
        source_interrupted: bool,
    ) -> Option<Self> {
        let current = current.unwrap_or_default();
        let dropout = !source_interrupted && snapshot.metrics.a_sent < AUDIO_DROPOUT_BELOW;
        Some(Self {
            audio_dropouts: current.audio_dropouts + u64::from(dropout),
            resumes: snapshot.stats.resumes,
            e2ee: snapshot.stats.e2ee,
            dave_protocol_version: (snapshot.stats.e2ee != E2ee::Unknown)
                .then_some(snapshot.stats.dave_protocol_version),
            metrics: Some(snapshot.metrics),
            audio_level: Some(snapshot.health.audio_level),
            video_level: Some(snapshot.health.video_level),
            dropped_video: snapshot.stats.video_frames_dropped,
            source_dropped_video: snapshot.stats.source_video_frames_dropped,
            source_dropped_audio: snapshot.stats.source_audio_frames_dropped,
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
                    InterruptReason::VideoStalled => "video_stalled",
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
    /// Why the session's last connection attempt failed: while it is live and not streaming, and after it gave up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_error: Option<String>,
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
            | CliError::UnknownAudioSource(_) => "invalid_params",
            CliError::AccountOfAnotherUser { .. } => "account_mismatch",
            CliError::NotLoggedIn => {
                return Self::new(
                    "not_logged_in",
                    "not logged in: use Log in, or run `dstreamy login`",
                );
            }
            CliError::Gateway(GatewayError::Rejected(AUTH_FAILED)) => {
                return Self::new("invalid_token", "Discord rejected the token");
            }
            CliError::Gateway(e @ GatewayError::IdentifyLimit { .. }) => {
                return Self::new("identify_limit", e.to_string());
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

fn token_digest(token: &str) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(token.as_bytes()).into()
}

pub fn invalid_params(e: impl std::fmt::Display) -> Failure {
    Failure::new("invalid_params", e.to_string())
}

/// What an attached producer is told when a control client's `stop` ends its session.
const STOPPED_BY_CLIENT: &str =
    "a D-Streamy client stopped it, such as another OBS or a script using the D-Streamy service";

/// Why a stopped session stopped, for an attached producer: the reason's wire name, its words and any retry delay.
fn stop_reason(conn: &Conn) -> Option<CloseReason> {
    match conn.state() {
        SessionState::Stopped { reason } => Some(stopped_because(&reason)),
        _ => None,
    }
}

fn stopped_because(reason: &StopReason) -> CloseReason {
    let wire = serde_json::to_value(reason).unwrap_or_default();
    CloseReason::Stopped {
        code: wire["reason"].as_str().unwrap_or("unknown").to_owned(),
        message: match reason {
            StopReason::Requested => STOPPED_BY_CLIENT.to_owned(),
            reason => reason.to_string(),
        },
        retry_after_secs: wire["retry_after_secs"].as_u64(),
    }
}

fn not_running() -> Failure {
    Failure::new("not_running", "no stream is running")
}

fn streaming_token_change() -> Failure {
    Failure::new("streaming", "stop the stream before changing the token")
}

fn queued_too_long() -> Failure {
    Failure::new(
        "busy",
        "another command is still connecting to Discord; check the network connection and try again",
    )
}

fn start_cancelled() -> Failure {
    Failure::new("cancelled", "Stop was pressed before the stream started")
}

fn shutting_down() -> Failure {
    Failure::new("shutting_down", "the service is shutting down")
}

async fn cancelled(abort: Option<&CancellationToken>) {
    match abort {
        Some(abort) => abort.cancelled().await,
        None => std::future::pending().await,
    }
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
    pub require_e2ee: Option<bool>,
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
    reported_end: AtomicBool,
}

/// A start's `Place`, and the connection whose forwarder may move it.
struct Placed {
    place: Place,
    conn: u64,
}

impl Conn {
    fn live(&self) -> bool {
        !self.ended() && live_state(&self.session.state().borrow())
    }

    /// The session task is gone, for example after a panic; its last state may still read live.
    fn ended(&self) -> bool {
        self.session.state().has_changed().is_err()
    }

    /// The session's state, reporting a dead task as stopped instead of its last value.
    fn state(&self) -> SessionState {
        if self.ended() {
            SessionState::Stopped {
                reason: StopReason::TaskEnded,
            }
        } else {
            self.session.state().borrow().clone()
        }
    }
}

/// Another connection replaced `seq`'s; false once none is left, so teardown still publishes.
fn superseded(current: &Option<Arc<Conn>>, seq: u64) -> bool {
    current.as_ref().is_some_and(|c| c.seq != seq)
}

fn is_current(current: &Option<Arc<Conn>>, seq: u64) -> bool {
    current.as_ref().is_some_and(|c| c.seq == seq)
}

fn live_state(state: &SessionState) -> bool {
    matches!(
        state,
        SessionState::Connecting | SessionState::Streaming | SessionState::Reconnecting { .. }
    )
}

struct Running {
    run: u64,
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
            run: self.run,
            target: self.target.clone(),
            stream,
            crop: p.crop.or(self.crop),
            audio_source: self.audio_source.clone(),
            external: self.external,
        };
        let capture = crate::session::capture_config(
            &next.target,
            &next.stream,
            next.crop,
            next.audio_source.clone(),
        );
        Ok((next, capture))
    }
}

#[derive(Default)]
struct StatsCache {
    stats: Option<StreamStats>,
    terminal_consumed: bool,
}

pub struct Engine {
    me: Weak<Engine>,
    ctx: Ctx,
    opts: Options,
    events: broadcast::Sender<EngineEvent>,
    lossless: Mutex<Vec<mpsc::Sender<EngineEvent>>>,
    conn: Mutex<Option<Arc<Conn>>>,
    /// Held while the connection is created, replaced or torn down, and while a stream starts.
    lifecycle: Arc<tokio::sync::Mutex<()>>,
    /// Tripped by `shutdown`, so a Discord login in progress gives up instead of holding `lifecycle`.
    closing: CancellationToken,
    /// Cancelled and replaced by each stop, so a start that has not begun its session gives up.
    start_abort: Mutex<CancellationToken>,
    /// Starts past their last abort check, which a stop must wait for and then stop.
    committed_starts: AtomicUsize,
    /// Held while an update applies, and by a start from before its session starts until `running` is set.
    updating: tokio::sync::Mutex<()>,
    run_seq: AtomicU64,
    last_stats: Mutex<StatsCache>,
    viewers: watch::Sender<Option<u32>>,
    health: watch::Sender<Option<LiveHealth>>,
    connect_error: Mutex<Option<String>>,
    running: Mutex<Option<Running>>,
    /// Set before the session starts, so a session that stops at once never leaves it behind.
    place: Mutex<Option<Placed>>,
    conn_seq: AtomicU64,
    /// Per token digest, so a replacement client keeps counting toward the daily IDENTIFY limit.
    identify_ledgers: Mutex<HashMap<[u8; 32], IdentifyLedger>>,
    login_gates: Mutex<HashMap<[u8; 32], Arc<tokio::sync::Mutex<()>>>>,
    published_session_events: AtomicU64,
    /// The token changed while a session streamed; its connection is replaced once the session is over.
    stale_token: AtomicBool,
    /// The account chosen for the next connection; `None` follows the default account.
    account: Mutex<Option<String>>,
    started_by: Mutex<Option<u64>>,
    external: Mutex<Option<External>>,
    portal: Arc<Mutex<Option<PortalBook>>>,
    session_lock: Mutex<Hold>,
    #[cfg(test)]
    test: TestGates,
}

#[cfg(test)]
#[derive(Default)]
struct TestGates {
    start_gate: tokio::sync::Mutex<()>,
    stop_gate: tokio::sync::Mutex<()>,
    start_commit_gate: tokio::sync::Mutex<()>,
    update_gate: tokio::sync::Mutex<()>,
    update_apply_gate: tokio::sync::Mutex<()>,
    update_commit_gate: tokio::sync::Mutex<()>,
    update_reconfigured: AtomicBool,
    /// Holding it parks forwarders at event handling and publication.
    forward_gate: tokio::sync::Mutex<()>,
    moves_applied: Mutex<Vec<Place>>,
    catch_ups_requested: AtomicUsize,
    stop_entered: AtomicBool,
    shutdown_entered: AtomicBool,
}

/// While `starting`, a `Stopped` state belongs to the previous run and must not release the lock.
#[derive(Default)]
struct Hold {
    lock: Option<SessionLock>,
    starting: bool,
}

impl Hold {
    fn release_if(&mut self, stopped: bool) -> bool {
        let release = stopped && !self.starting;
        if release {
            self.lock = None;
        }
        release
    }
}

/// Width, height and fps as op 12 announces them.
pub type VideoSize = (u32, u32, u32);

struct External {
    engine: Weak<Engine>,
    handle: ExternalHandle,
    announced: watch::Receiver<VideoSize>,
    watcher: JoinHandle<()>,
    announcer: JoinHandle<()>,
    /// Held by the watcher around each publish, so the final `Detached` follows one in flight.
    publishing: Arc<Mutex<()>>,
    /// Source-wide interruptions taint the audio interval until its next health sample.
    interrupted: Arc<AtomicBool>,
}

impl External {
    fn take_interrupted(&self) -> bool {
        let now = audio_interrupted(*self.handle.state().borrow());
        self.interrupted.swap(false, Ordering::AcqRel) || now
    }
}

fn audio_interrupted(state: SourceState) -> bool {
    matches!(
        state,
        SourceState::Detached
            | SourceState::Interrupted {
                reason: InterruptReason::Detached | InterruptReason::Stalled,
                ..
            }
    )
}

impl Drop for External {
    fn drop(&mut self) {
        self.watcher.abort();
        self.announcer.abort();
        // A stopped session keeps its source for a restart; no producer may feed it meanwhile.
        self.handle.end();
        if let Some(engine) = self.engine.upgrade() {
            let _after_watcher = lock(&self.publishing);
            engine.publish(EngineEvent::Source(SourceState::Detached));
        }
    }
}

async fn disk<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, CliError> + Send + 'static,
) -> Result<T, CliError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| CliError::Session(format!("account/config disk operation failed: {e}")))?
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
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
            closing: CancellationToken::new(),
            start_abort: Mutex::new(CancellationToken::new()),
            committed_starts: AtomicUsize::new(0),
            updating: tokio::sync::Mutex::new(()),
            run_seq: AtomicU64::new(0),
            last_stats: Mutex::new(StatsCache::default()),
            viewers: watch::Sender::new(None),
            health: watch::Sender::new(None),
            connect_error: Mutex::new(None),
            running: Mutex::new(None),
            place: Mutex::new(None),
            conn_seq: AtomicU64::new(0),
            identify_ledgers: Mutex::new(HashMap::new()),
            login_gates: Mutex::new(HashMap::new()),
            published_session_events: AtomicU64::new(0),
            stale_token: AtomicBool::new(false),
            account: Mutex::new(ctx.account.clone()),
            started_by: Mutex::new(None),
            external: Mutex::new(None),
            portal: Arc::new(Mutex::new(None)),
            session_lock: Mutex::new(Hold::default()),
            #[cfg(test)]
            test: TestGates::default(),
        })
    }

    /// May lag and skip events once `event_capacity` is exceeded.
    pub fn events(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }

    /// Every event in order; disconnects on overflow so callers must reconnect or shut down.
    pub fn subscribe_lossless(&self) -> mpsc::Receiver<EngineEvent> {
        let (tx, rx) = mpsc::channel(self.opts.event_capacity);
        lock(&self.lossless).push(tx);
        rx
    }

    #[cfg(test)]
    pub async fn handle(&self, command: Command) -> Result<Value, Failure> {
        let admitted = self.admit(&command);
        self.handle_admitted(command, None, admitted).await
    }

    /// Call when a command arrives, before it runs concurrently with others: a stop then cancels exactly the starts received before it.
    pub fn admit(&self, command: &Command) -> Admitted {
        match command {
            Command::Start(_) => Admitted(Some(lock(&self.start_abort).clone())),
            Command::Stop => {
                self.cancel_starts();
                Admitted(None)
            }
            _ => Admitted(None),
        }
    }

    fn cancel_starts(&self) {
        std::mem::take(&mut *lock(&self.start_abort)).cancel();
    }

    /// Runs a command already passed to [`Engine::admit`]; a successful `start` records `caller` as `started_by`.
    pub async fn handle_admitted(
        &self,
        command: Command,
        caller: Option<u64>,
        admitted: Admitted,
    ) -> Result<Value, Failure> {
        match command {
            Command::Hello => Ok(json!({
                "protocol": PROTOCOL,
                "version": env!("CARGO_PKG_VERSION"),
                "platform": std::env::consts::OS,
            })),
            Command::SetToken { token } => self.set_token(token).await,
            Command::LoggedIn => {
                let ctx = self.selected_ctx();
                let logged_in = match disk(move || crate::accounts::resolve(&ctx)).await {
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
            Command::Start(params) => {
                let abort = admitted
                    .0
                    .unwrap_or_else(|| lock(&self.start_abort).clone());
                self.start_admitted(params, caller, abort).await
            }
            Command::Update(params) => self.update(params).await,
            Command::Stop => self.stop_admitted().await,
        }
    }

    pub async fn status(&self) -> Status {
        let account = self.active_account().await;
        let account = match account {
            Some(name) => {
                let ctx = self.ctx.clone();
                disk(move || {
                    let user = ctx.paths.accounts().user(&name);
                    Ok(AccountView {
                        name,
                        username: user.as_ref().map(|u| u.username.clone()),
                        global_name: user.and_then(|u| u.global_name),
                    })
                })
                .await
                .ok()
            }
            None => None,
        };
        let session = self.current().map(|c| {
            let state = c.state();
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
            stats: lock(&self.last_stats).stats.clone(),
            account,
            connect_error: self
                .current()
                .filter(|c| {
                    c.live()
                        || matches!(
                            *c.session.state().borrow(),
                            SessionState::Stopped {
                                reason: StopReason::GaveUp { .. }
                            }
                        )
                })
                .and_then(|_| lock(&self.connect_error).clone()),
        }
    }

    /// The context with the account the next connection uses.
    fn selected_ctx(&self) -> Ctx {
        self.ctx.with_account(lock(&self.account).clone())
    }

    async fn selected_source(&self) -> Option<TokenSource> {
        let ctx = self.selected_ctx();
        disk(move || crate::accounts::resolve(&ctx))
            .await
            .ok()
            .map(|r| r.source)
    }

    /// The live session's account, else the account the next connection uses.
    async fn active_account(&self) -> Option<String> {
        let source = match self.current().filter(|c| c.live()) {
            Some(conn) => Some(conn.source.clone()),
            None => self.selected_source().await,
        };
        source.and_then(|s| s.account().map(str::to_owned))
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
                    "`dstreamy serve` takes its token from {TOKEN_ENV}, so it has no accounts: unset it where serve starts, then restart serve"
                ),
            ));
        }
        Ok(())
    }

    /// The saved accounts and the active one.
    pub async fn accounts(&self) -> Result<Value, Failure> {
        Self::accounts_available()?;
        let active = self.active_account().await;
        let ctx = self.ctx.clone();
        let list = disk(move || ctx.paths.accounts().list().map_err(CliError::from)).await?;
        let rows: Vec<Value> = list
            .into_iter()
            .map(|a| {
                let mut row = json!({
                    "active": active.as_deref() == Some(a.name.as_str()),
                    "username": a.user.as_ref().map(|u| &u.username),
                    "global_name": a.user.as_ref().and_then(|u| u.global_name.as_ref()),
                    "has_token": a.has_token,
                    "name": a.name,
                });
                if let Some(error) = a.token_error {
                    row["token_error"] = Value::String(error);
                }
                row
            })
            .collect();
        Ok(json!({ "active": active, "accounts": rows }))
    }

    /// Makes `name` the default account and the next connection's; refused while a session is live.
    pub async fn select_account(&self, name: &str) -> Result<Value, Failure> {
        let name = name.to_owned();
        self.under_lifecycle(move |engine, lifecycle| async move {
            engine.select_account_owned(&name, lifecycle).await
        })
        .await?
    }

    fn this(&self) -> Arc<Engine> {
        self.me
            .upgrade()
            .expect("an engine is only reachable through its Arc")
    }

    /// Runs `f` as its own task holding `lifecycle`.
    async fn under_lifecycle<T, F, Fut>(&self, f: F) -> Result<T, Failure>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Engine>, OwnedMutexGuard<()>) -> Fut,
        Fut: Future<Output = T> + Send + 'static,
    {
        let engine = self.this();
        // Once admitted, cancellation cannot release lifecycle before the disk commit and connection update.
        let lifecycle = self.lifecycle.clone().lock_owned().await;
        tokio::spawn(f(engine, lifecycle))
            .await
            .map_err(|e| Failure::new("internal", format!("account transaction failed: {e}")))
    }

    async fn select_account_owned(
        &self,
        name: &str,
        lifecycle: OwnedMutexGuard<()>,
    ) -> Result<Value, Failure> {
        Self::accounts_available()?;
        if self.is_live() {
            return Err(Failure::new(
                "streaming",
                "stop the stream before switching accounts",
            ));
        }
        let ctx = self.ctx.clone();
        let selected = name.to_owned();
        let has_token = disk(move || {
            let accounts = ctx.paths.accounts();
            let _store = crate::accounts::store_lock(&ctx)?;
            accounts.require(&selected)?;
            if accounts.load_token(&selected)?.is_none() {
                return Ok(false);
            }
            accounts.set_default(&selected)?;
            Ok(true)
        })
        .await?;
        if !has_token {
            return Err(Failure::new(
                "not_logged_in",
                format!(
                    "account {name} has no saved login: log in to {name} first (`dstreamy login --account {name}`), or remove it"
                ),
            ));
        }
        *lock(&self.account) = self.ctx.account.is_some().then(|| name.to_owned());
        if self
            .current()
            .is_some_and(|c| c.source.account() != Some(name))
        {
            self.disconnect_locked().await;
        }
        drop(lifecycle);
        self.accounts().await
    }

    /// Saves `name` as the default; serve then follows it, unless it was started with `--account`.
    async fn make_default(&self, name: &str) -> Result<(), Failure> {
        let ctx = self.ctx.clone();
        let selected = name.to_owned();
        disk(move || {
            ctx.paths
                .accounts()
                .set_default_locked(&selected, dstreamy_core::accounts::STORE_LOCK_WAIT)
                .map_err(CliError::from)
        })
        .await?;
        *lock(&self.account) = self.ctx.account.is_some().then(|| name.to_owned());
        Ok(())
    }

    /// Deletes a saved account; refused while it streams here or in another process.
    pub async fn logout_account(&self, name: &str) -> Result<Value, Failure> {
        let name = name.to_owned();
        self.under_lifecycle(move |engine, lifecycle| async move {
            engine.logout_account_owned(&name, lifecycle).await
        })
        .await?
    }

    async fn logout_account_owned(
        &self,
        name: &str,
        lifecycle: OwnedMutexGuard<()>,
    ) -> Result<Value, Failure> {
        Self::accounts_available()?;
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
        let ctx = self.ctx.clone();
        let removed = name.to_owned();
        let token = disk(move || {
            let accounts = ctx.paths.accounts();
            let _store = crate::accounts::store_lock(&ctx)?;
            accounts.require(&removed)?;
            let _held = crate::accounts::removal_lock(&ctx, &removed)?;
            let token = accounts.load_token(&removed).ok().flatten();
            accounts.remove(&removed)?;
            Ok(token)
        })
        .await?;
        if let Some(token) = token {
            lock(&self.identify_ledgers).remove(&token_digest(&token));
        }
        {
            let mut selected = lock(&self.account);
            if selected.as_deref() == Some(name) {
                *selected = None;
            }
        }
        if conn.is_some_and(|c| c.source == source) {
            self.disconnect_locked().await;
        }
        drop(lifecycle);
        tracing::info!("logged out account {name}");
        self.accounts().await
    }

    /// A login saved into `account` makes it the default; an older connection goes once idle.
    pub async fn logged_in_as(&self, account: Option<String>) {
        let _ = self
            .under_lifecycle(move |engine, lifecycle| async move {
                let _lifecycle = lifecycle;
                if let Some(name) = account
                    && let Err(e) = engine.make_default(&name).await
                {
                    tracing::warn!("making account {name} the default: {}", e.message);
                    *lock(&engine.account) = Some(name);
                }
                if engine.is_live() {
                    engine.stale_token.store(true, Ordering::Release);
                } else {
                    engine.stale_token.store(false, Ordering::Release);
                    engine.disconnect_locked().await;
                }
            })
            .await;
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
        if let Some(stats) = lock(&self.last_stats).stats.as_mut() {
            stats.viewers = None;
        }
        self.viewers.send_if_modified(|v| v.take().is_some());
    }

    /// The live external session's source; `None` unless live, so a producer never attaches to a stopped run.
    pub fn external(&self) -> Option<(ExternalHandle, watch::Receiver<VideoSize>)> {
        self.current().filter(|c| c.live())?;
        lock(&self.external)
            .as_ref()
            .map(|e| (e.handle.clone(), e.announced.clone()))
    }

    /// Announces a producer's video (op 12) for the live session.
    async fn apply_producer_video(&self, width: u32, height: u32, fps: u32) -> Result<(), Failure> {
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
        struct Held {
            _conn: Arc<Conn>,
            release: Duration,
        }
        impl Drop for Held {
            fn drop(&mut self) {
                std::thread::sleep(self.release);
            }
        }
        self.current().map(|conn| Held {
            _conn: conn,
            release,
        })
    }

    fn publish(&self, event: EngineEvent) {
        lock(&self.lossless).retain(|tx| tx.try_send(event.clone()).is_ok());
        let _ = self.events.send(event);
    }

    fn publish_session(&self, event: SessionEvent) {
        self.publish(EngineEvent::Session(event));
        self.published_session_events
            .fetch_add(1, Ordering::Release);
    }

    fn current(&self) -> Option<Arc<Conn>> {
        lock(&self.conn).clone()
    }

    pub fn is_live(&self) -> bool {
        self.current().is_some_and(|c| c.live())
    }

    fn identify_ledger(&self, token: &str) -> IdentifyLedger {
        lock(&self.identify_ledgers)
            .entry(token_digest(token))
            .or_insert_with(|| discord::identify_ledger(&self.ctx, token))
            .clone()
    }

    async fn reusable(&self, conn: &Conn) -> bool {
        conn.live()
            || (!conn.ended()
                && !conn.client.gateway().is_closed()
                && !self.stale_token.load(Ordering::Acquire)
                && self.selected_source().await.as_ref() == Some(&conn.source))
    }

    async fn conn(&self) -> Result<Arc<Conn>, Failure> {
        if let Some(conn) = self.current()
            && self.reusable(&conn).await
        {
            return Ok(conn);
        }
        let deadline = tokio::time::Instant::now() + self.opts.connect_budget;
        let _lifecycle = tokio::select! {
            biased;
            () = self.closing.cancelled() => return Err(shutting_down()),
            lifecycle = tokio::time::timeout_at(deadline, self.lifecycle.lock()) => {
                lifecycle.map_err(|_| queued_too_long())?
            }
        };
        self.ensure_conn(None, deadline).await
    }

    /// Bounds a login by `deadline`, shutdown or `abort`; one token's logins run in turn, so a set_token and a connection never IDENTIFY together.
    async fn login<T>(
        &self,
        token: &str,
        connect: impl Future<Output = Result<T, CliError>>,
        abort: Option<&CancellationToken>,
        deadline: tokio::time::Instant,
    ) -> Result<T, Failure> {
        let budget = self.opts.connect_budget;
        let gate = lock(&self.login_gates)
            .entry(token_digest(token))
            .or_default()
            .clone();
        let connect = async {
            let _one_at_a_time = gate.lock().await;
            connect.await
        };
        tokio::select! {
            biased;
            () = self.closing.cancelled() => Err(shutting_down()),
            () = cancelled(abort) => Err(start_cancelled()),
            connected = tokio::time::timeout_at(deadline, connect) => match connected {
                Ok(connected) => connected.map_err(Failure::from),
                Err(_) => Err(Failure::new(
                    "gateway",
                    format!(
                        "Discord did not answer within {} s: check the network connection, and any firewall, proxy or captive portal",
                        budget.as_secs()
                    ),
                )),
            },
        }
    }

    /// Needs `lifecycle` held; a new login must finish by `deadline`.
    async fn ensure_conn(
        &self,
        abort: Option<&CancellationToken>,
        deadline: tokio::time::Instant,
    ) -> Result<Arc<Conn>, Failure> {
        if let Some(conn) = self.current() {
            if self.reusable(&conn).await {
                return Ok(conn);
            }
            drop(conn);
            self.disconnect_locked().await;
        }
        let engine = self.this();
        let ctx = self.selected_ctx();
        let resolve_ctx = ctx.clone();
        let resolved = disk(move || crate::accounts::resolve(&resolve_ctx)).await?;
        let source = resolved.source.clone();
        let ledger = self.identify_ledger(&resolved.token);
        let token = resolved.token.clone();
        let client = Arc::new(
            self.login(
                &token,
                discord::connect_resolved_counted(
                    &ctx,
                    resolved,
                    Persistence::LongWithin(self.opts.first_login_window),
                    ledger,
                ),
                abort,
                deadline,
            )
            .await?,
        );
        let session = Session::new(client.clone());
        let seq = self.conn_seq.fetch_add(1, Ordering::Relaxed) + 1;
        // One request per `start` or `stop`, each awaited before the next; never more than a few queued.
        let (catch_up, catch_ups) = mpsc::unbounded_channel();
        let forwarder =
            tokio::spawn(engine.forward(session.subscribe(), catch_ups, session.state(), seq));
        let conn = Arc::new(Conn {
            source,
            client,
            session,
            forwarder,
            catch_up,
            seq,
            reported_end: AtomicBool::new(false),
        });
        *lock(&self.conn) = Some(conn.clone());
        self.stale_token.store(false, Ordering::Release);
        Ok(conn)
    }

    /// Answers each catch-up once every earlier session event is handled; `task` closes after the session's last event.
    async fn forward(
        self: Arc<Self>,
        mut events: broadcast::Receiver<SessionEvent>,
        mut catch_ups: mpsc::UnboundedReceiver<oneshot::Sender<()>>,
        mut task: watch::Receiver<SessionState>,
        conn: u64,
    ) {
        let mut waiting: Vec<oneshot::Sender<()>> = Vec::new();
        let mut last_state: Option<SessionState> = None;
        let mut resynced: Option<SessionState> = None;
        let mut lagged = false;
        let mut task_ended = false;
        loop {
            if events.is_empty() {
                if std::mem::take(&mut lagged) && self.resync(conn, &mut last_state).await {
                    // The actor may have queued this state's event just after the emptiness check.
                    resynced = last_state.clone();
                }
                if task_ended {
                    self.session_ended(conn);
                }
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
                changed = task.changed(), if !task_ended => {
                    task_ended = changed.is_err();
                    continue;
                }
            };
            #[cfg(test)]
            drop(self.test.forward_gate.lock().await);
            match received {
                Ok(event) => {
                    if superseded(&self.current(), conn) {
                        continue;
                    }
                    if !self.apply_event(conn, &event).await {
                        continue;
                    }
                    #[cfg(test)]
                    drop(self.test.forward_gate.lock().await);
                    let current = lock(&self.conn);
                    if !superseded(&current, conn) {
                        let duplicate = match &event {
                            SessionEvent::State { state } => {
                                last_state = Some(state.clone());
                                resynced.take().as_ref() == Some(state)
                            }
                            _ => false,
                        };
                        if !duplicate {
                            self.publish_session(event);
                        }
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "session events lagged");
                    self.release_if_stopped();
                    // The skipped events may hold the terminal state; the current one follows the retained backlog.
                    lagged = true;
                }
                Err(RecvError::Closed) => break,
            }
        }
    }

    /// Applies `event` to the engine's state; false when it must not be published.
    async fn apply_event(&self, conn: u64, event: &SessionEvent) -> bool {
        match event {
            SessionEvent::Health(h) => {
                let current = lock(&self.conn);
                let mut cache = lock(&self.last_stats);
                if !cache.terminal_consumed && is_current(&current, conn) {
                    cache.stats = Some(h.stats.clone());
                    self.viewers.send_if_modified(|v| {
                        std::mem::replace(v, h.stats.viewers) != h.stats.viewers
                    });
                    let interrupted = self.source_interrupted();
                    self.health.send_if_modified(|v| {
                        let next = LiveHealth::on_health(*v, h, interrupted);
                        std::mem::replace(v, next) != next
                    });
                }
            }
            SessionEvent::E2ee {
                state,
                dave_protocol_version,
            } => {
                let current = lock(&self.conn);
                if superseded(&current, conn) {
                    return false;
                }
                self.health.send_if_modified(|v| {
                    let next = Some(LiveHealth {
                        e2ee: *state,
                        dave_protocol_version: (*state != E2ee::Unknown)
                            .then_some(*dave_protocol_version),
                        ..v.unwrap_or_default()
                    });
                    std::mem::replace(v, next) != next
                });
            }
            SessionEvent::State { state } => {
                if !self.apply_state(conn, state) {
                    return false;
                }
                self.remember_portal_token().await;
                if !superseded(&self.current(), conn) {
                    self.release_if_stopped();
                }
            }
            SessionEvent::ChannelMoved {
                guild_id,
                channel_id,
            } => self.moved(conn, guild_id.as_deref(), channel_id),
            SessionEvent::Error {
                message,
                fatal: false,
            } => {
                let current = lock(&self.conn);
                if let Some(c) = current.as_ref().filter(|c| c.seq == conn)
                    && *c.session.state().borrow() != SessionState::Streaming
                {
                    *lock(&self.connect_error) = Some(message.clone());
                }
            }
            _ => {}
        }
        true
    }

    /// Publishes the current state after a lag unless it was the last one forwarded; true when it published.
    async fn resync(&self, conn: u64, last_state: &mut Option<SessionState>) -> bool {
        let Some(state) = self.current().filter(|c| c.seq == conn).map(|c| c.state()) else {
            return false;
        };
        if last_state.as_ref() == Some(&state) {
            return false;
        }
        if self.apply_state(conn, &state) {
            self.remember_portal_token().await;
        }
        self.release_if_stopped();
        let current = lock(&self.conn);
        if is_current(&current, conn) {
            *last_state = Some(state.clone());
            self.publish_session(SessionEvent::State { state });
            return true;
        }
        false
    }

    /// Caches the run's stats and health for `state`; false when `conn` is no longer the connection.
    fn apply_state(&self, conn: u64, state: &SessionState) -> bool {
        let current = lock(&self.conn);
        if superseded(&current, conn) {
            return false;
        }
        {
            let mut cache = lock(&self.last_stats);
            if !cache.terminal_consumed
                && let Some(stats) = current.as_ref().and_then(|c| c.session.terminal_stats())
            {
                cache.stats = Some(stats);
            }
        }
        self.health.send_if_modified(|v| {
            let next = LiveHealth::on_state(*v, state);
            std::mem::replace(v, next) != next
        });
        if *state != SessionState::Streaming {
            self.forget_viewers();
        }
        // A session that gave up keeps the cause until the next run.
        let gave_up = matches!(
            state,
            SessionState::Stopped {
                reason: StopReason::GaveUp { .. }
            }
        );
        if !gave_up
            && matches!(
                state,
                SessionState::Streaming | SessionState::Idle | SessionState::Stopped { .. }
            )
        {
            *lock(&self.connect_error) = None;
        }
        true
    }

    /// Reports and releases a live session whose task ended without a stop; idempotent.
    fn session_ended(&self, conn: u64) {
        let Some(current) = self.current().filter(|c| c.seq == conn && c.ended()) else {
            return;
        };
        if current.reported_end.swap(true, Ordering::AcqRel) {
            return;
        }
        let was_live = live_state(&current.session.state().borrow());
        drop(current);
        tracing::error!("the session task ended without a stop");
        *lock(&self.running) = None;
        let reason = StopReason::TaskEnded;
        if was_live && let Some(external) = lock(&self.external).as_ref() {
            external.handle.note_stop(stopped_because(&reason));
        }
        self.release_if_stopped();
        if !was_live {
            return;
        }
        let state = SessionState::Stopped { reason };
        if !self.apply_state(conn, &state) {
            return;
        }
        let current = lock(&self.conn);
        if is_current(&current, conn) {
            self.publish_session(SessionEvent::State { state });
        }
    }

    fn moved(&self, from: u64, guild_id: Option<&str>, channel_id: &str) {
        let Some(conn) = self.current().filter(|current| current.seq == from) else {
            return;
        };
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
        lock(&self.test.moves_applied).push(place.clone());
    }

    /// Waits, bounded, until `conn`'s forwarder has handled every event its session sent so far.
    async fn catch_up(&self, conn: &Conn) -> bool {
        let (caught_up, handled) = oneshot::channel();
        // A forwarder that is gone has nothing left to handle.
        if conn.catch_up.send(caught_up).is_err() {
            return true;
        }
        #[cfg(test)]
        self.test.catch_ups_requested.fetch_add(1, Ordering::SeqCst);
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
            .is_none_or(|c| crate::session::has_stopped(&c.session));
        if hold.release_if(stopped) {
            *lock(&self.started_by) = None;
            let external = lock(&self.external).take();
            if let Some(external) = external
                && let Some(reason) = self.current().and_then(|c| stop_reason(&c))
            {
                external.handle.note_stop(reason);
            }
            *lock(&self.place) = None;
        }
    }

    async fn remember_portal_token(&self) {
        let portal = self.portal.clone();
        let paths = self.ctx.paths.clone();
        if let Err(e) = disk(move || {
            if let Some(book) = lock(&portal).as_mut() {
                book.save(&paths)?;
            }
            Ok(())
        })
        .await
        {
            tracing::warn!("saving the portal restore token: {e}");
        }
    }

    /// Validates the token without holding `lifecycle`, so a slow login blocks no other command or shutdown.
    async fn set_token(&self, token: String) -> Result<Value, Failure> {
        let token = token.trim().to_owned();
        if token.is_empty() {
            return Err(CliError::EmptyToken.into());
        }
        if self.is_live() {
            return Err(streaming_token_change());
        }
        let ledger = self.identify_ledger(&token);
        let client = self
            .login(
                &token,
                discord::connect_counted(&self.ctx, token.clone(), Persistence::Brief, ledger),
                None,
                tokio::time::Instant::now() + self.opts.connect_budget,
            )
            .await?;
        let user = crate::accounts::account_user(client.user());
        client.close().await;
        self.under_lifecycle(move |engine, lifecycle| async move {
            engine.set_token_owned(token, user, lifecycle).await
        })
        .await?
    }

    async fn set_token_owned(
        &self,
        token: String,
        user: dstreamy_core::AccountUser,
        _lifecycle: OwnedMutexGuard<()>,
    ) -> Result<Value, Failure> {
        if self.is_live() {
            return Err(streaming_token_change());
        }
        let ctx = self.ctx.clone();
        let selected = lock(&self.account).clone();
        disk(move || {
            let accounts = ctx.paths.accounts();
            let _store = crate::accounts::store_lock(&ctx)?;
            let name = match selected {
                Some(name) => name,
                None => match accounts.default_name()? {
                    Some(name) => name,
                    None => accounts.name_for(&user)?,
                },
            };
            if let Some(owner) = accounts.user(&name).filter(|u| u.id != user.id) {
                return Err(CliError::AccountOfAnotherUser {
                    account: name,
                    signed_in: user.username,
                    owner: owner.username,
                });
            }
            accounts.save(&name, &token, Some(&user))?;
            Ok(())
        })
        .await?;
        self.disconnect_locked().await;
        Ok(Value::Null)
    }

    #[cfg(test)]
    async fn start(&self, p: StartParams, caller: Option<u64>) -> Result<Value, Failure> {
        let abort = lock(&self.start_abort).clone();
        self.start_admitted(p, caller, abort).await
    }

    fn start_settings(
        &self,
        p: &StartParams,
        config: &dstreamy_core::Config,
    ) -> Result<StartSettings, Failure> {
        let external = self.opts.allow_external && p.target == EXTERNAL_TARGET_ID;
        if external
            && (p.crop.is_some()
                || p.gain.is_some_and(|gain| gain != 1.0)
                || p.audio_source.is_some())
        {
            return Err(invalid_params(
                "external producers supply encoded media; crop, non-unity gain and audio_source are unsupported",
            ));
        }
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
            audio_gain: if external {
                1.0
            } else {
                p.gain.unwrap_or(config.stream.audio_gain)
            },
            require_e2ee: config.stream.require_e2ee || p.require_e2ee.unwrap_or(false),
        };
        settings.validate().map_err(invalid_params)?;
        if let Some(crop) = p.crop {
            crop.validate().map_err(invalid_params)?;
        }
        Ok(StartSettings {
            external,
            settings,
            external_options,
        })
    }

    async fn start_admitted(
        &self,
        p: StartParams,
        caller: Option<u64>,
        abort: CancellationToken,
    ) -> Result<Value, Failure> {
        let published_before = self.published_session_events.load(Ordering::Acquire);
        let config_file = self.ctx.paths.config_file.clone();
        let config =
            disk(move || dstreamy_core::Config::load(&config_file).map_err(CliError::from)).await?;
        let StartSettings {
            external,
            settings,
            external_options,
        } = self.start_settings(&p, &config)?;
        // Waiting behind another command's login counts toward this start's login budget.
        let deadline = tokio::time::Instant::now() + self.opts.connect_budget;
        let _lifecycle = tokio::select! {
            biased;
            () = self.closing.cancelled() => return Err(shutting_down()),
            () = abort.cancelled() => return Err(start_cancelled()),
            lifecycle = tokio::time::timeout_at(deadline, self.lifecycle.lock()) => {
                lifecycle.map_err(|_| queued_too_long())?
            }
        };
        let conn = self.ensure_conn(Some(&abort), deadline).await?;
        if conn.live() {
            return Err(CliError::AlreadyStreaming.into());
        }
        // Counted before the check, so a stop that sees no committed start has already cancelled this one.
        let _committed = Committed::new(&self.committed_starts);
        if abort.is_cancelled() {
            return Err(start_cancelled());
        }
        // A prior terminal event delivered during this request can cancel its waiting frontend.
        if !self.catch_up(&conn).await
            || self.published_session_events.load(Ordering::Acquire) != published_before
        {
            return Err(Failure::new(
                "busy",
                "previous stream events are still being delivered; wait for the stopped event and retry Start",
            ));
        }
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
        let audio_source = if external {
            None
        } else {
            p.audio_source.or(config.audio_source)
        };
        if let Some(id) = &audio_source
            && handle.is_none()
            && let Some(warning) = capture::verify_audio_source(&p.target, id, &*source).await?
        {
            tracing::warn!("{warning}");
        }
        let capture =
            crate::session::capture_config(&p.target, &settings, p.crop, audio_source.clone());
        // Recorded before the session starts: a source that fails at once stops it before start returns.
        let portal = self.portal.clone();
        disk(move || {
            *lock(&portal) = Some(PortalBook::new(restore_token, portal_token));
            Ok(())
        })
        .await?;
        let needs_lock = lock(&self.session_lock).lock.is_none();
        if needs_lock {
            let ctx = self.ctx.clone();
            let source = conn.source.clone();
            let held =
                disk(move || crate::accounts::session_lock(&ctx, &source).map_err(CliError::from))
                    .await?;
            lock(&self.session_lock).lock = Some(held);
        }
        lock(&self.session_lock).starting = true;
        *lock(&self.place) = Some(Placed {
            place,
            conn: conn.seq,
        });
        #[cfg(test)]
        drop(self.test.start_gate.lock().await);
        let _updating = self.updating.lock().await;
        *lock(&self.running) = None;
        *lock(&self.connect_error) = None;
        let started = crate::session::start(
            &conn.session,
            p.guild_id,
            p.channel_id,
            source,
            capture,
            settings.require_e2ee,
        )
        .await;
        {
            let mut hold = lock(&self.session_lock);
            hold.starting = false;
            hold.release_if(started.is_err() || crate::session::has_stopped(&conn.session));
        }
        if started.is_err() {
            *lock(&self.place) = None;
            self.remember_portal_token().await;
        }
        started?;
        #[cfg(test)]
        drop(self.test.start_commit_gate.lock().await);
        {
            let mut cache = lock(&self.last_stats);
            *cache = StatsCache {
                stats: conn.session.terminal_stats(),
                terminal_consumed: false,
            };
        }
        self.viewers.send_replace(None);
        *lock(&self.started_by) = caller;
        let announced = (
            settings.resolution.width(),
            settings.resolution.height(),
            settings.fps.get(),
        );
        *lock(&self.external) = handle.map(|handle| self.watch_source(handle, announced));
        self.release_if_stopped();
        *lock(&self.running) = Some(Running {
            run: self.run_seq.fetch_add(1, Ordering::Relaxed) + 1,
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
        let publishing = Arc::new(Mutex::new(()));
        let interrupted = Arc::new(AtomicBool::new(false));
        let (gate, flag) = (publishing.clone(), interrupted.clone());
        let watcher = tokio::spawn(async move {
            let mut was_interrupted = false;
            loop {
                {
                    let _ordered = lock(&gate);
                    let state = *states.borrow_and_update();
                    let interrupted = audio_interrupted(state);
                    // Entering and leaving both taint the interval, as it is partly without media.
                    if interrupted || was_interrupted {
                        flag.store(true, Ordering::Release);
                    }
                    was_interrupted = interrupted;
                    let Some(engine) = engine.upgrade() else {
                        return;
                    };
                    engine.publish(EngineEvent::Source(state));
                }
                if states.changed().await.is_err() {
                    return;
                }
            }
        });
        External {
            engine: self.me.clone(),
            handle,
            announced: announced_rx,
            watcher,
            announcer,
            publishing,
            interrupted,
        }
    }

    fn source_interrupted(&self) -> bool {
        lock(&self.external)
            .as_ref()
            .is_some_and(External::take_interrupted)
    }

    async fn update(&self, p: UpdateParams) -> Result<Value, Failure> {
        if let Some(crop) = p.crop {
            crop.validate().map_err(invalid_params)?;
        }
        let conn = self
            .current()
            .filter(|c| c.live())
            .ok_or_else(not_running)?;
        let run = lock(&self.running)
            .as_ref()
            .map(|r| r.run)
            .ok_or_else(not_running)?;
        #[cfg(test)]
        drop(self.test.update_gate.lock().await);
        let _updating = self.updating.lock().await;
        let (next, capture) = {
            let running = lock(&self.running);
            let running = running
                .as_ref()
                .filter(|r| r.run == run)
                .ok_or_else(not_running)?;
            if running.external {
                return Err(invalid_params(
                    "the producer owns the encoder settings of an external session",
                ));
            }
            running.updated(&p)?
        };
        #[cfg(test)]
        drop(self.test.update_apply_gate.lock().await);
        conn.session
            .reconfigure(capture)
            .await
            .map_err(session_failure)?;
        #[cfg(test)]
        self.test.update_reconfigured.store(true, Ordering::SeqCst);
        #[cfg(test)]
        drop(self.test.update_commit_gate.lock().await);
        match lock(&self.running).as_mut() {
            Some(running) if running.run == run => *running = next,
            _ => return Err(not_running()),
        }
        Ok(Value::Null)
    }

    #[cfg(test)]
    async fn stop(&self) -> Result<Value, Failure> {
        self.cancel_starts();
        self.stop_admitted().await
    }

    async fn stop_admitted(&self) -> Result<Value, Failure> {
        #[cfg(test)]
        self.test.stop_entered.store(true, Ordering::SeqCst);
        // Nothing to stop or wait for; `lifecycle` may be held by a Discord login for a while.
        if self.committed_starts.load(Ordering::SeqCst) == 0
            && self.current().is_none_or(|c| !c.live() && !c.ended())
        {
            return Err(not_running());
        }
        let _lifecycle = self.lifecycle.lock().await;
        if let Some(conn) = self.current().filter(|c| c.ended()) {
            self.catch_up(&conn).await;
            self.session_ended(conn.seq);
        }
        let conn = self
            .current()
            .filter(|c| c.live())
            .ok_or_else(not_running)?;
        #[cfg(test)]
        drop(self.test.stop_gate.lock().await);
        let stopped = match conn.session.stop().await {
            Err(SessionError::Gone) => {
                self.catch_up(&conn).await;
                self.session_ended(conn.seq);
                return Err(not_running());
            }
            stopped => stopped.map_err(session_failure)?,
        };
        // A run stopped while starting hands back no source, so its own final reason says whether a stop ended it.
        let requested = matches!(
            conn.state(),
            SessionState::Stopped {
                reason: StopReason::Requested
            }
        );
        if requested && let Some(external) = lock(&self.external).as_ref() {
            external
                .handle
                .note_stop(stopped_because(&StopReason::Requested));
        }
        let caught_up = self.catch_up(&conn).await;
        self.release_if_stopped();
        if !caught_up {
            return Err(Failure::new(
                "busy",
                "the stream has stopped but its terminal events are still being delivered; wait for the stopped event before starting again",
            ));
        }
        if stopped.is_none() && !requested {
            return Err(not_running());
        }
        self.forget_run();
        self.release_if_stopped();
        let cached = {
            let mut cache = lock(&self.last_stats);
            cache.terminal_consumed = true;
            cache.stats.take()
        };
        let stats = conn.session.terminal_stats().or(cached).unwrap_or_default();
        self.viewers.send_replace(None);
        self.health.send_replace(None);
        serde_json::to_value(stats).map_err(|e| Failure::new("internal", e.to_string()))
    }

    fn forget_run(&self) {
        *lock(&self.running) = None;
        *lock(&self.place) = None;
        *lock(&self.started_by) = None;
        *lock(&self.external) = None;
    }

    /// Stops the stream and disconnects; the session's last events are sent before it returns.
    pub async fn shutdown(&self) {
        #[cfg(test)]
        self.test.shutdown_entered.store(true, Ordering::SeqCst);
        self.closing.cancel();
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
                crate::session::teardown(session, client).await;
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
        self.forget_run();
        *lock(&self.session_lock) = Hold::default();
        self.remember_portal_token().await;
    }
}

struct StartSettings {
    external: bool,
    settings: StreamConfig,
    external_options: Option<ExternalOptions>,
}

/// What a command claimed when it arrived; see [`Engine::admit`].
pub struct Admitted(Option<CancellationToken>);

struct Committed<'a>(&'a AtomicUsize);

impl<'a> Committed<'a> {
    fn new(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count)
    }
}

impl Drop for Committed<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
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
mod tests;
