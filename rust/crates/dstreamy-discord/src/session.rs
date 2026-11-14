use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dstreamy_core::{MetricsSampler, PipelineStats, StreamHealth, StreamMetrics};
use dstreamy_media::{CaptureConfig, CaptureSource, MediaError};
use rand::Rng;
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval, sleep_until, timeout};
use tracing::{debug, info, warn};

use crate::DiscordClient;
use crate::gateway::{GatewayError, GatewayEvent, VoiceState};
use crate::stream::pump::{MediaPump, TASK_JOIN_TIMEOUT};
use crate::stream::{
    E2ee, Events, StreamError, StreamOptions, StreamStats, e2ee_of, initial_attributes, join_voice,
    open_stream,
};
use crate::transport::{MediaConnection, VideoAttributes, VoiceEvent};
use crate::viewers::Viewers;

const EVENT_CAPACITY: usize = 256;
/// Voice close code for a member disconnected from the channel (kicked, moved out, channel deleted).
const CLOSE_DISCONNECTED: u16 = 4014;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    Connecting,
    Streaming,
    /// `attempt` counts from 1 and resets once a stream stays up for `SessionOptions::stable_streaming`.
    Reconnecting {
        attempt: u32,
    },
    Stopped {
        reason: StopReason,
    },
}

impl fmt::Display for SessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle => f.write_str("idle"),
            Self::Connecting => f.write_str("connecting"),
            Self::Streaming => f.write_str("streaming"),
            Self::Reconnecting { attempt } => write!(f, "reconnecting (attempt {attempt})"),
            Self::Stopped { reason } => write!(f, "stopped: {reason}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum StopReason {
    Requested,
    CaptureEnded,
    GatewayClosed,
    /// Discord rejected the token (gateway close 4004): the user has to log in again.
    InvalidToken,
    GaveUp {
        attempts: u32,
    },
    SourceFailed {
        message: String,
    },
    /// Another device of this account joined voice; Discord allows one voice connection per user.
    VoiceTakenOver,
    /// `require_e2ee` is set and Discord offered no end-to-end encryption for the stream.
    E2eeUnavailable,
    /// The gateway client reached `max_identifies_per_day`; logging in is allowed again after `retry_after_secs`.
    IdentifyLimit {
        retry_after_secs: u64,
    },
    /// The session task ended without a stop, for example after a panic.
    TaskEnded,
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Requested => f.write_str("requested"),
            Self::CaptureEnded => f.write_str("the capture source ended"),
            Self::GatewayClosed => f.write_str("the gateway connection closed"),
            Self::InvalidToken => f.write_str("Discord rejected the login token; log in again"),
            Self::GaveUp { attempts } => write!(f, "gave up after {attempts} reconnect attempts"),
            Self::SourceFailed { message } => write!(f, "the capture source failed: {message}"),
            Self::VoiceTakenOver => f.write_str("another device of this account joined voice"),
            Self::E2eeUnavailable => {
                f.write_str("Discord offers no end-to-end encryption for this stream")
            }
            Self::TaskEnded => f.write_str(
                "the stream task ended unexpectedly; start the stream again, and if it keeps happening check the log",
            ),
            Self::IdentifyLimit { retry_after_secs } => write!(
                f,
                "Discord kept ending the session; D-Streamy stopped logging in again to protect the account. Try again in {}.",
                crate::gateway::about(*retry_after_secs)
            ),
        }
    }
}

// Health is the most frequent event; boxing it would add an allocation per event for no saving.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SessionEvent {
    State {
        state: SessionState,
    },
    Health(HealthSnapshot),
    /// Followed by patching the channel and recreating Go Live, without rejoining.
    ChannelMoved {
        guild_id: Option<String>,
        channel_id: String,
    },
    /// A voice or stream WebSocket dropped and resumed in place.
    Resumed,
    /// Whether media leaves end-to-end encrypted; sent when a stream connection is up and whenever it changes.
    E2ee {
        state: E2ee,
        dave_protocol_version: u16,
    },
    Error {
        message: String,
        fatal: bool,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthSnapshot {
    pub metrics: StreamMetrics,
    pub health: StreamHealth,
    pub stats: StreamStats,
}

/// Exponential backoff with 50–100 % jitter.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub base: Duration,
    pub max: Duration,
    pub max_attempts: u32,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(1),
            max: Duration::from_secs(30),
            max_attempts: 10,
        }
    }
}

impl Backoff {
    /// Delay before `attempt` (from 1): half to all of `base * 2^(attempt-1)`, capped at `max`.
    pub fn delay(&self, attempt: u32) -> Duration {
        jittered_backoff(self.base, self.max, attempt)
    }
}

pub(crate) fn jittered_backoff(base: Duration, max: Duration, attempt: u32) -> Duration {
    let exp = base.saturating_mul(1 << attempt.saturating_sub(1).min(16));
    exp.min(max).mul_f64(rand::rng().random_range(0.5..=1.0))
}

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub stream: StreamOptions,
    pub backoff: Backoff,
    /// A stream that drops sooner than this after connecting keeps counting reconnect attempts.
    pub stable_streaming: Duration,
    pub health_interval: Duration,
    /// Intervals kept for the session-average scores; the app keeps 60.
    pub health_history: usize,
}

impl SessionOptions {
    pub fn new(stream: StreamOptions) -> Self {
        Self {
            stream,
            backoff: Backoff::default(),
            stable_streaming: Duration::from_secs(30),
            health_interval: Duration::from_secs(1),
            health_history: 60,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("a session is already running")]
    AlreadyRunning,
    #[error("no session is running")]
    NotRunning,
    #[error("the session task has ended")]
    Gone,
    #[error("changing the codec needs a new stream")]
    CodecChange,
    #[error(transparent)]
    Stream(#[from] StreamError),
}

/// A rejected `start`, with the source handed back when the session still had it.
#[derive(thiserror::Error)]
#[error("{error}")]
pub struct StartError {
    pub source: Option<Box<dyn CaptureSource>>,
    #[source]
    pub error: SessionError,
}

impl fmt::Debug for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StartError")
            .field("source", &self.source.is_some())
            .field("error", &self.error)
            .finish()
    }
}

type Reply<T> = oneshot::Sender<T>;
type Rejected = (Box<dyn CaptureSource>, SessionError);

enum Command {
    Start {
        guild_id: Option<String>,
        channel_id: String,
        source: Box<dyn CaptureSource>,
        options: Box<SessionOptions>,
        reply: Reply<Result<(), Rejected>>,
    },
    Stop {
        reply: Reply<Option<Box<dyn CaptureSource>>>,
    },
    UpdateVideo {
        attrs: VideoAttributes,
        reply: Reply<Result<(), SessionError>>,
    },
    Reconfigure {
        capture: Box<CaptureConfig>,
        reply: Reply<Result<(), SessionError>>,
    },
    RequestKeyframe,
    #[cfg(feature = "test-hooks")]
    Park {
        parked: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    },
}

/// Supervises one Go Live stream; the client's `Streamer` holds one stream, so one `Session` per client.
pub struct Session {
    commands: mpsc::UnboundedSender<Command>,
    state: watch::Receiver<SessionState>,
    events: broadcast::Sender<SessionEvent>,
    terminal_stats: watch::Receiver<Option<StreamStats>>,
    task: JoinHandle<()>,
}

impl Session {
    pub fn new(client: Arc<DiscordClient>) -> Self {
        let (commands, commands_rx) = mpsc::unbounded_channel();
        let (state_tx, state) = watch::channel(SessionState::Idle);
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let (terminal_tx, terminal_stats) = watch::channel(None);
        let actor = Actor {
            client,
            commands: commands_rx,
            state: state_tx,
            events: events.clone(),
            terminal_stats: terminal_tx,
            retained: None,
        };
        let task = tokio::spawn(actor.run());
        Self {
            commands,
            state,
            events,
            terminal_stats,
            task,
        }
    }

    /// The stopped run's final counters, a lower bound with `last_error` if teardown failed; `None` while a run is active.
    pub fn terminal_stats(&self) -> Option<StreamStats> {
        matches!(*self.state.borrow(), SessionState::Stopped { .. })
            .then(|| self.terminal_stats.borrow().clone())
            .flatten()
    }

    pub fn state(&self) -> watch::Receiver<SessionState> {
        self.state.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }

    /// Returns once accepted; progress shows in `state()`. A stopped session's kept source is dropped.
    pub async fn start(
        &self,
        guild_id: Option<String>,
        channel_id: String,
        source: Box<dyn CaptureSource>,
        options: SessionOptions,
    ) -> Result<(), StartError> {
        let (reply, rx) = oneshot::channel();
        let command = Command::Start {
            guild_id,
            channel_id,
            source,
            options: Box::new(options),
            reply,
        };
        if let Err(mpsc::error::SendError(Command::Start { source, .. })) =
            self.commands.send(command)
        {
            return Err(StartError {
                source: Some(source),
                error: SessionError::Gone,
            });
        }
        match rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err((source, error))) => Err(StartError {
                source: Some(source),
                error,
            }),
            Err(_) => Err(StartError {
                source: None,
                error: SessionError::Gone,
            }),
        }
    }

    /// Parks the session task until the returned sender is dropped; events queue meanwhile.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub async fn park_for_tests(&self) -> oneshot::Sender<()> {
        let (parked, is_parked) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let _ = self.commands.send(Command::Park {
            parked,
            release: released,
        });
        let _ = is_parked.await;
        release
    }

    /// Ends Go Live, leaves voice and hands the stopped source back; `None` when there is none.
    pub async fn stop(&self) -> Result<Option<Box<dyn CaptureSource>>, SessionError> {
        let (reply, rx) = oneshot::channel();
        self.commands
            .send(Command::Stop { reply })
            .map_err(|_| SessionError::Gone)?;
        rx.await.map_err(|_| SessionError::Gone)
    }

    /// Announces new video attributes (op 12); they also apply to every reconnect.
    pub async fn update_video(
        &self,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<(), SessionError> {
        let (reply, rx) = oneshot::channel();
        let attrs = VideoAttributes { width, height, fps };
        self.commands
            .send(Command::UpdateVideo { attrs, reply })
            .map_err(|_| SessionError::Gone)?;
        rx.await.map_err(|_| SessionError::Gone)?
    }

    pub fn request_keyframe(&self) {
        let _ = self.commands.send(Command::RequestKeyframe);
    }

    /// Restarts the source with `capture` live (replacing `update_video` attributes), or on the next connection.
    pub async fn reconfigure(&self, capture: CaptureConfig) -> Result<(), SessionError> {
        let (reply, rx) = oneshot::channel();
        let capture = Box::new(capture);
        self.commands
            .send(Command::Reconfigure { capture, reply })
            .map_err(|_| SessionError::Gone)?;
        rx.await.map_err(|_| SessionError::Gone)?
    }

    /// Stops any stream and waits for the supervisor task to end.
    pub async fn shutdown(self) -> Option<Box<dyn CaptureSource>> {
        let source = self.stop().await.ok().flatten();
        let Self {
            commands, mut task, ..
        } = self;
        drop(commands);
        if timeout(TASK_JOIN_TIMEOUT, &mut task).await.is_err() {
            task.abort();
            let _ = task.await;
        }
        source
    }
}

struct Actor {
    client: Arc<DiscordClient>,
    commands: mpsc::UnboundedReceiver<Command>,
    state: watch::Sender<SessionState>,
    events: broadcast::Sender<SessionEvent>,
    terminal_stats: watch::Sender<Option<StreamStats>>,
    retained: Option<Box<dyn CaptureSource>>,
}

fn announce(
    watch: &watch::Sender<SessionState>,
    events: &broadcast::Sender<SessionEvent>,
    state: SessionState,
) {
    // Sent under the watch's write lock: whoever sees the new state finds its event already queued.
    watch.send_modify(|current| {
        *current = state.clone();
        let _ = events.send(SessionEvent::State { state });
    });
}

impl Actor {
    async fn run(mut self) {
        while let Some(command) = self.commands.recv().await {
            match command {
                Command::Start {
                    guild_id,
                    channel_id,
                    source,
                    options,
                    reply,
                } => {
                    self.retained = None;
                    self.terminal_stats.send_replace(None);
                    let _ = reply.send(Ok(()));
                    let run = Run::new(&self, guild_id, channel_id, *options);
                    let ended = run.drive(&mut self, source).await;
                    self.retained = ended.source;
                    if let Some(reply) = ended.stop_reply
                        && let Err(source) = reply.send(self.retained.take())
                    {
                        self.retained = source;
                    }
                    if ended.exit {
                        return;
                    }
                }
                Command::Stop { reply } => {
                    if let Err(source) = reply.send(self.retained.take()) {
                        self.retained = source;
                    }
                }
                Command::UpdateVideo { reply, .. } | Command::Reconfigure { reply, .. } => {
                    let _ = reply.send(Err(SessionError::NotRunning));
                }
                Command::RequestKeyframe => {}
                #[cfg(feature = "test-hooks")]
                Command::Park { parked, release } => {
                    let _ = parked.send(());
                    let _ = release.await;
                }
            }
        }
    }

    fn set_state(&self, state: SessionState) {
        if *self.state.borrow() == state {
            return;
        }
        info!(?state, "session state");
        announce(&self.state, &self.events, state);
    }

    fn emit(&self, event: SessionEvent) {
        let _ = self.events.send(event);
    }

    fn error(&self, message: impl Into<String>, fatal: bool) {
        let message = message.into();
        warn!(%message, fatal, "session error");
        self.emit(SessionEvent::Error { message, fatal });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Redo {
    /// The voice connection is fine: recreate Go Live only.
    Stream,
    /// Join voice again (new gateway session, or the voice connection gave up).
    Voice,
}

/// `Redo::Voice` connects voice only; the stream follows as `Redo::Stream`.
struct Connected {
    voice: Option<(MediaConnection, Events)>,
    stream: Option<(MediaConnection, Events)>,
    attrs: VideoAttributes,
}

type Connecting = Pin<Box<dyn Future<Output = Result<Connected, StreamError>> + Send>>;
type Started =
    Result<Result<MediaPump, (Box<dyn CaptureSource>, StreamError)>, tokio::time::error::Elapsed>;
type Starting = Pin<Box<dyn Future<Output = Started> + Send>>;

enum Phase {
    /// The source starts before voice and Go Live, so a screen sharing dialog never leaves viewers an empty stream.
    Starting(Starting),
    Connecting {
        redo: Redo,
        attempt: u32,
        fut: Connecting,
    },
    Waiting {
        redo: Redo,
        attempt: u32,
        until: tokio::time::Instant,
    },
    Streaming,
}

enum Gateway {
    Handled,
    Phase(Phase),
    Stop(StopReason),
}

struct Ended {
    source: Option<Box<dyn CaptureSource>>,
    stop_reply: Option<Reply<Option<Box<dyn CaptureSource>>>>,
    exit: bool,
}

struct Run {
    client: Arc<DiscordClient>,
    guild_id: Option<String>,
    channel_id: String,
    options: SessionOptions,
    attrs: VideoAttributes,
    gateway: mpsc::UnboundedReceiver<GatewayEvent>,
    voice: Option<(MediaConnection, Events)>,
    stream: Option<(MediaConnection, Events)>,
    pump: Option<MediaPump>,
    /// The capture config changed while no stream connection was up; applied on the next one.
    reconfigure_pending: bool,
    /// The capture config the pump's source is running with.
    pump_capture: CaptureConfig,
    /// Set once this run sent its first join; before that it has no voice state to leave.
    join_sent: bool,
    sampler: MetricsSampler,
    history: Vec<StreamMetrics>,
    /// Set once the first join completes; before that, voice states may answer the previous run's ops.
    joined: bool,
    /// The latest voice state held back while not `joined`.
    unacked: Option<VoiceState>,
    viewers: Viewers,
    /// The E2EE state last reported, so the event goes out on changes only.
    e2ee: Option<(E2ee, u16)>,
    /// When the latest stream came up, and the attempt that brought it up.
    streaming_since: Option<(tokio::time::Instant, u32)>,
}

impl Run {
    fn new(
        actor: &Actor,
        guild_id: Option<String>,
        channel_id: String,
        options: SessionOptions,
    ) -> Self {
        let mut run = Self {
            client: actor.client.clone(),
            guild_id,
            channel_id,
            attrs: initial_attributes(&options.stream),
            pump_capture: options.stream.capture.clone(),
            options,
            gateway: actor.client.subscribe_unbounded(),
            voice: None,
            stream: None,
            pump: None,
            reconfigure_pending: false,
            join_sent: false,
            sampler: MetricsSampler::new(),
            history: Vec::new(),
            joined: false,
            unacked: None,
            viewers: Viewers::default(),
            e2ee: None,
            streaming_since: None,
        };
        run.viewers.expect(
            run.guild_id.as_deref(),
            &run.channel_id,
            &run.client.user().id,
        );
        run
    }

    async fn drive(mut self, actor: &mut Actor, starting: Box<dyn CaptureSource>) -> Ended {
        let mut source: Option<Box<dyn CaptureSource>> = None;
        actor.set_state(SessionState::Connecting);
        let options = self.options.stream.clone();
        let mut phase = Phase::Starting(Box::pin(async move {
            timeout(
                options.source_start_timeout,
                MediaPump::start(starting, &options, None),
            )
            .await
        }));
        let mut health = interval(self.options.health_interval);
        health.set_missed_tick_behavior(MissedTickBehavior::Delay);
        health.reset();

        let (mut reason, stop_reply, exit) = 'run: loop {
            tokio::select! {
                biased;
                command = actor.commands.recv() => match command {
                    None => break (StopReason::Requested, None, true),
                    Some(Command::Stop { reply }) => break (StopReason::Requested, Some(reply), false),
                    Some(Command::Start { source, reply, .. }) => {
                        let _ = reply.send(Err((source, SessionError::AlreadyRunning)));
                    }
                    Some(Command::UpdateVideo { attrs, reply }) => {
                        let _ = reply.send(self.update_video(attrs));
                    }
                    Some(Command::RequestKeyframe) => {
                        if let Some(pump) = &self.pump {
                            pump.request_keyframe();
                        }
                    }
                    #[cfg(feature = "test-hooks")]
                    Some(Command::Park { parked, release }) => {
                        let _ = parked.send(());
                        let _ = release.await;
                    }
                    Some(Command::Reconfigure { capture, reply }) => {
                        if capture.codec != self.options.stream.capture.codec {
                            let _ = reply.send(Err(SessionError::CodecChange));
                            continue;
                        }
                        self.options.stream.capture = *capture;
                        self.attrs = initial_attributes(&self.options.stream);
                        let live = match (&phase, &self.stream) {
                            (Phase::Streaming, Some((conn, _))) if self.pump.is_some() => Some(conn.clone()),
                            _ => None,
                        };
                        let Some(conn) = live else {
                            self.reconfigure_pending = self.pump.is_some() || matches!(phase, Phase::Starting(_));
                            let _ = reply.send(Ok(()));
                            continue;
                        };
                        match self.restart_pump(actor, conn, true).await {
                            Ok(()) => {
                                let _ = reply.send(Ok(()));
                            }
                            Err((src, e)) => {
                                source = src;
                                let reason = restart_failed(actor, &e);
                                let _ = reply.send(Err(SessionError::Stream(e)));
                                break (reason, None, false);
                            }
                        }
                    }
                },
                step = advance(&mut phase) => match step {
                    Step::Started(Ok(Ok(pump))) => {
                        pump.set_fps(self.attrs.fps);
                        self.pump = Some(pump);
                        self.sampler.reset();
                        self.join_sent = true;
                        phase = Phase::Connecting { redo: Redo::Voice, attempt: 0, fut: self.connect(Redo::Voice) };
                    }
                    Step::Started(Ok(Err((src, e)))) => {
                        source = Some(src);
                        actor.error(format!("capture source failed to start: {e}"), true);
                        break (StopReason::SourceFailed { message: e.to_string() }, None, false);
                    }
                    // The source is lost with the cancelled start; stopping must not hang on it.
                    Step::Started(Err(_)) => {
                        let message = format!(
                            "the capture source did not start within {} s; if a screen sharing dialog was open, choose a source sooner",
                            self.options.stream.source_start_timeout.as_secs()
                        );
                        actor.error(message.clone(), true);
                        break (StopReason::SourceFailed { message }, None, false);
                    }
                    Step::Due { redo, attempt } => {
                        phase = Phase::Connecting { redo, attempt, fut: self.connect(redo) };
                    }
                    Step::Done { attempt, result } => match result {
                        Ok(connected) => {
                            if let Some(voice) = connected.voice {
                                self.voice = Some(voice);
                            }
                            let Some(stream) = connected.stream else {
                                // Voice states queued while the join was in flight still go behind the fence.
                                while let Ok(event) = self.gateway.try_recv() {
                                    match self.on_gateway(actor, &phase, Some(event)) {
                                        Gateway::Handled => {}
                                        Gateway::Phase(next) => {
                                            phase = next;
                                            continue 'run;
                                        }
                                        Gateway::Stop(reason) => break 'run (reason, None, false),
                                    }
                                }
                                self.joined = true;
                                phase = Phase::Connecting { redo: Redo::Stream, attempt, fut: self.connect(Redo::Stream) };
                                let unacked = self.unacked.take().filter(|state| !self.is_stale_session(state));
                                if let Some(next) = unacked.and_then(|state| self.on_voice_state(actor, &phase, state)) {
                                    phase = next;
                                }
                                continue;
                            };
                            let conn = stream.0.clone();
                            self.stream = Some(stream);
                            let restart = self.reconfigure_pending
                                && self.pump_capture != self.options.stream.capture;
                            self.reconfigure_pending &= restart;
                            if connected.attrs != self.attrs
                                && !restart
                                && let Err(e) = conn.set_video_attributes(Some(self.attrs))
                            {
                                actor.error(format!("announcing video attributes: {e}"), false);
                            }
                            match self.pump.as_ref() {
                                Some(_) if restart => {
                                    if let Err((src, e)) = self.restart_pump(actor, conn, false).await {
                                        source = src;
                                        break (restart_failed(actor, &e), None, false);
                                    }
                                }
                                Some(pump) => {
                                    pump.attach(conn);
                                    pump.take_frame_intervals();
                                    self.sampler.reset();
                                }
                                None => unreachable!("the source starts before the first connection"),
                            }
                            phase = Phase::Streaming;
                            self.streaming_since = Some((tokio::time::Instant::now(), attempt));
                            actor.set_state(SessionState::Streaming);
                            self.report_e2ee(actor);
                        }
                        Err(StreamError::E2eeUnavailable(version)) => {
                            actor.error(
                                format!("Discord offers no end-to-end encryption for this stream (DAVE protocol {version}); refusing to stream unencrypted"),
                                true,
                            );
                            break (StopReason::E2eeUnavailable, None, false);
                        }
                        Err(e) => {
                            actor.error(format!("connecting failed: {e}"), false);
                            match self.retry(actor, attempt) {
                                Ok(next) => phase = next,
                                Err(reason) => break (reason, None, false),
                            }
                        }
                    },
                },
                event = next_event(&mut self.voice) => {
                    match self.on_connection_event(actor, &phase, Redo::Voice, event) {
                        Ok(Some(next)) => phase = next,
                        Ok(None) => {}
                        Err(reason) => break (reason, None, false),
                    }
                }
                event = next_event(&mut self.stream) => {
                    match self.on_connection_event(actor, &phase, Redo::Stream, event) {
                        Ok(Some(next)) => phase = next,
                        Ok(None) => {}
                        Err(reason) => break (reason, None, false),
                    }
                }
                event = self.gateway.recv() => match self.on_gateway(actor, &phase, event) {
                    Gateway::Handled => {}
                    Gateway::Phase(next) => phase = next,
                    Gateway::Stop(reason) => break (reason, None, false),
                },
                _ = health.tick() => {
                    if self.pump.as_ref().is_some_and(MediaPump::capture_ended) {
                        actor.error("capture source ended", true);
                        break (StopReason::CaptureEnded, None, false);
                    }
                    if matches!(phase, Phase::Streaming) {
                        if self.stream.as_ref().is_some_and(|(conn, _)| conn.e2ee_unavailable().is_some()) {
                            actor.error("end-to-end encryption became unavailable; stopped before sending unencrypted media", true);
                            break (StopReason::E2eeUnavailable, None, false);
                        }
                        self.report_e2ee(actor);
                        if let Some(snapshot) = self.sample() {
                            actor.emit(SessionEvent::Health(snapshot));
                        }
                    }
                }
            }
        };

        drop(phase);
        if reason == StopReason::VoiceTakenOver {
            // Op 4 or STREAM_DELETE would act on the voice state and stream key the other device now uses.
            let streamer = self.client.streamer();
            streamer.stream_connection().map(MediaConnection::stop);
            streamer.voice_connection().map(MediaConnection::stop);
        } else if self.join_sent {
            self.client.stop_stream();
            self.client.leave_voice();
        }
        if let Some(pump) = self.pump.take() {
            let final_stats = pump.stats_updater();
            let (stopped, result) = pump.stop().await;
            self.remember_terminal_stats(actor, final_stats.stats(), result.as_ref().err());
            match result {
                Err(e) if reason == StopReason::CaptureEnded => {
                    let message = match e {
                        StreamError::Media(MediaError::Backend(message)) => message,
                        e => e.to_string(),
                    };
                    reason = StopReason::SourceFailed { message };
                }
                Err(e) => actor.error(format!("stopping the capture source: {e}"), false),
                Ok(_) => {}
            }
            source = stopped.or(source);
        }
        self.viewers.reset();
        if let StopReason::SourceFailed { message } = &reason {
            actor.terminal_stats.send_modify(|stats| {
                if let Some(stats) = stats {
                    stats.last_error = Some(message.clone());
                }
            });
        }
        actor.set_state(SessionState::Stopped { reason });
        Ended {
            source,
            stop_reply,
            exit,
        }
    }

    fn connect(&self, redo: Redo) -> Connecting {
        let client = self.client.clone();
        let (guild_id, channel_id) = (self.guild_id.clone(), self.channel_id.clone());
        let options = self.options.stream.clone();
        let attrs = self.attrs;
        Box::pin(async move {
            Ok(match redo {
                Redo::Voice => Connected {
                    voice: Some(
                        join_voice(&client, guild_id, channel_id, options.step_timeout).await?,
                    ),
                    stream: None,
                    attrs,
                },
                Redo::Stream => Connected {
                    voice: None,
                    stream: Some(open_stream(&client, &options, attrs).await?),
                    attrs,
                },
            })
        })
    }

    fn wait(&self, actor: &Actor, redo: Redo, attempt: u32) -> Phase {
        actor.set_state(SessionState::Reconnecting { attempt });
        let delay = self.options.backoff.delay(attempt);
        debug!(?redo, attempt, ?delay, "reconnecting");
        Phase::Waiting {
            redo,
            attempt,
            until: tokio::time::Instant::now() + delay,
        }
    }

    /// Tears the stream (and for `Redo::Voice` the voice connection) down and schedules a retry.
    fn reconnect(&mut self, actor: &Actor, phase: &Phase, redo: Redo) -> Phase {
        self.reconnect_from(actor, phase, redo, 1)
    }

    /// Like `reconnect` for a dropped connection; a stream that dropped soon after connecting keeps counting.
    fn reconnect_dropped(
        &mut self,
        actor: &Actor,
        phase: &Phase,
        redo: Redo,
    ) -> Result<Phase, StopReason> {
        let attempt = match self.streaming_since {
            Some((since, attempt)) if since.elapsed() < self.options.stable_streaming => {
                attempt + 1
            }
            _ => 1,
        };
        if matches!(phase, Phase::Streaming) && attempt > self.options.backoff.max_attempts {
            actor.error(
                "giving up: the stream kept dropping right after connecting",
                true,
            );
            return Err(StopReason::GaveUp {
                attempts: attempt - 1,
            });
        }
        Ok(self.reconnect_from(actor, phase, redo, attempt))
    }

    fn reconnect_from(
        &mut self,
        actor: &Actor,
        phase: &Phase,
        redo: Redo,
        streaming_attempt: u32,
    ) -> Phase {
        let (redo, attempt) = match phase {
            Phase::Starting(_) | Phase::Streaming => (redo, streaming_attempt),
            Phase::Connecting {
                redo: r, attempt, ..
            }
            | Phase::Waiting {
                redo: r, attempt, ..
            } => (merge(*r, redo), (*attempt).max(1)),
        };
        self.detach_stream();
        self.client.stop_stream();
        if redo == Redo::Voice {
            self.drop_voice();
        }
        self.wait(actor, redo, attempt)
    }

    fn report_e2ee(&mut self, actor: &Actor) {
        let Some((conn, _)) = &self.stream else {
            return;
        };
        let current = e2ee_of(conn);
        if self.e2ee == Some(current) {
            return;
        }
        self.e2ee = Some(current);
        let (state, dave_protocol_version) = current;
        if state != E2ee::Encrypted {
            warn!(
                dave_protocol_version,
                "media is not end-to-end encrypted on this stream connection"
            );
        }
        actor.emit(SessionEvent::E2ee {
            state,
            dave_protocol_version,
        });
    }

    /// Discord may not answer a join for the channel the account is still in, so the rejoin must follow a leave.
    fn drop_voice(&mut self) {
        self.voice = None;
        self.client.leave_voice();
    }

    fn detach_stream(&mut self) {
        if let Some(pump) = &self.pump {
            pump.detach();
        }
        self.stream = None;
        self.e2ee = None;
    }

    fn on_connection_event(
        &mut self,
        actor: &Actor,
        phase: &Phase,
        kind: Redo,
        event: Option<VoiceEvent>,
    ) -> Result<Option<Phase>, StopReason> {
        let removed = matches!(
            event,
            Some(VoiceEvent::Closed {
                code: Some(CLOSE_DISCONNECTED)
            })
        );
        match event {
            Some(VoiceEvent::Resumed) => {
                if let Some(pump) = &self.pump {
                    pump.update_stats(|s| s.resumes += 1);
                }
                actor.emit(SessionEvent::Resumed);
                return Ok(None);
            }
            Some(VoiceEvent::Closed { code }) => {
                let name = connection_name(kind);
                actor.error(format!("{name} connection closed (code {code:?})"), false);
            }
            Some(VoiceEvent::MediaFailed { reason }) => {
                let name = connection_name(kind);
                actor.error(format!("{name} media path failed: {reason}"), false);
            }
            Some(_) => return Ok(None),
            None => {}
        }
        match phase {
            Phase::Connecting { attempt, .. } => self.retry(actor, *attempt).map(Some),
            // A removal rejoins without counting, as its null voice state does, whichever arrives first.
            _ if removed => {
                actor.error("removed from the voice channel", false);
                Ok(Some(self.reconnect(actor, phase, Redo::Voice)))
            }
            _ => self.reconnect_dropped(actor, phase, kind).map(Some),
        }
    }

    /// A failed connect attempt: tears everything down and waits to rejoin, or gives up past `max_attempts`.
    fn retry(&mut self, actor: &Actor, attempt: u32) -> Result<Phase, StopReason> {
        let attempt = attempt + 1;
        if attempt > self.options.backoff.max_attempts {
            actor.error("giving up after repeated reconnect failures", true);
            return Err(StopReason::GaveUp {
                attempts: attempt - 1,
            });
        }
        self.detach_stream();
        self.client.stop_stream();
        // A failed stream-only retry may mean voice is gone too, so always rejoin.
        self.drop_voice();
        Ok(self.wait(actor, Redo::Voice, attempt))
    }

    fn on_gateway(&mut self, actor: &Actor, phase: &Phase, event: Option<GatewayEvent>) -> Gateway {
        match event {
            Some(GatewayEvent::Closed(GatewayError::IdentifyLimit {
                retry_after_secs, ..
            })) => {
                let reason = StopReason::IdentifyLimit { retry_after_secs };
                actor.error(reason.to_string(), true);
                Gateway::Stop(reason)
            }
            Some(GatewayEvent::Closed(e)) if e.is_rejected_token() => {
                actor.error(format!("{}: {e}", StopReason::InvalidToken), true);
                Gateway::Stop(StopReason::InvalidToken)
            }
            Some(GatewayEvent::Closed(e)) => {
                actor.error(format!("gateway closed: {e}"), true);
                Gateway::Stop(StopReason::GatewayClosed)
            }
            Some(GatewayEvent::Ready { .. }) if matches!(phase, Phase::Starting(_)) => {
                Gateway::Handled
            }
            Some(GatewayEvent::Ready { .. }) => {
                debug!("new gateway session; joining voice again");
                Gateway::Phase(self.reconnect(actor, phase, Redo::Voice))
            }
            Some(GatewayEvent::VoiceStateUpdate(state)) => {
                if state.user_id != self.client.user().id {
                    return Gateway::Handled;
                }
                if self.is_other_session(&state) {
                    if matches!(phase, Phase::Starting(_)) {
                        debug!("another device's voice state before this run joined; ignored");
                    } else if state.channel_id.is_some() {
                        actor.error("another device of this account joined voice", true);
                        return Gateway::Stop(StopReason::VoiceTakenOver);
                    }
                } else if self.is_stale_session(&state) {
                    debug!("voice state of an earlier gateway session ignored");
                } else if !self.joined {
                    self.unacked = Some(state);
                } else if let Some(next) = self.on_voice_state(actor, phase, state) {
                    return Gateway::Phase(next);
                }
                Gateway::Handled
            }
            Some(GatewayEvent::Dispatch(d)) => {
                let user = &self.client.user().id;
                self.viewers
                    .apply(&d, self.guild_id.as_deref(), &self.channel_id, user);
                Gateway::Handled
            }
            Some(_) => Gateway::Handled,
            None => {
                actor.error("gateway closed", true);
                Gateway::Stop(StopReason::GatewayClosed)
            }
        }
    }

    fn on_voice_state(&mut self, actor: &Actor, phase: &Phase, state: VoiceState) -> Option<Phase> {
        match state.channel_id {
            Some(channel_id) if channel_id != self.channel_id => {
                let guild_id = state.guild_id.or_else(|| self.guild_id.clone());
                info!(%channel_id, "moved to another voice channel");
                // Ends Go Live under the old stream key before the channel is patched.
                self.client.stop_stream();
                self.client
                    .set_voice_channel(guild_id.clone(), channel_id.clone());
                self.guild_id = guild_id.clone();
                self.channel_id = channel_id.clone();
                self.viewers.reset();
                self.viewers.expect(
                    self.guild_id.as_deref(),
                    &self.channel_id,
                    &self.client.user().id,
                );
                actor.emit(SessionEvent::ChannelMoved {
                    guild_id,
                    channel_id,
                });
                self.voice.as_ref()?;
                self.detach_stream();
                let attempt = match phase {
                    Phase::Starting(_) | Phase::Streaming => 1,
                    Phase::Connecting { attempt, .. } | Phase::Waiting { attempt, .. } => {
                        (*attempt).max(1)
                    }
                };
                actor.set_state(SessionState::Reconnecting { attempt });
                Some(Phase::Connecting {
                    redo: Redo::Stream,
                    attempt,
                    fut: self.connect(Redo::Stream),
                })
            }
            // Old-session cleanup arrives as a null channel while reconnecting; ignore it there.
            None if matches!(phase, Phase::Streaming) => {
                actor.error("removed from the voice channel", false);
                Some(self.reconnect(actor, phase, Redo::Voice))
            }
            _ => None,
        }
    }

    fn remember_terminal_stats(
        &self,
        actor: &Actor,
        mut stats: StreamStats,
        error: Option<&StreamError>,
    ) {
        stats.viewers = None;
        (stats.e2ee, stats.dave_protocol_version) = self.e2ee.unwrap_or_default();
        if let Some(error) = error {
            stats.last_error = Some(error.to_string());
        }
        actor.terminal_stats.send_replace(Some(stats));
    }

    /// Restarts the source with the current capture config on `conn`, continuing the pump's timeline.
    async fn restart_pump(
        &mut self,
        actor: &Actor,
        conn: MediaConnection,
        same_connection: bool,
    ) -> Result<(), (Option<Box<dyn CaptureSource>>, StreamError)> {
        self.reconfigure_pending = false;
        let Some(pump) = self.pump.take() else {
            return Ok(());
        };
        let final_stats = pump.stats_updater();
        let (source, mut resume, stopped) = pump.stop_resumable().await;
        self.remember_terminal_stats(actor, final_stats.stats(), stopped.as_ref().err());
        let Some(source) = source else {
            return Err((None, StreamError::SourceLost));
        };
        if let Err(e) = stopped {
            return Err((Some(source), e));
        }
        resume.same_connection = same_connection;
        let start = MediaPump::start_resumed(
            source,
            &self.options.stream,
            Some(conn.clone()),
            Some(resume),
        );
        let pump = match timeout(self.options.stream.step_timeout, start).await {
            Ok(Ok(pump)) => pump,
            Ok(Err((source, e))) => return Err((Some(source), e)),
            // The source is lost with the cancelled start.
            Err(_) => return Err((None, StreamError::Timeout("restarting the capture source"))),
        };
        self.pump_capture = self.options.stream.capture.clone();
        if let Err(e) = conn.set_video_attributes(Some(self.attrs)) {
            warn!(%e, "announcing video attributes after a restart");
        }
        pump.request_keyframe();
        self.sampler.reset();
        self.history.clear();
        self.pump = Some(pump);
        actor.terminal_stats.send_replace(None);
        Ok(())
    }

    fn is_other_session(&self, state: &VoiceState) -> bool {
        self.client.session_id().is_some()
            && state
                .session_id
                .as_deref()
                .is_some_and(|id| !self.client.is_own_session(id))
    }

    fn is_stale_session(&self, state: &VoiceState) -> bool {
        state.session_id.as_deref().is_some_and(|id| {
            self.client.is_own_session(id) && self.client.session_id().as_deref() != Some(id)
        })
    }

    /// Succeeds once the attributes are stored; a connection that refuses them gets them on the reconnect.
    fn update_video(&mut self, attrs: VideoAttributes) -> Result<(), SessionError> {
        self.attrs = attrs;
        if let Some(pump) = &self.pump {
            pump.set_fps(attrs.fps);
        }
        if let Some((conn, _)) = &self.stream
            && let Err(e) = conn.set_video_attributes(Some(attrs))
        {
            warn!(%e, "announcing video attributes; the next connection will");
        }
        Ok(())
    }

    fn sample(&mut self) -> Option<HealthSnapshot> {
        let pump = self.pump.as_ref()?;
        let (e2ee, dave_protocol_version) = self.e2ee.unwrap_or_default();
        let stats = StreamStats {
            viewers: self.viewers.count(),
            e2ee,
            dave_protocol_version,
            ..pump.stats()
        };
        let jitter = pump.take_frame_intervals();
        let cumulative = pipeline_stats(&stats);
        let metrics = self.sampler.sample(Instant::now(), cumulative, jitter)?;
        self.history.push(metrics);
        let excess = self
            .history
            .len()
            .saturating_sub(self.options.health_history.max(1));
        self.history.drain(..excess);
        Some(HealthSnapshot {
            metrics,
            health: StreamHealth::compute(&self.history, self.attrs.fps),
            stats,
        })
    }
}

fn pipeline_stats(stats: &StreamStats) -> PipelineStats {
    PipelineStats {
        frame_count: stats
            .video_frames_sent
            .saturating_add(stats.video_frames_dropped)
            .saturating_add(stats.source_video_frames_dropped),
        bytes_sent: stats.bytes_sent,
        audio_frames_encoded: stats
            .audio_frames_sent
            .saturating_add(stats.audio_frames_dropped)
            .saturating_add(stats.source_audio_frames_dropped),
        audio_frames_sent: stats.audio_frames_sent,
        audio_frames_dropped_not_ready: stats.audio_frames_dropped,
        audio_encode_errors: 0,
    }
}

fn restart_failed(actor: &Actor, e: &StreamError) -> StopReason {
    let message = e.to_string();
    actor.error(format!("capture source failed to restart: {message}"), true);
    StopReason::SourceFailed { message }
}

fn connection_name(kind: Redo) -> &'static str {
    match kind {
        Redo::Voice => "voice",
        Redo::Stream => "stream",
    }
}

fn merge(a: Redo, b: Redo) -> Redo {
    if a == Redo::Voice || b == Redo::Voice {
        Redo::Voice
    } else {
        Redo::Stream
    }
}

enum Step {
    Started(Started),
    Due {
        redo: Redo,
        attempt: u32,
    },
    Done {
        attempt: u32,
        result: Result<Connected, StreamError>,
    },
}

async fn advance(phase: &mut Phase) -> Step {
    match phase {
        Phase::Starting(fut) => Step::Started(fut.await),
        Phase::Connecting { attempt, fut, .. } => {
            let result = fut.await;
            Step::Done {
                attempt: *attempt,
                result,
            }
        }
        Phase::Waiting {
            redo,
            attempt,
            until,
        } => {
            sleep_until(*until).await;
            Step::Due {
                redo: *redo,
                attempt: *attempt,
            }
        }
        Phase::Streaming => std::future::pending().await,
    }
}

async fn next_event(conn: &mut Option<(MediaConnection, Events)>) -> Option<VoiceEvent> {
    match conn {
        Some((_, events)) => events.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_losses_affect_video_health_without_becoming_network_not_ready_audio() {
        let mut sampler = MetricsSampler::new();
        let now = Instant::now();
        assert!(
            sampler
                .sample(
                    now,
                    pipeline_stats(&StreamStats::default()),
                    Default::default()
                )
                .is_none()
        );
        let stats = StreamStats {
            video_frames_sent: 30,
            video_frames_dropped: 2,
            audio_frames_sent: 48,
            audio_frames_dropped: 1,
            source_video_frames_dropped: 3,
            source_audio_frames_dropped: 4,
            ..Default::default()
        };
        let metrics = sampler
            .sample(
                now + Duration::from_secs(1),
                pipeline_stats(&stats),
                dstreamy_core::FrameIntervalSnapshot {
                    samples: 30,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(metrics.fps, 30);
        assert_eq!(metrics.v_drop, 5);
        assert_eq!(metrics.a_enc, 53);
        assert_eq!(metrics.a_sent, 48);
        assert_eq!(metrics.a_drop_not_ready, 1);
        assert_eq!(stats.audio_frames_dropped, 1);
    }

    #[test]
    fn backoff_doubles_with_jitter_up_to_the_cap() {
        let backoff = Backoff::default();
        for _ in 0..100 {
            let first = backoff.delay(1);
            assert!(first >= Duration::from_millis(500) && first <= Duration::from_secs(1));
            let fourth = backoff.delay(4);
            assert!(fourth >= Duration::from_secs(4) && fourth <= Duration::from_secs(8));
            let late = backoff.delay(40);
            assert!(late >= Duration::from_secs(15) && late <= Duration::from_secs(30));
        }
    }

    #[test]
    fn a_state_change_is_queued_as_an_event_before_the_state_watch_wakes_anyone() {
        use std::sync::Mutex;
        use std::task::{Context, Wake, Waker};

        struct Check {
            queued: Mutex<broadcast::Receiver<SessionEvent>>,
            event_queued: Mutex<Option<bool>>,
        }
        impl Wake for Check {
            fn wake(self: Arc<Self>) {
                let queued = self.queued.lock().unwrap().try_recv().is_ok();
                *self.event_queued.lock().unwrap() = Some(queued);
            }
        }

        let (watch_tx, mut watch_rx) = watch::channel(SessionState::Idle);
        let (events, queued) = broadcast::channel(4);
        let check = Arc::new(Check {
            queued: Mutex::new(queued),
            event_queued: Mutex::new(None),
        });
        let waker = Waker::from(check.clone());
        let mut changed = std::pin::pin!(watch_rx.changed());
        assert!(
            changed
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );

        announce(&watch_tx, &events, SessionState::Streaming);

        assert_eq!(*check.event_queued.lock().unwrap(), Some(true));
        assert_eq!(*watch_tx.borrow(), SessionState::Streaming);
    }
}
