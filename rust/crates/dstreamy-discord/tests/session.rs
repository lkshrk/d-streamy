mod support;

use support::{T, fast_reconnects, stream_video, wait_state};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use dstreamy_core::StreamHealth;
use dstreamy_discord::transport::{MediaTimeouts, Streamer};
use dstreamy_discord::{
    Backoff, DiscordClient, E2ee, GatewayEvent, GatewayOptions, Session, SessionError,
    SessionEvent, SessionOptions, SessionState, StopReason, StreamError, StreamOptions,
};
use dstreamy_media::{
    CaptureConfig, CaptureSource, CaptureTarget, FRAME_CHANNEL_CAPACITY, FrameReceiver, MediaError,
    TestPatternSource, VideoCodec,
};
use dstreamy_mock_discord::opcodes::gateway as gw;
use dstreamy_mock_discord::opcodes::voice as vo;
use dstreamy_mock_discord::{
    MOCK_GUILD_ID, MOCK_OTHER_VOICE_CHANNEL_ID, MOCK_USER_ID, MOCK_VOICE_CHANNEL_ID, MockConfig,
    MockDiscord, VoiceKind, rtc,
};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

async fn client(mock: &MockDiscord) -> Arc<DiscordClient> {
    client_configured(mock, |_| {}).await
}

async fn client_configured(
    mock: &MockDiscord,
    configure: impl FnOnce(&mut Streamer),
) -> Arc<DiscordClient> {
    client_with(mock, |_| {}, configure).await
}

async fn client_with(
    mock: &MockDiscord,
    gateway: impl FnOnce(&mut GatewayOptions),
    configure: impl FnOnce(&mut Streamer),
) -> Arc<DiscordClient> {
    support::client_with(
        mock,
        |opts| {
            fast_reconnects(opts);
            gateway(opts);
        },
        |streamer| {
            streamer.set_media_timeouts(MediaTimeouts {
                disconnected_grace: Duration::from_secs(1),
                restart_timeout: Duration::from_secs(1),
            });
            configure(streamer);
        },
    )
    .await
}

fn options() -> SessionOptions {
    let capture = CaptureConfig {
        width: 320,
        height: 180,
        fps: 30,
        bitrate_bps: 400_000,
        codec: VideoCodec::H264,
        ..CaptureConfig::default()
    };
    let mut options = SessionOptions::new(StreamOptions::new(capture));
    options.backoff = Backoff {
        base: Duration::from_millis(20),
        max: Duration::from_millis(200),
        max_attempts: 5,
    };
    options
}

struct Counters {
    starts: AtomicUsize,
    configs: Mutex<Vec<CaptureConfig>>,
    keyframe_requests: AtomicUsize,
    /// `Behaviour::GateRestart` holds every start after the first until a permit is added.
    restart_gate: tokio::sync::Semaphore,
}

impl Default for Counters {
    fn default() -> Self {
        Self {
            starts: AtomicUsize::default(),
            configs: Mutex::default(),
            keyframe_requests: AtomicUsize::default(),
            restart_gate: tokio::sync::Semaphore::new(0),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Behaviour {
    Normal,
    FailStart,
    HangStart,
    /// Starts after this many ms, as a user choosing in the screen sharing dialog would.
    SlowStart(u64),
    FailRestart,
    GateRestart,
    StopDrops,
    StopError,
    HangStop,
    EndAfter(usize),
}

struct Counted {
    inner: TestPatternSource,
    counters: Arc<Counters>,
    behaviour: Behaviour,
    forward: Option<JoinHandle<()>>,
    drops: dstreamy_media::CaptureDrops,
}

impl Counted {
    fn boxed() -> (Box<dyn CaptureSource>, Arc<Counters>) {
        Self::with(Behaviour::Normal)
    }

    fn with(behaviour: Behaviour) -> (Box<dyn CaptureSource>, Arc<Counters>) {
        let counters = Arc::new(Counters::default());
        let source = Self {
            inner: TestPatternSource::new(),
            counters: counters.clone(),
            behaviour,
            forward: None,
            drops: dstreamy_media::CaptureDrops::default(),
        };
        (Box::new(source), counters)
    }
}

#[async_trait]
impl CaptureSource for Counted {
    async fn list_targets(&self) -> Result<Vec<CaptureTarget>, MediaError> {
        self.inner.list_targets().await
    }

    async fn start(&mut self, config: CaptureConfig) -> Result<FrameReceiver, MediaError> {
        self.counters.starts.fetch_add(1, Ordering::SeqCst);
        self.counters.configs.lock().unwrap().push(config.clone());
        let mut frames = match self.behaviour {
            Behaviour::FailStart => return Err(MediaError::AlreadyRunning),
            Behaviour::HangStart => std::future::pending().await,
            Behaviour::SlowStart(ms) => {
                sleep(Duration::from_millis(ms)).await;
                self.inner.start(config).await?
            }
            Behaviour::FailRestart if self.counters.starts.load(Ordering::SeqCst) > 1 => {
                return Err(MediaError::AlreadyRunning);
            }
            Behaviour::GateRestart if self.counters.starts.load(Ordering::SeqCst) > 1 => {
                self.counters.restart_gate.acquire().await.unwrap().forget();
                self.inner.start(config).await?
            }
            _ => self.inner.start(config).await?,
        };
        self.drops = if self.behaviour == Behaviour::HangStop {
            dstreamy_media::CaptureDrops {
                video_frames: 3,
                audio_frames: 5,
            }
        } else {
            dstreamy_media::CaptureDrops::default()
        };
        let Behaviour::EndAfter(n) = self.behaviour else {
            return Ok(frames);
        };
        let (tx, rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
        self.forward = Some(tokio::spawn(async move {
            for _ in 0..n {
                let Some(frame) = frames.recv().await else {
                    return;
                };
                if tx.send(frame).await.is_err() {
                    return;
                }
            }
        }));
        Ok(rx)
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        if let Some(forward) = self.forward.take() {
            forward.abort();
        }
        self.inner.stop().await?;
        if self.behaviour == Behaviour::HangStop {
            std::future::pending::<()>().await;
        }
        if matches!(self.behaviour, Behaviour::StopDrops | Behaviour::StopError) {
            self.drops = dstreamy_media::CaptureDrops {
                video_frames: 7,
                audio_frames: 11,
            };
        }
        if self.behaviour == Behaviour::StopError {
            return Err(MediaError::Backend("injected teardown failure".into()));
        }
        Ok(())
    }

    fn capture_drops(&self) -> dstreamy_media::CaptureDrops {
        self.drops
    }

    fn request_keyframe(&self) {
        self.counters
            .keyframe_requests
            .fetch_add(1, Ordering::SeqCst);
        self.inner.request_keyframe();
    }
}

struct Fixture {
    mock: MockDiscord,
    client: Arc<DiscordClient>,
    session: Session,
    state: watch::Receiver<SessionState>,
    events: broadcast::Receiver<SessionEvent>,
    counters: Arc<Counters>,
}

impl Fixture {
    async fn streaming() -> Self {
        Self::streaming_with(options()).await
    }

    async fn streaming_with(options: SessionOptions) -> Self {
        Self::streaming_source(options, Behaviour::Normal).await
    }

    async fn streaming_source(options: SessionOptions, behaviour: Behaviour) -> Self {
        Self::streaming_armed(options, behaviour, |_| {}).await
    }

    async fn streaming_armed(
        options: SessionOptions,
        behaviour: Behaviour,
        arm: impl FnOnce(&MockDiscord),
    ) -> Self {
        Self::streaming_configured(options, behaviour, arm, |_| {}).await
    }

    async fn streaming_configured(
        options: SessionOptions,
        behaviour: Behaviour,
        arm: impl FnOnce(&MockDiscord),
        configure: impl FnOnce(&mut Streamer),
    ) -> Self {
        Self::streaming_on(MockConfig::default(), options, behaviour, arm, configure).await
    }

    async fn streaming_on(
        config: MockConfig,
        options: SessionOptions,
        behaviour: Behaviour,
        arm: impl FnOnce(&MockDiscord),
        configure: impl FnOnce(&mut Streamer),
    ) -> Self {
        let mock = MockDiscord::start_with(config).await.unwrap();
        arm(&mock);
        let client = client_configured(&mock, configure).await;
        let session = Session::new(client.clone());
        let events = session.subscribe();
        let state = session.state();
        let (source, counters) = Counted::with(behaviour);
        session
            .start(
                Some(MOCK_GUILD_ID.into()),
                MOCK_VOICE_CHANNEL_ID.into(),
                source,
                options,
            )
            .await
            .unwrap_or_else(|e| panic!("start: {e}"));
        let mut fixture = Self {
            mock,
            client,
            session,
            state,
            events,
            counters,
        };
        fixture
            .wait_state_explained(|s| *s == SessionState::Streaming)
            .await;
        fixture.video_flowing().await;
        fixture
    }

    /// At least 10 more video frames reach the mock.
    async fn video_flowing(&self) {
        let seen = self.mock.recorded().video.len();
        self.mock
            .wait_for(T, |r| (r.video.len() >= seen + 10).then_some(()))
            .await
            .unwrap_or_else(|| panic!("video stalled at {seen} frames"));
    }

    fn gateway_ops(&self, op: u8) -> usize {
        self.mock
            .received_ops()
            .iter()
            .filter(|o| o.is(None, op))
            .count()
    }

    fn voice_joins(&self) -> Vec<String> {
        self.mock
            .received_ops()
            .iter()
            .filter(|o| o.is(None, gw::OP_VOICE_STATE_UPDATE))
            .filter_map(|o| o.json().unwrap()["channel_id"].as_str().map(str::to_owned))
            .collect()
    }

    fn starts(&self) -> usize {
        self.counters.starts.load(Ordering::SeqCst)
    }

    /// The states entered since the fixture started, without blocking.
    fn states_seen(&mut self) -> Vec<SessionState> {
        let mut states = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            if let SessionEvent::State { state: s } = event {
                states.push(s);
            }
        }
        states
    }

    /// `wait_state` that on a timeout also names what the session reported and what the mock received.
    async fn wait_state_explained(&mut self, f: impl Fn(&SessionState) -> bool) {
        let waited = timeout(T, self.state.wait_for(|s| f(s)))
            .await
            .map(|r| r.is_ok());
        if waited == Ok(true) {
            return;
        }
        let mut events = Vec::new();
        loop {
            match self.events.try_recv() {
                Ok(SessionEvent::Health(_)) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Ok(event) => events.push(format!("{event:?}")),
                Err(_) => break,
            }
        }
        panic!("state stuck: {}", self.context(&events));
    }

    /// The state, `events` and the last 40 non-heartbeat ops the mock received, for failure messages.
    fn context(&self, events: &[String]) -> String {
        let received = self.mock.received_ops();
        let start = received.first().map(|o| o.received_at);
        let ops: Vec<String> = received
            .iter()
            .filter(|o| !o.is(None, gw::OP_HEARTBEAT))
            .map(|o| {
                let at = start.map_or(0, |t| o.received_at.duration_since(t).as_millis());
                format!("{at} ms {:?} op {}", o.socket, o.op)
            })
            .collect();
        format!(
            "state {:?}; session events {events:?}; the last ops the mock received {:?}",
            *self.state.borrow(),
            &ops[ops.len().saturating_sub(40)..]
        )
    }

    /// The states entered before the first event matching `f`.
    async fn states_until(&mut self, f: impl Fn(&SessionEvent) -> bool) -> Vec<SessionState> {
        let mut states = Vec::new();
        let found = timeout(T, async {
            loop {
                match self.events.recv().await {
                    Ok(event) if f(&event) => return,
                    Ok(SessionEvent::State { state }) => states.push(state),
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(e) => panic!("session events: {e}"),
                }
            }
        })
        .await;
        found.unwrap_or_else(|_| panic!("no such session event: {}", self.context(&[])));
        states
    }

    async fn wait_event(&mut self, f: impl Fn(&SessionEvent) -> bool) -> SessionEvent {
        let mut skipped = Vec::new();
        let found = timeout(T, async {
            loop {
                match self.events.recv().await {
                    Ok(event) if f(&event) => return event,
                    Ok(SessionEvent::Health(_)) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Ok(event) => skipped.push(format!("{event:?}")),
                    Err(e) => panic!("session events: {e}"),
                }
            }
        })
        .await;
        found.unwrap_or_else(|_| panic!("no such session event: {}", self.context(&skipped)))
    }

    async fn finish(self) {
        let source = timeout(T, self.session.shutdown()).await.unwrap();
        assert!(source.is_some(), "source handed back");
        Arc::try_unwrap(self.client).ok().unwrap().close().await;
        self.mock.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn voice_ws_drop_resumes_in_place() {
    let mut f = Fixture::streaming().await;
    f.states_seen();
    assert!(f.mock.drop_voice(VoiceKind::Stream, None));
    f.mock
        .wait_for_op(Some(VoiceKind::Stream), vo::OP_RESUME, 0, T)
        .await
        .expect("stream RESUME");
    f.wait_event(|e| matches!(e, SessionEvent::Resumed)).await;
    f.video_flowing().await;

    assert_eq!(f.states_seen(), []);
    assert_eq!(*f.state.borrow(), SessionState::Streaming);
    assert_eq!(f.gateway_ops(gw::OP_STREAM_CREATE), 1);
    assert_eq!(f.starts(), 1);
    f.finish().await;
}

fn media_failure(e: &SessionEvent) -> bool {
    matches!(e, SessionEvent::Error { message, fatal: false } if message.contains("media path failed"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_peer_connection_whose_restart_goes_unanswered_recreates_go_live() {
    let mut f = Fixture::streaming().await;
    f.states_seen();
    f.client
        .streamer()
        .stream_connection()
        .expect("stream connection")
        .simulate_peer_failed();
    f.mock
        .wait_for_op(Some(VoiceKind::Stream), vo::OP_SELECT_PROTOCOL, 1, T)
        .await
        .expect("the restart's SELECT_PROTOCOL");
    f.wait_event(media_failure).await;
    wait_state(&mut f.state, |s| *s == SessionState::Streaming).await;
    f.video_flowing().await;

    assert!(
        f.states_seen()
            .contains(&SessionState::Reconnecting { attempt: 1 })
    );
    assert_eq!(f.gateway_ops(gw::OP_STREAM_CREATE), 2);
    assert_eq!(f.voice_joins(), [MOCK_VOICE_CHANNEL_ID]);
    assert_eq!(f.starts(), 1);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_connection_disconnected_past_the_grace_recreates_go_live() {
    let mut f = Fixture::streaming().await;
    f.states_seen();
    f.client
        .streamer()
        .stream_connection()
        .expect("stream connection")
        .simulate_peer_disconnected();
    f.wait_event(media_failure).await;
    wait_state(&mut f.state, |s| *s == SessionState::Streaming).await;
    f.video_flowing().await;

    assert_eq!(f.gateway_ops(gw::OP_STREAM_CREATE), 2);
    assert_eq!(f.voice_joins(), [MOCK_VOICE_CHANNEL_ID]);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dead_udp_path_under_a_live_voice_socket_is_detected_and_go_live_recreated() {
    let configure = |streamer: &mut Streamer| {
        let mut engine = rtc::loopback_settings(false);
        engine.set_ice_timeouts(
            Some(Duration::from_secs(1)),
            Some(Duration::from_secs(2)),
            Some(Duration::from_millis(200)),
        );
        streamer.set_setting_engine(engine);
    };
    let mut f =
        Fixture::streaming_configured(options(), Behaviour::Normal, |_| {}, configure).await;
    f.states_seen();
    assert!(f.mock.drop_media(VoiceKind::Stream).await);
    f.wait_event(media_failure).await;
    wait_state(&mut f.state, |s| *s == SessionState::Streaming).await;
    f.video_flowing().await;

    assert_eq!(f.gateway_ops(gw::OP_STREAM_CREATE), 2);
    assert_eq!(f.voice_joins(), [MOCK_VOICE_CHANNEL_ID]);
    assert_eq!(f.starts(), 1);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn lost_stream_connection_recreates_go_live_with_the_same_source() {
    let mut f = Fixture::streaming().await;
    assert!(
        f.mock
            .drop_voice(VoiceKind::Stream, Some(vo::CLOSE_SESSION_NO_LONGER_VALID))
    );
    wait_state(&mut f.state, |s| {
        matches!(s, SessionState::Reconnecting { .. })
    })
    .await;
    wait_state(&mut f.state, |s| *s == SessionState::Streaming).await;
    f.video_flowing().await;

    assert_eq!(f.gateway_ops(gw::OP_STREAM_CREATE), 2);
    assert_eq!(f.voice_joins(), [MOCK_VOICE_CHANNEL_ID]);
    assert_eq!(f.starts(), 1);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn lost_voice_connection_rejoins_with_the_same_source() {
    let mut f = Fixture::streaming().await;
    assert!(
        f.mock
            .drop_voice(VoiceKind::Voice, Some(vo::CLOSE_SESSION_NO_LONGER_VALID))
    );
    wait_state(&mut f.state, |s| {
        matches!(s, SessionState::Reconnecting { .. })
    })
    .await;
    wait_state(&mut f.state, |s| *s == SessionState::Streaming).await;
    f.video_flowing().await;

    assert_eq!(
        f.voice_joins(),
        [MOCK_VOICE_CHANNEL_ID, MOCK_VOICE_CHANNEL_ID]
    );
    assert_eq!(f.gateway_ops(gw::OP_STREAM_CREATE), 2);
    assert_eq!(f.starts(), 1);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_voice_connection_leaves_before_rejoining() {
    let mut options = options();
    options.stream.step_timeout = Duration::from_secs(1);
    let mut f = Fixture::streaming_with(options).await;
    f.mock.ignore_repeated_joins();
    assert!(
        f.mock
            .drop_voice(VoiceKind::Voice, Some(vo::CLOSE_SESSION_NO_LONGER_VALID))
    );
    wait_state(&mut f.state, |s| {
        matches!(s, SessionState::Reconnecting { .. })
    })
    .await;
    wait_state(&mut f.state, |s| *s == SessionState::Streaming).await;
    f.video_flowing().await;

    let channels: Vec<Option<String>> = f
        .mock
        .received_ops()
        .iter()
        .filter(|o| o.is(None, gw::OP_VOICE_STATE_UPDATE))
        .map(|o| o.json().unwrap()["channel_id"].as_str().map(str::to_owned))
        .collect();
    let rejoin = channels.len() - 1;
    assert_eq!(channels[rejoin], Some(MOCK_VOICE_CHANNEL_ID.to_owned()));
    assert!(
        channels[1..rejoin].contains(&None),
        "no leave before the rejoin: {channels:?}"
    );
    f.finish().await;
}

async fn wait_viewers(f: &mut Fixture, want: Option<u32>) {
    f.wait_event(|e| matches!(e, SessionEvent::Health(h) if h.stats.viewers == want))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn health_counts_viewers_as_they_join_and_leave() {
    let mut f =
        Fixture::streaming_armed(options(), Behaviour::Normal, MockDiscord::quiet_set_paused).await;
    wait_viewers(&mut f, Some(0)).await;
    assert!(
        f.mock
            .set_viewers(&["400000000000000001", "400000000000000002"])
    );
    wait_viewers(&mut f, Some(2)).await;
    assert!(f.mock.set_viewers(&["400000000000000002", MOCK_USER_ID]));
    wait_viewers(&mut f, Some(1)).await;
    assert!(f.mock.set_viewers(&[]));
    wait_viewers(&mut f, Some(0)).await;
    assert!(f.mock.set_viewers(&["400000000000000003"]));
    wait_viewers(&mut f, Some(1)).await;

    assert!(
        f.mock
            .drop_voice(VoiceKind::Stream, Some(vo::CLOSE_SESSION_NO_LONGER_VALID))
    );
    f.mock
        .wait_for_op(None, gw::OP_STREAM_CREATE, 1, T)
        .await
        .expect("second STREAM_CREATE");
    wait_state(&mut f.state, |s| *s == SessionState::Streaming).await;
    wait_viewers(&mut f, Some(0)).await;
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn channel_move_recreates_go_live_without_rejoining() {
    let mut f = Fixture::streaming().await;
    assert!(
        f.mock
            .move_to_channel(MOCK_GUILD_ID, MOCK_OTHER_VOICE_CHANNEL_ID)
    );
    let moved = f
        .wait_event(|e| matches!(e, SessionEvent::ChannelMoved { .. }))
        .await;
    let SessionEvent::ChannelMoved { channel_id, .. } = moved else {
        unreachable!()
    };
    assert_eq!(channel_id, MOCK_OTHER_VOICE_CHANNEL_ID);
    f.mock
        .wait_for_op(None, gw::OP_STREAM_CREATE, 1, T)
        .await
        .expect("second STREAM_CREATE");
    wait_state(&mut f.state, |s| *s == SessionState::Streaming).await;
    f.video_flowing().await;

    let ops = f.mock.received_ops();
    let creates: Vec<_> = ops
        .iter()
        .filter(|o| o.is(None, gw::OP_STREAM_CREATE))
        .map(|o| o.json().unwrap()["channel_id"].clone())
        .collect();
    assert_eq!(creates[1], MOCK_OTHER_VOICE_CHANNEL_ID);
    let deleted: Vec<_> = ops
        .iter()
        .filter(|o| o.is(None, gw::OP_STREAM_DELETE))
        .map(|o| o.json().unwrap()["stream_key"].clone())
        .collect();
    assert_eq!(deleted.len(), 1);
    assert!(deleted[0].as_str().unwrap().contains(MOCK_VOICE_CHANNEL_ID));
    assert_eq!(f.voice_joins(), [MOCK_VOICE_CHANNEL_ID]);
    assert!(
        f.mock
            .streams()
            .iter()
            .any(|s| s.stream_key.contains(MOCK_OTHER_VOICE_CHANNEL_ID))
    );
    assert_eq!(f.starts(), 1);
    f.finish().await;
}

/// Stops `f`'s session, arms the mock with `arm`, and streams again in `channel_id`.
async fn restart_in(f: &mut Fixture, channel_id: &str, arm: impl FnOnce(&MockDiscord)) {
    let source = timeout(T, f.session.stop())
        .await
        .unwrap()
        .unwrap()
        .expect("source handed back");
    arm(&f.mock);
    f.states_seen();
    f.session
        .start(
            Some(MOCK_GUILD_ID.into()),
            channel_id.into(),
            source,
            options(),
        )
        .await
        .unwrap_or_else(|e| panic!("restart: {e}"));
    f.wait_state_explained(|s| *s == SessionState::Streaming)
        .await;
    f.video_flowing().await;
}

fn events_seen(f: &mut Fixture) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    loop {
        match f.events.try_recv() {
            Ok(event) => events.push(event),
            Err(broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(_) => return events,
        }
    }
}

fn moves_seen(f: &mut Fixture) -> Vec<String> {
    events_seen(f)
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::ChannelMoved { channel_id, .. } => Some(channel_id),
            _ => None,
        })
        .collect()
}

/// Stops `f`'s session, arms the mock with `arm`, and starts again in `channel_id` without waiting.
async fn stop_and_start(
    f: &mut Fixture,
    channel_id: &str,
    options: SessionOptions,
    arm: impl FnOnce(&MockDiscord),
) {
    let source = timeout(T, f.session.stop())
        .await
        .unwrap()
        .unwrap()
        .expect("source handed back");
    arm(&f.mock);
    f.states_seen();
    f.session
        .start(
            Some(MOCK_GUILD_ID.into()),
            channel_id.into(),
            source,
            options,
        )
        .await
        .unwrap_or_else(|e| panic!("restart: {e}"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_kick_while_joining_recovers_on_the_step_timeout() {
    let mut f = Fixture::streaming().await;
    let mut options = options();
    options.stream.step_timeout = Duration::from_secs(2);
    stop_and_start(&mut f, MOCK_VOICE_CHANNEL_ID, options, |mock| {
        mock.kick_during_next_join();
    })
    .await;
    f.wait_state_explained(|s| *s == SessionState::Streaming)
        .await;
    f.video_flowing().await;
    let events = events_seen(&mut f);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::ChannelMoved { .. })),
        "{events:?}"
    );
    let errors: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Error { message, .. } => Some(message.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(errors, ["connecting failed: timed out joining voice"]);
    assert_eq!(
        f.voice_joins(),
        [
            MOCK_VOICE_CHANNEL_ID,
            MOCK_VOICE_CHANNEL_ID,
            MOCK_VOICE_CHANNEL_ID
        ]
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn voice_states_queued_when_the_join_completes_never_move_the_run() {
    let mut f = Fixture::streaming().await;
    stop_and_start(&mut f, MOCK_OTHER_VOICE_CHANNEL_ID, options(), |mock| {
        mock.defer_next_join();
        mock.late_voice_state_before_next_join(MOCK_GUILD_ID, Some(MOCK_VOICE_CHANNEL_ID));
    })
    .await;
    f.mock
        .wait_for_op(None, gw::OP_VOICE_STATE_UPDATE, 2, T)
        .await
        .expect("the restart's op 4");
    let release = f.session.park_for_tests().await;
    assert!(f.mock.answer_deferred_join());
    timeout(T, async {
        loop {
            let voice = f.client.streamer().voice_connection().cloned();
            if let Some(voice) = voice
                && voice.is_ready().await
            {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the join never completed");
    drop(release);
    f.wait_state_explained(|s| *s == SessionState::Streaming)
        .await;
    f.video_flowing().await;
    assert_eq!(moves_seen(&mut f), Vec::<String>::new());
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn voice_closing_during_every_stream_create_gives_up() {
    let mut f = Fixture::streaming().await;
    let max_attempts = options().backoff.max_attempts;
    stop_and_start(&mut f, MOCK_VOICE_CHANNEL_ID, options(), |mock| {
        mock.close_voice_on_stream_create();
    })
    .await;
    wait_state(&mut f.state, |s| {
        matches!(
            s,
            SessionState::Stopped {
                reason: StopReason::GaveUp { .. }
            }
        )
    })
    .await;
    assert_eq!(
        *f.state.borrow(),
        SessionState::Stopped {
            reason: StopReason::GaveUp {
                attempts: max_attempts
            }
        }
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_voice_resume_during_every_stream_create_gives_up() {
    let mut f = Fixture::streaming().await;
    let max_attempts = options().backoff.max_attempts;
    stop_and_start(&mut f, MOCK_VOICE_CHANNEL_ID, options(), |mock| {
        mock.reset_voice_on_stream_create();
    })
    .await;
    wait_state(&mut f.state, |s| {
        matches!(
            s,
            SessionState::Stopped {
                reason: StopReason::GaveUp { .. }
            }
        )
    })
    .await;
    assert_eq!(
        *f.state.borrow(),
        SessionState::Stopped {
            reason: StopReason::GaveUp {
                attempts: max_attempts
            }
        }
    );
    let resumes = f
        .mock
        .received_ops()
        .iter()
        .filter(|o| o.is(Some(VoiceKind::Voice), vo::OP_RESUME))
        .count();
    assert_eq!(resumes, max_attempts as usize + 1);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_stream_create_rejoins_once_without_a_stale_voice_close() {
    let mut f = Fixture::streaming().await;
    let mut options = options();
    options.stream.step_timeout = Duration::from_secs(2);
    stop_and_start(&mut f, MOCK_VOICE_CHANNEL_ID, options, |mock| {
        mock.ignore_stream_creates(1);
    })
    .await;
    f.wait_state_explained(|s| *s == SessionState::Streaming)
        .await;
    f.video_flowing().await;
    let closes: Vec<_> = events_seen(&mut f)
        .into_iter()
        .filter(|e| matches!(e, SessionEvent::Error { message, .. } if message.contains("connection closed")))
        .collect();
    assert!(closes.is_empty(), "{closes:?}");
    assert_eq!(
        f.voice_joins(),
        [
            MOCK_VOICE_CHANNEL_ID,
            MOCK_VOICE_CHANNEL_ID,
            MOCK_VOICE_CHANNEL_ID
        ]
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn late_voice_states_of_the_last_run_never_move_the_next_one() {
    let mut f = Fixture::streaming().await;
    restart_in(&mut f, MOCK_OTHER_VOICE_CHANNEL_ID, |mock| {
        mock.late_voice_state_before_next_join(MOCK_GUILD_ID, Some(MOCK_VOICE_CHANNEL_ID));
        mock.late_voice_state_before_next_join(MOCK_GUILD_ID, None);
    })
    .await;
    assert_eq!(moves_seen(&mut f), Vec::<String>::new());
    assert_eq!(
        f.voice_joins(),
        [MOCK_VOICE_CHANNEL_ID, MOCK_OTHER_VOICE_CHANNEL_ID]
    );
    let streams = f.mock.streams();
    assert!(
        !streams.is_empty()
            && streams
                .iter()
                .all(|s| s.stream_key.contains(MOCK_OTHER_VOICE_CHANNEL_ID)),
        "{streams:?}"
    );
    f.finish().await;
}

const OTHER_MEMBER: &str = "100000000000000777";

#[tokio::test(flavor = "multi_thread")]
async fn another_members_voice_state_during_the_join_never_hides_the_accounts_move() {
    let mut f = Fixture::streaming().await;
    restart_in(&mut f, MOCK_VOICE_CHANNEL_ID, |mock| {
        mock.move_during_next_join(MOCK_GUILD_ID, MOCK_OTHER_VOICE_CHANNEL_ID);
        mock.other_user_voice_state_during_next_join(
            OTHER_MEMBER,
            MOCK_GUILD_ID,
            Some(MOCK_VOICE_CHANNEL_ID),
        );
    })
    .await;
    assert_eq!(moves_seen(&mut f), [MOCK_OTHER_VOICE_CHANNEL_ID]);
    let streams = f.mock.streams();
    assert!(
        !streams.is_empty()
            && streams
                .iter()
                .all(|s| s.stream_key.contains(MOCK_OTHER_VOICE_CHANNEL_ID)),
        "{streams:?}"
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn another_members_voice_states_while_streaming_change_nothing() {
    let mut f = Fixture::streaming().await;
    f.states_seen();
    assert!(f.mock.voice_state_of_other_user(
        OTHER_MEMBER,
        MOCK_GUILD_ID,
        Some(MOCK_OTHER_VOICE_CHANNEL_ID)
    ));
    assert!(
        f.mock
            .voice_state_of_other_user(OTHER_MEMBER, MOCK_GUILD_ID, None)
    );
    // Gateway events are handled in order, so a viewer count that arrives later proves both states were seen.
    assert!(f.mock.set_viewers(&[OTHER_MEMBER]));
    wait_viewers(&mut f, Some(1)).await;

    assert_eq!(*f.state.borrow(), SessionState::Streaming);
    assert_eq!(f.gateway_ops(gw::OP_STREAM_CREATE), 1);
    assert_eq!(f.gateway_ops(gw::OP_STREAM_DELETE), 0);
    assert_eq!(f.voice_joins(), [MOCK_VOICE_CHANNEL_ID]);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_move_while_joining_is_followed() {
    let mut f = Fixture::streaming().await;
    restart_in(&mut f, MOCK_VOICE_CHANNEL_ID, |mock| {
        mock.move_during_next_join(MOCK_GUILD_ID, MOCK_OTHER_VOICE_CHANNEL_ID);
    })
    .await;
    let events = events_seen(&mut f);
    let moves: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ChannelMoved { channel_id, .. } => Some(channel_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(moves, [MOCK_OTHER_VOICE_CHANNEL_ID]);
    let replayed = |e: &SessionEvent| {
        matches!(
            e,
            SessionEvent::State {
                state: SessionState::Reconnecting { attempt: 1 }
            }
        )
    };
    assert!(
        events.iter().any(replayed),
        "the move was not replayed at the join: {events:?}"
    );
    assert_eq!(
        f.voice_joins(),
        [MOCK_VOICE_CHANNEL_ID, MOCK_VOICE_CHANNEL_ID]
    );
    let streams = f.mock.streams();
    assert!(
        !streams.is_empty()
            && streams
                .iter()
                .all(|s| s.stream_key.contains(MOCK_OTHER_VOICE_CHANNEL_ID)),
        "{streams:?}"
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn gateway_drop_resumes_and_leaves_the_stream_alone() {
    let mut f = Fixture::streaming().await;
    f.states_seen();
    let mut gateway = f.client.subscribe();
    assert!(f.mock.drop_gateway(Some(4000)));
    f.mock
        .wait_for_op(None, gw::OP_RESUME, 0, T)
        .await
        .expect("gateway RESUME");
    // The session reads the same broadcast, so Disconnected and Resumed are in its queue by now.
    timeout(T, async {
        while !matches!(gateway.recv().await, Ok(GatewayEvent::Resumed)) {}
    })
    .await
    .expect("the client's Resumed");
    let mut states = f
        .states_until(|e| matches!(e, SessionEvent::Health(_)))
        .await;
    f.video_flowing().await;
    states.extend(f.states_seen());

    assert_eq!(states, []);
    assert_eq!(f.gateway_ops(gw::OP_STREAM_CREATE), 1);
    assert_eq!(f.voice_joins(), [MOCK_VOICE_CHANNEL_ID]);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_session_rejoins_and_recovers() {
    let mut options = options();
    // A peer connection that never connects is retried after the step timeout, which must come within the wait below.
    options.stream.step_timeout = Duration::from_secs(5);
    let mut f = Fixture::streaming_with(options).await;
    f.states_seen();
    assert!(f.mock.invalidate_session(false));
    f.mock
        .wait_for_op(None, gw::OP_IDENTIFY, 1, T)
        .await
        .expect("second IDENTIFY");
    // The event, unlike the state, is still there if this task only runs once the session is streaming again.
    f.wait_event(|e| {
        matches!(
            e,
            SessionEvent::State {
                state: SessionState::Reconnecting { attempt: 1 }
            }
        )
    })
    .await;
    f.wait_state_explained(|s| *s == SessionState::Streaming)
        .await;
    f.video_flowing().await;

    let joins = f.voice_joins();
    assert!(
        (2..=4).contains(&joins.len()) && joins.iter().all(|c| c == MOCK_VOICE_CHANNEL_ID),
        "{joins:?}"
    );
    assert_eq!(f.starts(), 1);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn health_snapshots_report_plausible_scores() {
    let mut f = Fixture::streaming().await;
    let mut snapshots = Vec::new();
    while snapshots.len() < 3 {
        if let SessionEvent::Health(h) =
            f.wait_event(|e| matches!(e, SessionEvent::Health(_))).await
        {
            snapshots.push(h);
        }
    }
    let mut history = Vec::new();
    for h in &snapshots {
        history.push(h.metrics);
        assert_eq!(h.health, StreamHealth::compute(&history, 30), "{h:?}");
    }
    for pair in snapshots.windows(2) {
        let (prev, cur) = (&pair[0].stats, &pair[1].stats);
        let m = &pair[1].metrics;
        assert!(
            cur.video_frames_sent >= prev.video_frames_sent,
            "{snapshots:?}"
        );
        assert_eq!(m.a_sent, cur.audio_frames_sent - prev.audio_frames_sent);
        assert_eq!(
            m.a_drop_not_ready,
            cur.audio_frames_dropped - prev.audio_frames_dropped
        );
    }
    let (first, last) = (&snapshots[0], snapshots.last().unwrap());
    assert!(
        last.stats.video_frames_sent > first.stats.video_frames_sent,
        "{snapshots:?}"
    );
    assert_eq!(last.health.fps_series.len(), history.len());
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn keyframes_are_requested_after_dave_and_on_viewer_pli() {
    let mut f = Fixture::streaming().await;
    assert!(f.mock.dave_active(VoiceKind::Stream).await);
    let dave_requested = |e: &SessionEvent| matches!(e, SessionEvent::Health(h) if h.stats.dave_keyframe_requests == 1);
    let SessionEvent::Health(h) = f.wait_event(dave_requested).await else {
        unreachable!()
    };
    assert_eq!(h.stats.viewer_keyframe_requests, 0, "{h:?}");

    let before = f.counters.keyframe_requests.load(Ordering::SeqCst);
    assert!(f.mock.send_pli(VoiceKind::Stream).await);
    let viewer_requested = |e: &SessionEvent| matches!(e, SessionEvent::Health(h) if h.stats.viewer_keyframe_requests == 1);
    let SessionEvent::Health(h) = f.wait_event(viewer_requested).await else {
        unreachable!()
    };
    assert_eq!(h.stats.dave_keyframe_requests, 1, "{h:?}");
    assert!(f.counters.keyframe_requests.load(Ordering::SeqCst) > before);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn update_video_announces_op_12_and_survives_reconnects() {
    let mut f = Fixture::streaming().await;
    f.session.update_video(160, 90, 15).await.unwrap();
    let has_attrs = |o: &dstreamy_mock_discord::ReceivedOp| {
        o.json()
            .is_some_and(|v| v["streams"][0]["max_resolution"]["width"] == 160)
    };
    f.mock
        .wait_for(T, |r| {
            r.ops
                .iter()
                .filter(|o| o.is(Some(VoiceKind::Stream), vo::OP_VIDEO))
                .any(has_attrs)
                .then_some(())
        })
        .await
        .expect("op 12 with the new attributes");
    let announced = |ops: &[dstreamy_mock_discord::ReceivedOp]| {
        ops.iter()
            .filter(|o| o.is(Some(VoiceKind::Stream), vo::OP_VIDEO))
            .filter(|o| has_attrs(o))
            .count()
    };
    let once = announced(&f.mock.received_ops());
    assert!(
        f.mock
            .drop_voice(VoiceKind::Stream, Some(vo::CLOSE_SESSION_NO_LONGER_VALID))
    );
    wait_state(&mut f.state, |s| {
        matches!(s, SessionState::Reconnecting { .. })
    })
    .await;
    wait_state(&mut f.state, |s| *s == SessionState::Streaming).await;
    f.mock
        .wait_for(T, |r| (announced(&r.ops) > once).then_some(()))
        .await
        .expect("new attributes after the reconnect");
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn e2ee_is_reported_once_dave_is_up_and_in_every_health_snapshot() {
    let mut f = Fixture::streaming().await;
    let reported = f
        .wait_event(|e| matches!(e, SessionEvent::E2ee { .. }))
        .await;
    assert!(
        matches!(
            reported,
            SessionEvent::E2ee {
                state: E2ee::Encrypted,
                dave_protocol_version: 1
            }
        ),
        "{reported:?}"
    );
    f.wait_event(|e| {
        matches!(e, SessionEvent::Health(h) if h.stats.e2ee == E2ee::Encrypted && h.stats.dave_protocol_version == 1)
    })
    .await;
    f.finish().await;
}

const GUEST: u64 = 100_000_000_000_008_888;

async fn dave_guest_present(f: &Fixture, want: bool) {
    timeout(T, async {
        while f.mock.dave_has_guest(VoiceKind::Stream).await != want {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the mock's DAVE guest never became {want}"));
}

fn tagged_first_frame_drops(r: &dstreamy_mock_discord::Recorded) -> usize {
    r.dropped_frames
        .iter()
        .filter(|d| d.starts_with(dstreamy_mock_discord::FIRST_FRAME_DAVE_DROP))
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_member_joining_and_leaving_the_call_rekeys_dave_without_losing_frames() {
    let mut f = Fixture::streaming().await;
    f.wait_event(|e| {
        matches!(
            e,
            SessionEvent::E2ee {
                state: E2ee::Encrypted,
                ..
            }
        )
    })
    .await;
    let before = f.mock.recorded();
    let drops_before = before.dropped_frames.len() - tagged_first_frame_drops(&before);

    assert!(f.mock.join_dave_guest(VoiceKind::Stream, GUEST).await);
    dave_guest_present(&f, true).await;
    f.mock
        .wait_for_op(Some(VoiceKind::Stream), vo::OP_DAVE_TRANSITION_READY, 1, T)
        .await
        .expect("the join's TRANSITION_READY");
    f.video_flowing().await;

    assert!(f.mock.leave_dave_guest(VoiceKind::Stream, GUEST).await);
    dave_guest_present(&f, false).await;
    f.mock
        .wait_for_op(Some(VoiceKind::Stream), vo::OP_DAVE_TRANSITION_READY, 2, T)
        .await
        .expect("the leave's TRANSITION_READY");
    f.video_flowing().await;

    let after = f.mock.recorded();
    let drops_after = after.dropped_frames.len() - tagged_first_frame_drops(&after);
    assert_eq!(drops_after, drops_before, "{:?}", after.dropped_frames);
    assert!(after.video.iter().rev().take(10).all(|v| v.e2ee));
    assert_eq!(*f.state.borrow(), SessionState::Streaming);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dave_downgrade_and_upgrade_are_followed_and_reported() {
    let mut f = Fixture::streaming().await;
    f.wait_event(|e| {
        matches!(
            e,
            SessionEvent::E2ee {
                state: E2ee::Encrypted,
                ..
            }
        )
    })
    .await;
    let before = f.mock.recorded();
    let drops_before = before.dropped_frames.len() - tagged_first_frame_drops(&before);

    assert!(f.mock.downgrade_dave(VoiceKind::Stream).await);
    f.wait_event(|e| {
        matches!(
            e,
            SessionEvent::E2ee {
                state: E2ee::Unencrypted,
                dave_protocol_version: 0
            }
        )
    })
    .await;
    f.video_flowing().await;
    let plain = f.mock.recorded();
    assert!(plain.video.iter().rev().take(10).all(|v| !v.e2ee));

    assert!(f.mock.upgrade_dave(VoiceKind::Stream).await);
    f.wait_event(|e| {
        matches!(
            e,
            SessionEvent::E2ee {
                state: E2ee::Encrypted,
                dave_protocol_version: 1
            }
        )
    })
    .await;
    f.video_flowing().await;

    let after = f.mock.recorded();
    assert!(after.video.iter().rev().take(10).all(|v| v.e2ee));
    let drops_after = after.dropped_frames.len() - tagged_first_frame_drops(&after);
    assert_eq!(drops_after, drops_before, "{:?}", after.dropped_frames);
    assert_eq!(*f.state.borrow(), SessionState::Streaming);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn require_e2ee_stops_a_running_stream_before_downgraded_media_is_sent() {
    let mut opts = options();
    opts.stream.require_e2ee = true;
    let mut f = Fixture::streaming_with(opts).await;
    assert!(f.mock.recorded().video.iter().all(|v| v.e2ee));
    assert!(f.mock.downgrade_dave(VoiceKind::Stream).await);
    let stopped = timeout(
        Duration::from_secs(3),
        f.state
            .wait_for(|s| matches!(s, SessionState::Stopped { .. })),
    )
    .await;
    assert!(
        stopped.is_ok(),
        "strict stream continued after DAVE downgrade"
    );
    drop(stopped);
    assert_eq!(
        *f.state.borrow(),
        SessionState::Stopped {
            reason: StopReason::E2eeUnavailable
        }
    );
    let recorded = f.mock.recorded();
    assert!(
        recorded.video.iter().all(|v| v.e2ee),
        "plaintext video escaped"
    );
    assert!(
        recorded.audio.iter().all(|a| a.e2ee),
        "plaintext audio escaped"
    );
    f.wait_event(|e| matches!(e, SessionEvent::Error { fatal: true, .. }))
        .await;
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gateway_rate_limit_close_resumes_and_leaves_the_stream_alone() {
    let mut f = Fixture::streaming().await;
    f.states_seen();
    assert!(f.mock.drop_gateway(Some(4008)));
    f.mock
        .wait_for_op(None, gw::OP_RESUME, 0, T)
        .await
        .expect("gateway RESUME");
    f.video_flowing().await;

    assert_eq!(f.states_seen(), []);
    assert_eq!(f.gateway_ops(gw::OP_STREAM_CREATE), 1);
    assert_eq!(f.voice_joins(), [MOCK_VOICE_CHANNEL_ID]);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_voice_server_without_dave_is_reported_as_unencrypted() {
    let config = MockConfig {
        dave_protocol_version: 0,
        ..MockConfig::default()
    };
    let mut f = Fixture::streaming_on(config, options(), Behaviour::Normal, |_| {}, |_| {}).await;
    let reported = f
        .wait_event(|e| matches!(e, SessionEvent::E2ee { .. }))
        .await;
    assert!(
        matches!(
            reported,
            SessionEvent::E2ee {
                state: E2ee::Unencrypted,
                dave_protocol_version: 0
            }
        ),
        "{reported:?}"
    );
    f.wait_event(|e| matches!(e, SessionEvent::Health(h) if h.stats.e2ee == E2ee::Unencrypted))
        .await;
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn require_e2ee_refuses_a_voice_server_without_dave() {
    let config = MockConfig {
        dave_protocol_version: 0,
        ..MockConfig::default()
    };
    let mock = MockDiscord::start_with(config).await.unwrap();
    let client = client(&mock).await;
    let session = Session::new(client.clone());
    let mut events = session.subscribe();
    let mut state = session.state();
    let mut options = options();
    options.stream.require_e2ee = true;
    let (source, counters) = Counted::boxed();
    session
        .start(
            Some(MOCK_GUILD_ID.into()),
            MOCK_VOICE_CHANNEL_ID.into(),
            source,
            options,
        )
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));
    wait_state(&mut state, |s| matches!(s, SessionState::Stopped { .. })).await;
    assert_eq!(
        *state.borrow(),
        SessionState::Stopped {
            reason: StopReason::E2eeUnavailable
        }
    );
    let mut fatal = None;
    while let Ok(event) = events.try_recv() {
        if let SessionEvent::Error {
            message,
            fatal: true,
        } = event
        {
            fatal = Some(message);
        }
    }
    assert!(
        fatal
            .as_deref()
            .is_some_and(|m| m.contains("no end-to-end encryption")),
        "{fatal:?}"
    );
    assert_eq!(counters.starts.load(Ordering::SeqCst), 1);
    assert_eq!(mock.recorded_video().len(), 0);
    assert!(timeout(T, session.stop()).await.unwrap().unwrap().is_some());
    timeout(T, session.shutdown()).await.unwrap();
    Arc::try_unwrap(client).ok().unwrap().close().await;
    mock.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn gives_up_and_stops_when_voice_never_connects() {
    let mock = MockDiscord::start().await.unwrap();
    let client = client(&mock).await;
    let session = Session::new(client.clone());
    let mut state = session.state();
    let mut options = options();
    options.stream.step_timeout = Duration::from_millis(100);
    options.backoff.max_attempts = 2;
    let (source, counters) = Counted::boxed();
    session
        .start(Some(MOCK_GUILD_ID.into()), "999".into(), source, options)
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));
    wait_state(&mut state, |s| matches!(s, SessionState::Stopped { .. })).await;
    assert_eq!(
        *state.borrow(),
        SessionState::Stopped {
            reason: StopReason::GaveUp { attempts: 2 }
        }
    );
    assert_eq!(counters.starts.load(Ordering::SeqCst), 1);
    assert!(timeout(T, session.stop()).await.unwrap().unwrap().is_some());
    timeout(T, session.shutdown()).await.unwrap();
    Arc::try_unwrap(client).ok().unwrap().close().await;
    mock.shutdown().await;
}

#[test]
fn stop_hands_the_source_back_and_ends_every_task() {
    let mock_runtime = tokio::runtime::Runtime::new().unwrap();
    let mock = mock_runtime.block_on(MockDiscord::start()).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let client = client(&mock).await;
        let metrics = tokio::runtime::Handle::current().metrics();
        sleep(Duration::from_millis(200)).await;
        let baseline = metrics.num_alive_tasks();

        let session = Session::new(client.clone());
        let mut state = session.state();
        let (source, _) = Counted::boxed();
        session
            .start(
                Some(MOCK_GUILD_ID.into()),
                MOCK_VOICE_CHANNEL_ID.into(),
                source,
                options(),
            )
            .await
            .unwrap_or_else(|e| panic!("start: {e}"));
        wait_state(&mut state, |s| *s == SessionState::Streaming).await;
        mock.wait_for(T, |r| (r.video.len() >= 10).then_some(()))
            .await
            .expect("video");

        let source = timeout(T, session.stop()).await.unwrap().unwrap();
        assert!(source.is_some());
        assert_eq!(
            *state.borrow(),
            SessionState::Stopped {
                reason: StopReason::Requested
            }
        );
        mock.wait_for_op(None, gw::OP_STREAM_DELETE, 0, T)
            .await
            .expect("STREAM_DELETE");
        timeout(T, session.shutdown()).await.unwrap();

        timeout(T, async {
            while metrics.num_alive_tasks() > baseline {
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{} tasks alive after stop, {baseline} before start",
                metrics.num_alive_tasks()
            )
        });
        Arc::try_unwrap(client).ok().unwrap().close().await;
    });
    mock_runtime.block_on(mock.shutdown());
}

async fn idle_session(
    mock: &MockDiscord,
) -> (Arc<DiscordClient>, Session, watch::Receiver<SessionState>) {
    let client = client(mock).await;
    let session = Session::new(client.clone());
    let state = session.state();
    (client, session, state)
}

async fn close(client: Arc<DiscordClient>, session: Session, mock: MockDiscord) {
    timeout(T, session.shutdown()).await.unwrap();
    Arc::try_unwrap(client).ok().unwrap().close().await;
    mock.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_stats_include_shutdown_losses_and_clear_before_the_next_run() {
    let mut opts = options();
    opts.health_interval = Duration::from_secs(3600);
    let mut f = Fixture::streaming_source(opts.clone(), Behaviour::StopDrops).await;
    assert!(f.session.terminal_stats().is_none());
    let source = f.session.stop().await.unwrap().unwrap();
    let stats = f.session.terminal_stats().unwrap();
    assert_eq!(stats.source_video_frames_dropped, 7);
    assert_eq!(stats.source_audio_frames_dropped, 11);
    assert_eq!(stats.e2ee, E2ee::Encrypted);
    assert_eq!(source.capture_drops().video_frames, 7);
    while let Ok(event) = f.events.try_recv() {
        assert!(
            !matches!(event, SessionEvent::Health(_)),
            "stop fabricated a health sample"
        );
    }
    f.session
        .start(
            Some(MOCK_GUILD_ID.into()),
            MOCK_VOICE_CHANNEL_ID.into(),
            source,
            opts,
        )
        .await
        .unwrap();
    assert!(f.session.terminal_stats().is_none());
    wait_state(&mut f.state, |s| matches!(s, SessionState::Streaming)).await;
    assert!(f.session.terminal_stats().is_none());
    f.session.stop().await.unwrap();
    assert_eq!(
        f.session
            .terminal_stats()
            .unwrap()
            .source_video_frames_dropped,
        7
    );
    close(f.client, f.session, f.mock).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_stats_preserve_teardown_errors_and_timeout_lower_bounds() {
    for behaviour in [Behaviour::StopError, Behaviour::HangStop] {
        let f = Fixture::streaming_source(options(), behaviour).await;
        let source = f.session.stop().await.unwrap().unwrap();
        let stats = f.session.terminal_stats().unwrap();
        let expected = if behaviour == Behaviour::StopError {
            (7, 11)
        } else {
            (3, 5)
        };
        assert_eq!(
            (
                stats.source_video_frames_dropped,
                stats.source_audio_frames_dropped
            ),
            expected
        );
        let error = stats.last_error.unwrap();
        assert!(
            error.contains(if behaviour == Behaviour::StopError {
                "injected teardown failure"
            } else {
                "stopping the capture source"
            }),
            "{error}"
        );
        assert_eq!(source.capture_drops().video_frames, expected.0);
        close(f.client, f.session, f.mock).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_while_reconnecting_hands_the_source_back() {
    let mock = MockDiscord::start().await.unwrap();
    let (client, session, mut state) = idle_session(&mock).await;
    let mut options = options();
    options.stream.step_timeout = Duration::from_millis(100);
    options.backoff.base = Duration::from_secs(5);
    options.backoff.max = Duration::from_secs(5);
    let (source, _) = Counted::boxed();
    session
        .start(Some(MOCK_GUILD_ID.into()), "999".into(), source, options)
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));
    wait_state(&mut state, |s| {
        matches!(s, SessionState::Reconnecting { .. })
    })
    .await;
    let source = timeout(T, session.stop()).await.unwrap().unwrap();
    assert!(source.is_some());
    assert_eq!(
        *state.borrow(),
        SessionState::Stopped {
            reason: StopReason::Requested
        }
    );
    assert!(timeout(T, session.stop()).await.unwrap().unwrap().is_none());
    close(client, session, mock).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_start_is_refused_with_its_source() {
    let f = Fixture::streaming().await;
    let (source, counters) = Counted::boxed();
    let err = f
        .session
        .start(
            Some(MOCK_GUILD_ID.into()),
            MOCK_VOICE_CHANNEL_ID.into(),
            source,
            options(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err.error, SessionError::AlreadyRunning), "{err}");
    assert!(err.source.is_some());
    assert_eq!(counters.starts.load(Ordering::SeqCst), 0);
    assert_eq!(f.starts(), 1);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_source_that_fails_to_start_stops_the_session_and_comes_back() {
    let mock = MockDiscord::start().await.unwrap();
    let (client, session, mut state) = idle_session(&mock).await;
    let (source, _) = Counted::with(Behaviour::FailStart);
    session
        .start(
            Some(MOCK_GUILD_ID.into()),
            MOCK_VOICE_CHANNEL_ID.into(),
            source,
            options(),
        )
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));
    wait_state(&mut state, |s| matches!(s, SessionState::Stopped { .. })).await;
    assert!(matches!(
        &*state.borrow(),
        SessionState::Stopped {
            reason: StopReason::SourceFailed { .. }
        }
    ));
    let recorded = mock.recorded();
    let ops = |op| recorded.ops.iter().filter(|o| o.is(None, op)).count();
    assert_eq!(ops(gw::OP_STREAM_CREATE), 0, "Go Live without a source");
    assert!(timeout(T, session.stop()).await.unwrap().unwrap().is_some());
    close(client, session, mock).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ended_capture_stops_the_session() {
    let mock = MockDiscord::start().await.unwrap();
    let (client, session, mut state) = idle_session(&mock).await;
    let (source, _) = Counted::with(Behaviour::EndAfter(30));
    let mut options = options();
    options.health_interval = Duration::from_millis(100);
    session
        .start(
            Some(MOCK_GUILD_ID.into()),
            MOCK_VOICE_CHANNEL_ID.into(),
            source,
            options,
        )
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));
    wait_state(&mut state, |s| matches!(s, SessionState::Stopped { .. })).await;
    assert_eq!(
        *state.borrow(),
        SessionState::Stopped {
            reason: StopReason::CaptureEnded
        }
    );
    assert!(timeout(T, session.stop()).await.unwrap().unwrap().is_some());
    close(client, session, mock).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_identify_limit_stops_the_session_with_its_reason() {
    let mock = MockDiscord::start().await.unwrap();
    let client = client_with(&mock, |o| o.max_identifies_per_day = 1, |_| {}).await;
    let session = Session::new(client.clone());
    let mut state = session.state();
    let mut events = session.subscribe();
    let (source, _) = Counted::with(Behaviour::Normal);
    session
        .start(
            Some(MOCK_GUILD_ID.into()),
            MOCK_VOICE_CHANNEL_ID.into(),
            source,
            options(),
        )
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));
    wait_state(&mut state, |s| *s == SessionState::Streaming).await;

    assert!(mock.drop_gateway(Some(4009)));
    wait_state(&mut state, |s| matches!(s, SessionState::Stopped { .. })).await;
    let stopped = state.borrow().clone();
    let SessionState::Stopped {
        reason: StopReason::IdentifyLimit { retry_after_secs },
    } = stopped
    else {
        panic!("{stopped:?}")
    };
    assert!(retry_after_secs > 23 * 60 * 60, "{retry_after_secs}");
    let message = timeout(T, async {
        loop {
            if let Ok(SessionEvent::Error { message, .. }) = events.recv().await {
                return message;
            }
        }
    })
    .await
    .expect("no error event");
    assert!(message.contains("protect the account"), "{message}");
    assert!(message.contains("Try again in about 24 h."), "{message}");
    assert!(timeout(T, session.stop()).await.unwrap().unwrap().is_some());
    close(client, session, mock).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_gateway_stops_the_session() {
    let mut f = Fixture::streaming().await;
    assert!(f.mock.drop_gateway(Some(gw::CLOSE_AUTHENTICATION_FAILED)));
    wait_state(&mut f.state, |s| matches!(s, SessionState::Stopped { .. })).await;
    assert_eq!(
        *f.state.borrow(),
        SessionState::Stopped {
            reason: StopReason::InvalidToken
        }
    );
    assert!(
        timeout(T, f.session.stop())
            .await
            .unwrap()
            .unwrap()
            .is_some()
    );
    timeout(T, f.session.shutdown()).await.unwrap();
    Arc::try_unwrap(f.client).ok().unwrap().close().await;
    f.mock.shutdown().await;
}

/// The next `Reconnecting` attempt or `Stopped` state announced on `events`.
async fn next_settled(events: &mut broadcast::Receiver<SessionEvent>) -> SessionState {
    timeout(T, async {
        loop {
            if let Ok(SessionEvent::State { state }) = events.recv().await
                && matches!(
                    state,
                    SessionState::Reconnecting { .. } | SessionState::Stopped { .. }
                )
            {
                return state;
            }
        }
    })
    .await
    .expect("no reconnect or stop announced")
}

#[tokio::test(flavor = "multi_thread")]
async fn reconnect_attempts_count_rapid_drops_and_reset_after_a_stable_stream() {
    let mut options = options();
    options.backoff.max_attempts = 3;
    options.stable_streaming = Duration::from_millis(600);
    let mut f = Fixture::streaming_with(options).await;
    let mut events = f.session.subscribe();
    let drop_stream = async |f: &mut Fixture, events: &mut broadcast::Receiver<SessionEvent>| {
        assert!(
            f.mock
                .drop_voice(VoiceKind::Stream, Some(vo::CLOSE_SESSION_NO_LONGER_VALID))
        );
        let settled = next_settled(events).await;
        if !matches!(settled, SessionState::Stopped { .. }) {
            wait_state(&mut f.state, |s| *s == SessionState::Streaming).await;
        }
        settled
    };
    let reconnecting = |attempt| SessionState::Reconnecting { attempt };
    assert_eq!(drop_stream(&mut f, &mut events).await, reconnecting(1));
    assert_eq!(drop_stream(&mut f, &mut events).await, reconnecting(2));
    sleep(Duration::from_millis(800)).await;
    assert_eq!(drop_stream(&mut f, &mut events).await, reconnecting(1));
    assert_eq!(drop_stream(&mut f, &mut events).await, reconnecting(2));
    assert_eq!(drop_stream(&mut f, &mut events).await, reconnecting(3));
    assert_eq!(
        drop_stream(&mut f, &mut events).await,
        SessionState::Stopped {
            reason: StopReason::GaveUp { attempts: 3 }
        }
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_moderator_removal_rejoins_whichever_event_comes_first() {
    for close_first in [true, false] {
        let mut options = options();
        options.backoff.max_attempts = 2;
        let mut f = Fixture::streaming_with(options).await;
        for _ in 0..4 {
            let mut events = f.session.subscribe();
            if close_first {
                assert!(
                    f.mock
                        .drop_voice(VoiceKind::Voice, Some(vo::CLOSE_DISCONNECTED))
                );
                assert_eq!(
                    next_settled(&mut events).await,
                    SessionState::Reconnecting { attempt: 1 }
                );
                f.mock.null_voice_state();
            } else {
                assert!(f.mock.null_voice_state());
                assert_eq!(
                    next_settled(&mut events).await,
                    SessionState::Reconnecting { attempt: 1 }
                );
                f.mock
                    .drop_voice(VoiceKind::Voice, Some(vo::CLOSE_DISCONNECTED));
            }
            wait_state(&mut f.state, |s| *s == SessionState::Streaming).await;
        }
        f.finish().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn update_video_while_reconnecting_reaches_the_new_connection() {
    let mut options = options();
    options.backoff.base = Duration::from_millis(600);
    options.backoff.max = Duration::from_millis(600);
    let mut f = Fixture::streaming_with(options).await;
    assert!(
        f.mock
            .drop_voice(VoiceKind::Stream, Some(vo::CLOSE_SESSION_NO_LONGER_VALID))
    );
    wait_state(&mut f.state, |s| {
        matches!(s, SessionState::Reconnecting { .. })
    })
    .await;
    f.session.update_video(160, 90, 15).await.unwrap();
    wait_state(&mut f.state, |s| *s == SessionState::Streaming).await;
    let last_width = |r: &dstreamy_mock_discord::Recorded| {
        r.ops
            .iter()
            .rev()
            .filter(|o| o.is(Some(VoiceKind::Stream), vo::OP_VIDEO))
            .find_map(|o| o.json()?["streams"][0]["max_resolution"]["width"].as_u64())
    };
    f.mock
        .wait_for(T, |r| (last_width(r) == Some(160)).then_some(()))
        .await
        .unwrap_or_else(|| panic!("last op 12 width {:?}", last_width(&f.mock.recorded())));
    f.video_flowing().await;
    f.finish().await;
}

#[test]
fn session_state_and_events_serialize_for_json_output() {
    let state = SessionState::Stopped {
        reason: StopReason::GaveUp { attempts: 3 },
    };
    assert_eq!(
        serde_json::to_value(&state).unwrap(),
        serde_json::json!({ "state": "stopped", "reason": { "reason": "gave_up", "attempts": 3 } })
    );
    assert_eq!(
        state.to_string(),
        "stopped: gave up after 3 reconnect attempts"
    );
    let event = SessionEvent::State {
        state: SessionState::Reconnecting { attempt: 2 },
    };
    assert_eq!(
        serde_json::to_value(&event).unwrap(),
        serde_json::json!({ "event": "state", "state": { "state": "reconnecting", "attempt": 2 } })
    );
    let e2ee = SessionEvent::E2ee {
        state: E2ee::Unencrypted,
        dave_protocol_version: 0,
    };
    assert_eq!(
        serde_json::to_value(&e2ee).unwrap(),
        serde_json::json!({ "event": "e2ee", "state": "unencrypted", "dave_protocol_version": 0 })
    );
    let refused = SessionState::Stopped {
        reason: StopReason::E2eeUnavailable,
    };
    assert_eq!(
        serde_json::to_value(&refused).unwrap(),
        serde_json::json!({ "state": "stopped", "reason": { "reason": "e2ee_unavailable" } })
    );
    let shapes = [
        (
            StopReason::InvalidToken,
            serde_json::json!({ "reason": "invalid_token" }),
        ),
        (
            StopReason::IdentifyLimit {
                retry_after_secs: 3600,
            },
            serde_json::json!({ "reason": "identify_limit", "retry_after_secs": 3600 }),
        ),
    ];
    for (reason, json) in shapes {
        assert_eq!(serde_json::to_value(&reason).unwrap(), json);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_source_start_outlasts_the_step_timeout_before_go_live_exists() {
    let mock = MockDiscord::start().await.unwrap();
    let (client, session, mut state) = idle_session(&mock).await;
    let (source, counters) = Counted::with(Behaviour::SlowStart(1500));
    let mut options = options();
    options.stream.step_timeout = Duration::from_millis(300);
    session
        .start(
            Some(MOCK_GUILD_ID.into()),
            MOCK_VOICE_CHANNEL_ID.into(),
            source,
            options,
        )
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));
    sleep(Duration::from_millis(900)).await;
    assert_eq!(counters.starts.load(Ordering::SeqCst), 1);
    assert_eq!(*state.borrow(), SessionState::Connecting);
    let recorded = mock.recorded();
    let ops = |op| recorded.ops.iter().filter(|o| o.is(None, op)).count();
    assert_eq!(
        ops(gw::OP_STREAM_CREATE),
        0,
        "Go Live before the source started"
    );
    assert_eq!(
        ops(gw::OP_VOICE_STATE_UPDATE),
        0,
        "voice joined before the source started"
    );
    wait_state(&mut state, |s| *s == SessionState::Streaming).await;
    assert!(timeout(T, session.stop()).await.unwrap().unwrap().is_some());
    close(client, session, mock).await;
}

fn ops(mock: &MockDiscord, op: u8) -> usize {
    mock.recorded()
        .ops
        .iter()
        .filter(|o| o.is(None, op))
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_source_that_never_starts_stops_at_the_source_start_timeout_without_joining() {
    let mock = MockDiscord::start().await.unwrap();
    let (client, session, mut state) = idle_session(&mock).await;
    let (source, _) = Counted::with(Behaviour::HangStart);
    let mut options = options();
    options.stream.source_start_timeout = Duration::from_secs(1);
    session
        .start(
            Some(MOCK_GUILD_ID.into()),
            MOCK_VOICE_CHANNEL_ID.into(),
            source,
            options,
        )
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));
    wait_state(&mut state, |s| matches!(s, SessionState::Stopped { .. })).await;
    let stopped = state.borrow().clone();
    let SessionState::Stopped {
        reason: StopReason::SourceFailed { message },
    } = stopped
    else {
        panic!("{stopped:?}")
    };
    assert!(message.contains("did not start within 1 s"), "{message}");
    assert!(message.contains("screen sharing dialog"), "{message}");
    assert_eq!(ops(&mock, gw::OP_VOICE_STATE_UPDATE), 0);
    assert_eq!(ops(&mock, gw::OP_STREAM_CREATE), 0);
    close(client, session, mock).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_never_joined_sends_no_leave() {
    for behaviour in [Behaviour::FailStart, Behaviour::HangStart] {
        let mock = MockDiscord::start().await.unwrap();
        let (client, session, mut state) = idle_session(&mock).await;
        let (source, _) = Counted::with(behaviour);
        session
            .start(
                Some(MOCK_GUILD_ID.into()),
                MOCK_VOICE_CHANNEL_ID.into(),
                source,
                options(),
            )
            .await
            .unwrap_or_else(|e| panic!("start: {e}"));
        if behaviour == Behaviour::HangStart {
            sleep(Duration::from_millis(200)).await;
            timeout(T, session.stop()).await.unwrap().unwrap();
        }
        wait_state(&mut state, |s| matches!(s, SessionState::Stopped { .. })).await;
        sleep(Duration::from_millis(200)).await;
        assert_eq!(
            ops(&mock, gw::OP_VOICE_STATE_UPDATE),
            0,
            "op 4 without a join"
        );
        close(client, session, mock).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn another_device_in_voice_while_the_source_starts_does_not_end_the_run() {
    let mock = MockDiscord::start().await.unwrap();
    let (client, session, mut state) = idle_session(&mock).await;
    let (source, _) = Counted::with(Behaviour::SlowStart(800));
    session
        .start(
            Some(MOCK_GUILD_ID.into()),
            MOCK_VOICE_CHANNEL_ID.into(),
            source,
            options(),
        )
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));
    sleep(Duration::from_millis(200)).await;
    assert!(mock.voice_state_from_other_session(MOCK_GUILD_ID, Some(MOCK_OTHER_VOICE_CHANNEL_ID)));
    wait_state(&mut state, |s| {
        matches!(s, SessionState::Streaming | SessionState::Stopped { .. })
    })
    .await;
    assert_eq!(*state.borrow(), SessionState::Streaming);
    assert!(timeout(T, session.stop()).await.unwrap().unwrap().is_some());
    close(client, session, mock).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reconfigure_during_the_start_restarts_the_source_only_for_a_new_config() {
    for changed in [false, true] {
        let mock = MockDiscord::start().await.unwrap();
        let (client, session, mut state) = idle_session(&mock).await;
        let (source, counters) = Counted::with(Behaviour::SlowStart(600));
        let options = options();
        let original = options.stream.capture.clone();
        session
            .start(
                Some(MOCK_GUILD_ID.into()),
                MOCK_VOICE_CHANNEL_ID.into(),
                source,
                options,
            )
            .await
            .unwrap_or_else(|e| panic!("start: {e}"));
        sleep(Duration::from_millis(100)).await;
        let other = CaptureConfig {
            bitrate_bps: original.bitrate_bps * 2,
            ..original.clone()
        };
        session.reconfigure(other.clone()).await.unwrap();
        if !changed {
            session.reconfigure(original.clone()).await.unwrap();
        }
        wait_state(&mut state, |s| *s == SessionState::Streaming).await;
        let configs = counters.configs.lock().unwrap().clone();
        if changed {
            assert_eq!(configs.len(), 2);
            assert_eq!(configs[1].bitrate_bps, other.bitrate_bps);
        } else {
            assert_eq!(configs.len(), 1, "restarted for the config it started with");
        }
        assert!(timeout(T, session.stop()).await.unwrap().unwrap().is_some());
        close(client, session, mock).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hung_source_start_times_out_and_stop_returns() {
    let mock = MockDiscord::start().await.unwrap();
    let (client, session, mut state) = idle_session(&mock).await;
    let (source, _) = Counted::with(Behaviour::HangStart);
    let mut options = options();
    options.stream.step_timeout = Duration::from_secs(2);
    session
        .start(
            Some(MOCK_GUILD_ID.into()),
            MOCK_VOICE_CHANNEL_ID.into(),
            source,
            options,
        )
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));
    let stopped = timeout(T, session.stop()).await.expect("stop hung");
    assert!(stopped.is_ok());
    wait_state(&mut state, |s| matches!(s, SessionState::Stopped { .. })).await;
    close(client, session, mock).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn another_device_joining_voice_stops_the_session_without_leaving() {
    let mut f = Fixture::streaming().await;
    assert!(
        f.mock
            .voice_state_from_other_session(MOCK_GUILD_ID, Some(MOCK_OTHER_VOICE_CHANNEL_ID))
    );
    wait_state(&mut f.state, |s| matches!(s, SessionState::Stopped { .. })).await;
    assert_eq!(
        *f.state.borrow(),
        SessionState::Stopped {
            reason: StopReason::VoiceTakenOver
        }
    );
    // A marker op 4 on the same path as a leave would take: once it arrives, any leave would have too.
    f.client.streamer().signal_video(false);
    f.mock
        .wait_for(T, |r| {
            let joins = r
                .ops
                .iter()
                .filter(|o| o.is(None, gw::OP_VOICE_STATE_UPDATE))
                .filter(|o| o.json().is_some_and(|d| d["channel_id"].is_string()))
                .count();
            (joins == 2).then_some(())
        })
        .await
        .expect("marker op 4");
    let leaves = f
        .mock
        .received_ops()
        .iter()
        .filter(|o| o.is(None, gw::OP_VOICE_STATE_UPDATE))
        .filter(|o| o.json().is_some_and(|d| d["channel_id"].is_null()))
        .count();
    assert_eq!(
        leaves, 0,
        "left the voice channel the other device now holds"
    );
    assert_eq!(f.gateway_ops(gw::OP_STREAM_DELETE), 0);
    assert!(
        timeout(T, f.session.stop())
            .await
            .unwrap()
            .unwrap()
            .is_some()
    );
    timeout(T, f.session.shutdown()).await.unwrap();
    Arc::try_unwrap(f.client).ok().unwrap().close().await;
    f.mock.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn another_device_leaving_voice_is_ignored() {
    let mut f = Fixture::streaming().await;
    assert!(f.mock.voice_state_from_other_session(MOCK_GUILD_ID, None));
    assert!(
        f.mock
            .move_to_channel(MOCK_GUILD_ID, MOCK_OTHER_VOICE_CHANNEL_ID)
    );
    let mut removed = false;
    timeout(T, async {
        loop {
            match f.events.recv().await {
                Ok(SessionEvent::ChannelMoved { .. }) => break,
                Ok(SessionEvent::Error { message, .. }) => removed |= message.contains("removed"),
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("session events: {e}"),
            }
        }
    })
    .await
    .expect("ChannelMoved");
    assert!(!removed, "another device leaving was taken as our removal");
    wait_state(&mut f.state, |s| *s == SessionState::Streaming).await;
    f.video_flowing().await;
    assert_eq!(f.voice_joins(), [MOCK_VOICE_CHANNEL_ID]);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn viewer_fir_and_pli_each_count_once() {
    let mut f = Fixture::streaming().await;
    let dave_requested = |e: &SessionEvent| matches!(e, SessionEvent::Health(h) if h.stats.dave_keyframe_requests == 1);
    f.wait_event(dave_requested).await;
    let conn = f.client.streamer().stream_connection().cloned().unwrap();
    let mut requests = conn.keyframe_requests();
    let base = *requests.borrow_and_update();
    let before = f.counters.keyframe_requests.load(Ordering::SeqCst);

    assert!(f.mock.send_fir(VoiceKind::Stream, 1).await);
    timeout(T, requests.wait_for(|n| *n > base))
        .await
        .unwrap()
        .unwrap();
    timeout(T, async {
        while f.counters.keyframe_requests.load(Ordering::SeqCst) == before {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("keyframe after FIR");

    assert!(f.mock.send_pli(VoiceKind::Stream).await);
    let viewer_twice = |e: &SessionEvent| matches!(e, SessionEvent::Health(h) if h.stats.viewer_keyframe_requests == 2);
    f.wait_event(viewer_twice).await;
    assert_eq!(
        *requests.borrow(),
        base + 2,
        "a viewer request counted more than once"
    );
    drop(conn);
    f.finish().await;
}

fn smaller_capture() -> CaptureConfig {
    CaptureConfig {
        width: 160,
        height: 90,
        fps: 15,
        bitrate_bps: 200_000,
        codec: VideoCodec::H264,
        ..CaptureConfig::default()
    }
}

fn announced(r: &dstreamy_mock_discord::Recorded, width: u64, fps: u64) -> bool {
    r.ops
        .iter()
        .rev()
        .filter(|o| o.is(Some(VoiceKind::Stream), vo::OP_VIDEO))
        .find_map(|o| o.json().map(|d| d["streams"][0].clone()))
        .is_some_and(|s| s["max_resolution"]["width"] == width && s["max_framerate"] == fps)
}

#[tokio::test(flavor = "multi_thread")]
async fn reconfigure_restarts_the_source_inside_go_live() {
    let mut f = Fixture::streaming().await;
    let dave_requested = |e: &SessionEvent| matches!(e, SessionEvent::Health(h) if h.stats.dave_keyframe_requests == 1);
    f.wait_event(dave_requested).await;
    let mut h265 = smaller_capture();
    h265.codec = VideoCodec::H265;
    let err = f.session.reconfigure(h265).await.unwrap_err();
    assert!(matches!(err, SessionError::CodecChange), "{err}");

    let before = stream_video(&f.mock.recorded()).len();
    let audio_before = f.mock.recorded().audio.len();
    f.session.reconfigure(smaller_capture()).await.unwrap();
    assert_eq!(*f.state.borrow(), SessionState::Streaming);
    f.mock
        .wait_for(T, |r| announced(r, 160, 15).then_some(()))
        .await
        .expect("op 12 with the new attributes");
    let recorded = f
        .mock
        .wait_for(T, |r| {
            (stream_video(r).len() >= before + 25).then(|| r.clone())
        })
        .await
        .expect("video after the reconfigure");

    assert_eq!(f.starts(), 2);
    assert_eq!(
        f.counters.configs.lock().unwrap().last(),
        Some(&smaller_capture())
    );
    let video = stream_video(&recorded);
    assert!(
        video
            .windows(2)
            .all(|w| w[1].rtp_timestamp.wrapping_sub(w[0].rtp_timestamp) as i32 > 0),
        "RTP timestamps went backwards across the restart"
    );
    let mut steps: Vec<u32> = video[before + 5..]
        .windows(2)
        .map(|w| w[1].rtp_timestamp.wrapping_sub(w[0].rtp_timestamp))
        .collect();
    steps.sort_unstable();
    let median = steps[steps.len() / 2];
    assert!(
        median.abs_diff(6000) <= 600,
        "median RTP step {median} at 15 fps"
    );
    let audio: Vec<u32> = recorded
        .audio
        .iter()
        .filter(|a| a.kind == VoiceKind::Stream)
        .map(|a| a.rtp_timestamp)
        .collect();
    assert!(audio.len() > audio_before, "no audio after the reconfigure");
    assert!(
        audio.windows(2).all(|w| w[1].wrapping_sub(w[0]) as i32 > 0),
        "audio RTP timestamps went backwards across the restart"
    );
    assert_eq!(f.voice_joins(), [MOCK_VOICE_CHANNEL_ID]);
    assert_eq!(f.gateway_ops(gw::OP_STREAM_CREATE), 1);
    assert_eq!(f.gateway_ops(gw::OP_STREAM_DELETE), 0);

    // Two snapshots after the restart cover the 500 ms DAVE delay a fresh pump would wait.
    for _ in 0..2 {
        let SessionEvent::Health(h) = f.wait_event(|e| matches!(e, SessionEvent::Health(_))).await
        else {
            unreachable!()
        };
        assert_eq!(h.stats.dave_keyframe_requests, 1, "{h:?}");
        assert_eq!(h.stats.viewer_keyframe_requests, 0, "{h:?}");
    }
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_source_that_fails_to_restart_stops_the_session() {
    let mock = MockDiscord::start().await.unwrap();
    let (client, session, mut state) = idle_session(&mock).await;
    let (source, counters) = Counted::with(Behaviour::FailRestart);
    session
        .start(
            Some(MOCK_GUILD_ID.into()),
            MOCK_VOICE_CHANNEL_ID.into(),
            source,
            options(),
        )
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));
    wait_state(&mut state, |s| *s == SessionState::Streaming).await;
    let err = session.reconfigure(smaller_capture()).await.unwrap_err();
    assert!(
        matches!(err, SessionError::Stream(StreamError::Media(_))),
        "{err}"
    );
    wait_state(&mut state, |s| matches!(s, SessionState::Stopped { .. })).await;
    assert!(matches!(
        &*state.borrow(),
        SessionState::Stopped {
            reason: StopReason::SourceFailed { .. }
        }
    ));
    assert_eq!(counters.starts.load(Ordering::SeqCst), 2);
    assert!(timeout(T, session.stop()).await.unwrap().unwrap().is_some());
    close(client, session, mock).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reconfigure_while_reconnecting_applies_on_the_next_connection() {
    let mut options = options();
    options.backoff.base = Duration::from_millis(600);
    options.backoff.max = Duration::from_millis(600);
    let mut f = Fixture::streaming_with(options).await;
    assert!(
        f.mock
            .drop_voice(VoiceKind::Stream, Some(vo::CLOSE_SESSION_NO_LONGER_VALID))
    );
    wait_state(&mut f.state, |s| {
        matches!(s, SessionState::Reconnecting { .. })
    })
    .await;
    f.session.reconfigure(smaller_capture()).await.unwrap();
    wait_state(&mut f.state, |s| *s == SessionState::Streaming).await;
    f.mock
        .wait_for(T, |r| announced(r, 160, 15).then_some(()))
        .await
        .expect("op 12 with the new attributes on the new connection");
    f.video_flowing().await;
    assert_eq!(f.starts(), 2);
    assert_eq!(
        f.counters.configs.lock().unwrap().last(),
        Some(&smaller_capture())
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reconfigure_needs_a_running_session() {
    let mock = MockDiscord::start().await.unwrap();
    let (client, session, _) = idle_session(&mock).await;
    let err = session.reconfigure(smaller_capture()).await.unwrap_err();
    assert!(matches!(err, SessionError::NotRunning), "{err}");
    close(client, session, mock).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn update_video_on_a_closing_connection_is_kept_and_announced_on_the_reconnect() {
    let f = Fixture::streaming_source(options(), Behaviour::GateRestart).await;
    let session = &f.session;
    let reconfigure = session.reconfigure(smaller_capture());
    let parked = async {
        while f.counters.starts.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    };
    let (reconfigured, ()) = tokio::join!(reconfigure, async {
        timeout(T, parked).await.expect("the restart never began");
        // The actor is held in the restart, so it cannot see the stream connection close.
        assert!(
            f.mock
                .drop_voice(VoiceKind::Stream, Some(vo::CLOSE_SESSION_NO_LONGER_VALID))
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let update = session.update_video(240, 136, 30);
        let release = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            f.counters.restart_gate.add_permits(1);
        };
        let (updated, ()) = tokio::join!(update, release);
        assert!(updated.is_ok(), "{updated:?}");
    });
    reconfigured.unwrap();
    let last_width = |r: &dstreamy_mock_discord::Recorded| {
        r.ops
            .iter()
            .rev()
            .filter(|o| o.is(Some(VoiceKind::Stream), vo::OP_VIDEO))
            .find_map(|o| o.json()?["streams"][0]["max_resolution"]["width"].as_u64())
    };
    f.mock
        .wait_for(T, |r| (last_width(r) == Some(240)).then_some(()))
        .await
        .unwrap_or_else(|| panic!("last op 12 width {:?}", last_width(&f.mock.recorded())));
    // Going back to the earlier size after the refused one still reaches Discord.
    let mut state = f.session.state();
    wait_state(&mut state, |s| *s == SessionState::Streaming).await;
    f.session.update_video(160, 90, 15).await.unwrap();
    f.mock
        .wait_for(T, |r| (last_width(r) == Some(160)).then_some(()))
        .await
        .unwrap_or_else(|| panic!("last op 12 width {:?}", last_width(&f.mock.recorded())));
    f.finish().await;
}
