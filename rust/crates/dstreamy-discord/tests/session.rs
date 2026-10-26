use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use dstreamy_core::StreamHealth;
use dstreamy_discord::{
    Backoff, DiscordClient, GatewayOptions, Session, SessionError, SessionEvent, SessionOptions,
    SessionState, StopReason, StreamError, StreamOptions,
};
use dstreamy_media::{
    CaptureConfig, CaptureSource, CaptureTarget, FRAME_CHANNEL_CAPACITY, FrameReceiver, MediaError,
    TestPatternSource, VideoCodec,
};
use dstreamy_mock_discord::opcodes::gateway as gw;
use dstreamy_mock_discord::opcodes::voice as vo;
use dstreamy_mock_discord::{
    MOCK_GUILD_ID, MOCK_OTHER_VOICE_CHANNEL_ID, MOCK_TOKEN, MOCK_VOICE_CHANNEL_ID, MockDiscord,
    VoiceKind, rtc,
};
use secrecy::SecretString;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

const T: Duration = Duration::from_secs(20);

async fn client(mock: &MockDiscord) -> Arc<DiscordClient> {
    let mut opts = GatewayOptions::new(SecretString::from(MOCK_TOKEN)).with_url(mock.gateway_url());
    opts.reconnect_delay_min = Duration::from_millis(10);
    opts.reconnect_delay_max = Duration::from_millis(50);
    opts.invalid_session_delay = Duration::from_millis(50);
    let client = timeout(T, DiscordClient::connect(opts))
        .await
        .unwrap()
        .unwrap();
    {
        let mut streamer = client.streamer();
        streamer.set_ice_servers(vec![]);
        streamer.set_setting_engine(rtc::loopback_settings(false));
    }
    Arc::new(client)
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
    FailRestart,
    GateRestart,
    EndAfter(usize),
}

struct Counted {
    inner: TestPatternSource,
    counters: Arc<Counters>,
    behaviour: Behaviour,
    forward: Option<JoinHandle<()>>,
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
            Behaviour::FailRestart if self.counters.starts.load(Ordering::SeqCst) > 1 => {
                return Err(MediaError::AlreadyRunning);
            }
            Behaviour::GateRestart if self.counters.starts.load(Ordering::SeqCst) > 1 => {
                self.counters.restart_gate.acquire().await.unwrap().forget();
                self.inner.start(config).await?
            }
            _ => self.inner.start(config).await?,
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
        self.inner.stop().await
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
        let mock = MockDiscord::start().await.unwrap();
        let client = client(&mock).await;
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

async fn wait_state(state: &mut watch::Receiver<SessionState>, f: impl Fn(&SessionState) -> bool) {
    let waited = timeout(T, state.wait_for(|s| f(s)))
        .await
        .map(|r| r.is_ok());
    assert_eq!(waited, Ok(true), "state stuck at {:?}", *state.borrow());
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

#[tokio::test(flavor = "multi_thread")]
async fn gateway_drop_resumes_and_leaves_the_stream_alone() {
    let mut f = Fixture::streaming().await;
    f.states_seen();
    assert!(f.mock.drop_gateway(Some(4000)));
    f.mock
        .wait_for_op(None, gw::OP_RESUME, 0, T)
        .await
        .expect("gateway RESUME");
    f.video_flowing().await;
    sleep(Duration::from_millis(200)).await;

    assert_eq!(f.states_seen(), []);
    assert_eq!(f.gateway_ops(gw::OP_STREAM_CREATE), 1);
    assert_eq!(f.voice_joins(), [MOCK_VOICE_CHANNEL_ID]);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_session_rejoins_and_recovers() {
    let mut f = Fixture::streaming().await;
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

    assert_eq!(
        f.voice_joins(),
        [MOCK_VOICE_CHANNEL_ID, MOCK_VOICE_CHANNEL_ID]
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
    assert_eq!(counters.starts.load(Ordering::SeqCst), 0);
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
    mock.wait_for_op(None, gw::OP_STREAM_DELETE, 0, T)
        .await
        .expect("Go Live ended");
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
async fn a_rejected_gateway_stops_the_session() {
    let mut f = Fixture::streaming().await;
    assert!(f.mock.drop_gateway(Some(gw::CLOSE_AUTHENTICATION_FAILED)));
    wait_state(&mut f.state, |s| matches!(s, SessionState::Stopped { .. })).await;
    assert_eq!(
        *f.state.borrow(),
        SessionState::Stopped {
            reason: StopReason::GatewayClosed
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

fn stream_video(
    r: &dstreamy_mock_discord::Recorded,
) -> Vec<dstreamy_mock_discord::RecordedVideoFrame> {
    r.video
        .iter()
        .filter(|v| v.kind == VoiceKind::Stream)
        .cloned()
        .collect()
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
