use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dstreamy_core::{MetricsSampler, PipelineStats, StreamHealth, StreamMetrics};
use dstreamy_media::{CaptureConfig, CaptureSource};
use rand::Rng;
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval, sleep_until, timeout};
use tracing::{debug, info, warn};

use crate::DiscordClient;
use crate::gateway::{GatewayEvent, VoiceState};
use crate::stream::pump::{MediaPump, TASK_JOIN_TIMEOUT};
use crate::stream::{
    Events, StreamError, StreamOptions, StreamStats, initial_attributes, join_voice, open_stream,
};
use crate::transport::{MediaConnection, VideoAttributes, VoiceEvent};

const EVENT_CAPACITY: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    Connecting,
    Streaming,
    /// `attempt` counts from 1 and resets once streaming again.
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
    GaveUp {
        attempts: u32,
    },
    SourceFailed {
        message: String,
    },
    /// Another device of this account joined voice; Discord allows one voice connection per user.
    VoiceTakenOver,
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Requested => f.write_str("requested"),
            Self::CaptureEnded => f.write_str("the capture source ended"),
            Self::GatewayClosed => f.write_str("the gateway connection closed"),
            Self::GaveUp { attempts } => write!(f, "gave up after {attempts} reconnect attempts"),
            Self::SourceFailed { message } => write!(f, "the capture source failed: {message}"),
            Self::VoiceTakenOver => f.write_str("another device of this account joined voice"),
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

/// Exponential backoff with 50–100 % jitter; the defaults follow `src/core/stream.ts`.
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
        let exp = self
            .base
            .saturating_mul(1 << attempt.saturating_sub(1).min(16));
        exp.min(self.max)
            .mul_f64(rand::rng().random_range(0.5..=1.0))
    }
}

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub stream: StreamOptions,
    pub backoff: Backoff,
    pub health_interval: Duration,
    /// Intervals kept for the session-average scores; the app keeps 60.
    pub health_history: usize,
}

impl SessionOptions {
    pub fn new(stream: StreamOptions) -> Self {
        Self {
            stream,
            backoff: Backoff::default(),
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
}

/// Supervises one Go Live stream; the client's `Streamer` holds one stream, so one `Session` per client.
pub struct Session {
    commands: mpsc::UnboundedSender<Command>,
    state: watch::Receiver<SessionState>,
    events: broadcast::Sender<SessionEvent>,
    task: JoinHandle<()>,
}

impl Session {
    pub fn new(client: Arc<DiscordClient>) -> Self {
        let (commands, commands_rx) = mpsc::unbounded_channel();
        let (state_tx, state) = watch::channel(SessionState::Idle);
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let actor = Actor {
            client,
            commands: commands_rx,
            state: state_tx,
            events: events.clone(),
            retained: None,
        };
        let task = tokio::spawn(actor.run());
        Self {
            commands,
            state,
            events,
            task,
        }
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
    retained: Option<Box<dyn CaptureSource>>,
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
                    if matches!(*self.state.borrow(), SessionState::Stopped { .. }) {
                        self.retained = None;
                    }
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
            }
        }
    }

    fn set_state(&self, state: SessionState) {
        if *self.state.borrow() == state {
            return;
        }
        info!(?state, "session state");
        self.state.send_replace(state.clone());
        let _ = self.events.send(SessionEvent::State { state });
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

enum Phase {
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
    /// The pump runs an older capture config and restarts on the next connection.
    reconfigure_pending: bool,
    sampler: MetricsSampler,
    history: Vec<StreamMetrics>,
    /// Until the first join completes, voice states may answer the previous run's ops; only the latest is kept.
    joined: bool,
    unacked: Option<VoiceState>,
}

impl Run {
    fn new(
        actor: &Actor,
        guild_id: Option<String>,
        channel_id: String,
        options: SessionOptions,
    ) -> Self {
        Self {
            client: actor.client.clone(),
            guild_id,
            channel_id,
            attrs: initial_attributes(&options.stream),
            options,
            gateway: actor.client.subscribe_unbounded(),
            voice: None,
            stream: None,
            pump: None,
            reconfigure_pending: false,
            sampler: MetricsSampler::new(),
            history: Vec::new(),
            joined: false,
            unacked: None,
        }
    }

    async fn drive(mut self, actor: &mut Actor, source: Box<dyn CaptureSource>) -> Ended {
        let mut source = Some(source);
        actor.set_state(SessionState::Connecting);
        let mut phase = Phase::Connecting {
            redo: Redo::Voice,
            attempt: 0,
            fut: self.connect(Redo::Voice),
        };
        let mut health = interval(self.options.health_interval);
        health.set_missed_tick_behavior(MissedTickBehavior::Delay);
        health.reset();

        let (reason, stop_reply, exit) = loop {
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
                            self.reconfigure_pending = self.pump.is_some();
                            let _ = reply.send(Ok(()));
                            continue;
                        };
                        match self.restart_pump(conn, true).await {
                            Ok(()) => {
                                let _ = reply.send(Ok(()));
                            }
                            Err((src, e)) => {
                                source = src;
                                let message = e.to_string();
                                actor.error(format!("capture source failed to restart: {message}"), true);
                                let _ = reply.send(Err(SessionError::Stream(e)));
                                break (StopReason::SourceFailed { message }, None, false);
                            }
                        }
                    }
                },
                step = advance(&mut phase) => match step {
                    Step::Due { redo, attempt } => {
                        phase = Phase::Connecting { redo, attempt, fut: self.connect(redo) };
                    }
                    Step::Done { attempt, result } => match result {
                        Ok(connected) => {
                            if let Some(voice) = connected.voice {
                                self.voice = Some(voice);
                            }
                            let Some(stream) = connected.stream else {
                                self.joined = true;
                                phase = Phase::Connecting { redo: Redo::Stream, attempt, fut: self.connect(Redo::Stream) };
                                if let Some(next) = self.unacked.take().and_then(|state| self.on_voice_state(actor, &phase, state)) {
                                    phase = next;
                                }
                                continue;
                            };
                            let conn = stream.0.clone();
                            self.stream = Some(stream);
                            if connected.attrs != self.attrs
                                && !self.reconfigure_pending
                                && let Err(e) = conn.set_video_attributes(Some(self.attrs))
                            {
                                actor.error(format!("announcing video attributes: {e}"), false);
                            }
                            match self.pump.as_ref() {
                                Some(_) if self.reconfigure_pending => {
                                    if let Err((src, e)) = self.restart_pump(conn, false).await {
                                        source = src;
                                        let message = e.to_string();
                                        actor.error(format!("capture source failed to restart: {message}"), true);
                                        break (StopReason::SourceFailed { message }, None, false);
                                    }
                                }
                                Some(pump) => {
                                    pump.attach(conn);
                                    pump.take_frame_intervals();
                                    self.sampler.reset();
                                }
                                None => {
                                    let Some(src) = source.take() else { unreachable!() };
                                    let started = timeout(
                                        self.options.stream.step_timeout,
                                        MediaPump::start(src, &self.options.stream, Some(conn)),
                                    );
                                    match started.await {
                                        Ok(Ok(pump)) => {
                                            self.pump = Some(pump);
                                            self.sampler.reset();
                                        }
                                        Ok(Err((src, e))) => {
                                            source = Some(src);
                                            actor.error(format!("capture source failed to start: {e}"), true);
                                            break (StopReason::SourceFailed { message: e.to_string() }, None, false);
                                        }
                                        // The source is lost with the cancelled start; stopping must not hang on it.
                                        Err(_) => {
                                            let message = "timed out starting the capture source".to_owned();
                                            actor.error(message.clone(), true);
                                            break (StopReason::SourceFailed { message }, None, false);
                                        }
                                    }
                                }
                            }
                            phase = Phase::Streaming;
                            actor.set_state(SessionState::Streaming);
                        }
                        Err(e) => {
                            actor.error(format!("connecting failed: {e}"), false);
                            let attempt = attempt + 1;
                            if attempt > self.options.backoff.max_attempts {
                                actor.error("giving up after repeated reconnect failures", true);
                                break (StopReason::GaveUp { attempts: attempt - 1 }, None, false);
                            }
                            // A failed stream-only retry may mean voice is gone too; the reference always rejoins.
                            phase = self.wait(actor, Redo::Voice, attempt);
                        }
                    },
                },
                event = next_event(&mut self.voice) => {
                    if let Some(next) = self.on_connection_event(actor, &phase, Redo::Voice, event) {
                        phase = next;
                    }
                }
                event = next_event(&mut self.stream) => {
                    if let Some(next) = self.on_connection_event(actor, &phase, Redo::Stream, event) {
                        phase = next;
                    }
                }
                event = self.gateway.recv() => match event {
                    Some(GatewayEvent::Closed(e)) => {
                        actor.error(format!("gateway closed: {e}"), true);
                        break (StopReason::GatewayClosed, None, false);
                    }
                    Some(GatewayEvent::Ready { .. }) => {
                        debug!("new gateway session; joining voice again");
                        phase = self.reconnect(actor, &phase, Redo::Voice);
                    }
                    Some(GatewayEvent::VoiceStateUpdate(state)) => {
                        if self.is_other_session(&state) {
                            if state.channel_id.is_some() {
                                actor.error("another device of this account joined voice", true);
                                break (StopReason::VoiceTakenOver, None, false);
                            }
                        } else if self.is_stale_session(&state) {
                            debug!("voice state of an earlier gateway session ignored");
                        } else if !self.joined {
                            self.unacked = Some(state);
                        } else if let Some(next) = self.on_voice_state(actor, &phase, state) {
                            phase = next;
                        }
                    }
                    Some(_) => {}
                    None => {
                        actor.error("gateway closed", true);
                        break (StopReason::GatewayClosed, None, false);
                    }
                },
                _ = health.tick() => {
                    if self.pump.as_ref().is_some_and(MediaPump::capture_ended) {
                        actor.error("capture source ended", true);
                        break (StopReason::CaptureEnded, None, false);
                    }
                    if matches!(phase, Phase::Streaming)
                        && let Some(snapshot) = self.sample()
                    {
                        actor.emit(SessionEvent::Health(snapshot));
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
        } else {
            self.client.stop_stream();
            self.client.leave_voice();
        }
        if let Some(pump) = self.pump.take() {
            let (stopped, result) = pump.stop().await;
            if let Err(e) = result {
                actor.error(format!("stopping the capture source: {e}"), false);
            }
            source = stopped.or(source);
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
        let (redo, attempt) = match phase {
            Phase::Streaming => (redo, 1),
            Phase::Connecting {
                redo: r, attempt, ..
            }
            | Phase::Waiting {
                redo: r, attempt, ..
            } => (merge(*r, redo), (*attempt).max(1)),
        };
        if let Some(pump) = &self.pump {
            pump.detach();
        }
        self.stream = None;
        self.client.stop_stream();
        if redo == Redo::Voice {
            self.voice = None;
        }
        self.wait(actor, redo, attempt)
    }

    fn on_connection_event(
        &mut self,
        actor: &Actor,
        phase: &Phase,
        kind: Redo,
        event: Option<VoiceEvent>,
    ) -> Option<Phase> {
        match event {
            Some(VoiceEvent::Resumed) => {
                if let Some(pump) = &self.pump {
                    pump.update_stats(|s| s.resumes += 1);
                }
                actor.emit(SessionEvent::Resumed);
                None
            }
            Some(VoiceEvent::Closed { code }) => {
                let name = if kind == Redo::Voice {
                    "voice"
                } else {
                    "stream"
                };
                actor.error(format!("{name} connection closed (code {code:?})"), false);
                Some(self.reconnect(actor, phase, kind))
            }
            Some(_) => None,
            None => Some(self.reconnect(actor, phase, kind)),
        }
    }

    fn on_voice_state(&mut self, actor: &Actor, phase: &Phase, state: VoiceState) -> Option<Phase> {
        if state.user_id != self.client.user().id {
            return None;
        }
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
                actor.emit(SessionEvent::ChannelMoved {
                    guild_id,
                    channel_id,
                });
                self.voice.as_ref()?;
                if let Some(pump) = &self.pump {
                    pump.detach();
                }
                self.stream = None;
                let attempt = match phase {
                    Phase::Streaming => 1,
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

    /// Restarts the source with the current capture config on `conn`, continuing the pump's timeline.
    async fn restart_pump(
        &mut self,
        conn: MediaConnection,
        same_connection: bool,
    ) -> Result<(), (Option<Box<dyn CaptureSource>>, StreamError)> {
        self.reconfigure_pending = false;
        let Some(pump) = self.pump.take() else {
            return Ok(());
        };
        let (source, mut resume, stopped) = pump.stop_resumable().await;
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
        if let Err(e) = conn.set_video_attributes(Some(self.attrs)) {
            warn!(%e, "announcing video attributes after a restart");
        }
        pump.request_keyframe();
        self.sampler.reset();
        self.history.clear();
        self.pump = Some(pump);
        Ok(())
    }

    fn is_other_session(&self, state: &VoiceState) -> bool {
        state.user_id == self.client.user().id
            && self.client.session_id().is_some()
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
        let stats = pump.stats();
        let jitter = pump.take_frame_intervals();
        let cumulative = PipelineStats {
            frame_count: stats.video_frames_sent + stats.video_frames_dropped,
            bytes_sent: stats.bytes_sent,
            audio_frames_encoded: stats.audio_frames_sent + stats.audio_frames_dropped,
            audio_frames_sent: stats.audio_frames_sent,
            audio_frames_dropped_not_ready: stats.audio_frames_dropped,
            audio_encode_errors: 0,
        };
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

fn merge(a: Redo, b: Redo) -> Redo {
    if a == Redo::Voice || b == Redo::Voice {
        Redo::Voice
    } else {
        Redo::Stream
    }
}

enum Step {
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
}
