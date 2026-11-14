use super::*;

fn engine_in(dir: &std::path::Path, gateway: &str, opts: Options) -> Arc<Engine> {
    let ctx = Ctx::new(
        dstreamy_core::ConfigPaths::in_dir(dir),
        gateway.to_owned(),
        None,
    );
    Engine::new(&ctx, opts)
}

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
fn live_health_encryption_is_unknown_until_negotiated_and_during_reconnect() {
    let h = LiveHealth::on_state(None, &SessionState::Streaming).unwrap();
    assert_eq!(h.e2ee, E2ee::Unknown);
    assert_eq!(h.dave_protocol_version, None);
    let mut s = snapshot(0, 0, 50);
    for (state, version) in [(E2ee::Encrypted, 1), (E2ee::Unencrypted, 0)] {
        s.stats.e2ee = state;
        s.stats.dave_protocol_version = version;
        let live = LiveHealth::on_health(Some(h), &s, false).unwrap();
        assert_eq!(live.e2ee, state);
        assert_eq!(live.dave_protocol_version, Some(version));
        let reconnecting =
            LiveHealth::on_state(Some(live), &SessionState::Reconnecting { attempt: 1 }).unwrap();
        assert_eq!(reconnecting.e2ee, E2ee::Unknown);
        assert_eq!(reconnecting.dave_protocol_version, None);
    }
}

#[test]
fn live_health_keeps_source_losses_separate_from_transport_losses() {
    let mut snapshot = snapshot(0, 7, 50);
    snapshot.stats.source_video_frames_dropped = 3;
    snapshot.stats.source_audio_frames_dropped = 5;
    let health = LiveHealth::on_health(None, &snapshot, false).unwrap();
    assert_eq!(health.dropped_video, 7);
    assert_eq!(health.source_dropped_video, 3);
    assert_eq!(health.source_dropped_audio, 5);
}

#[test]
fn live_health_counts_reconnects_and_resumes_per_stream() {
    let reconnecting = SessionState::Reconnecting { attempt: 1 };
    let mut h = LiveHealth::on_state(None, &SessionState::Connecting);
    assert_eq!(h, None, "no stream runs while connecting");
    h = LiveHealth::on_state(h, &SessionState::Streaming);
    assert_eq!(
        h,
        Some(LiveHealth {
            streamed: true,
            ..LiveHealth::default()
        })
    );
    h = LiveHealth::on_health(h, &snapshot(1, 4, 50), false);
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
    assert_eq!(LiveHealth::on_state(h, &SessionState::Connecting), None);
    assert_eq!(
        LiveHealth::on_state(None, &SessionState::Streaming),
        Some(LiveHealth {
            streamed: true,
            ..LiveHealth::default()
        }),
        "a new stream counts from zero"
    );
    let first = LiveHealth::on_state(None, &reconnecting).unwrap();
    assert!(first.reconnecting, "a failing first connect is not healthy");
    let again = LiveHealth::on_state(Some(first), &SessionState::Reconnecting { attempt: 2 });
    let streaming = LiveHealth::on_state(again, &SessionState::Streaming).unwrap();
    assert!(!streaming.reconnecting);
    assert_eq!(
        streaming.reconnects(),
        0,
        "first-connect retries never interrupted the live stream"
    );
}

#[test]
fn live_health_counts_intervals_with_too_little_audio_per_stream() {
    let mut h = LiveHealth::on_state(None, &SessionState::Streaming);
    h = LiveHealth::on_health(h, &snapshot(0, 0, 50), false);
    assert_eq!(h.unwrap().audio_dropouts, 0);
    h = LiveHealth::on_health(h, &snapshot(0, 0, AUDIO_DROPOUT_BELOW), false);
    assert_eq!(
        h.unwrap().audio_dropouts,
        0,
        "half the frames still count as flowing"
    );
    h = LiveHealth::on_health(h, &snapshot(0, 0, AUDIO_DROPOUT_BELOW - 1), false);
    h = LiveHealth::on_health(h, &snapshot(0, 0, 0), false);
    assert_eq!(h.unwrap().audio_dropouts, 2);
    h = LiveHealth::on_health(h, &snapshot(0, 0, 0), true);
    h = LiveHealth::on_health(h, &snapshot(0, 0, 5), true);
    assert_eq!(
        h.unwrap().audio_dropouts,
        2,
        "silence while the source is interrupted is not a dropout"
    );
    h = LiveHealth::on_state(h, &SessionState::Reconnecting { attempt: 1 });
    h = LiveHealth::on_state(h, &SessionState::Streaming);
    assert_eq!(h.unwrap().audio_dropouts, 2, "kept across a reconnect");
    let stopped = SessionState::Stopped {
        reason: dstreamy_discord::StopReason::Requested,
    };
    let next = LiveHealth::on_state(h, &stopped);
    assert_eq!(
        LiveHealth::on_state(next, &SessionState::Streaming).map(|h| h.audio_dropouts),
        Some(0),
        "a new stream counts from zero"
    );
}

#[tokio::test]
async fn video_only_stalls_do_not_hide_audio_dropouts() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_in(dir.path(), "ws://127.0.0.1:1", Options::default());
    let mut source = ExternalSource::new(ExternalOptions {
        stall: Duration::from_millis(500),
        ..ExternalOptions::default()
    })
    .unwrap();
    let handle = source.handle();
    let attachment = handle
        .attach(ProducerInfo {
            codec: dstreamy_media::VideoCodec::H264,
            width: 1280,
            height: 720,
            fps: 30,
        })
        .unwrap();
    let _frames = source.start(CaptureConfig::default()).await.unwrap();
    let external = engine.watch_source(handle.clone(), (1280, 720, 30));
    attachment
        .send(dstreamy_media::ProducerFrame::Video {
            data: vec![0, 0, 1, 0x65, 1],
            pts_us: 0,
            keyframe: true,
        })
        .unwrap();
    for i in 0..6 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        attachment
            .send(dstreamy_media::ProducerFrame::Audio {
                data: vec![0xf8, 0xff, 0xfe],
                pts_us: i * 100_000,
            })
            .unwrap();
    }
    tokio::task::yield_now().await;
    assert!(matches!(
        *handle.state().borrow(),
        SourceState::Interrupted {
            reason: InterruptReason::VideoStalled,
            ..
        }
    ));
    let health =
        LiveHealth::on_health(None, &snapshot(0, 0, 5), external.take_interrupted()).unwrap();
    assert_eq!(health.audio_dropouts, 1);
    drop(attachment);
    tokio::task::yield_now().await;
    assert!(external.take_interrupted());
    let health = LiveHealth::on_health(
        Some(health),
        &snapshot(0, 0, 0),
        external.take_interrupted(),
    )
    .unwrap();
    assert_eq!(health.audio_dropouts, 1);
    source.stop().await.unwrap();
    drop(external);
    engine.shutdown().await;
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
        run: 1,
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

mod mock_discord {
    use std::time::Duration;

    use dstreamy_core::ConfigPaths;
    use dstreamy_mock_discord::{
        MOCK_GUILD_ID, MOCK_OTHER_VOICE_CHANNEL_ID, MOCK_TOKEN, MOCK_VOICE_CHANNEL_ID, MockDiscord,
    };
    use tempfile::TempDir;
    use tokio::time::timeout;

    use super::*;

    const T: Duration = Duration::from_secs(20);

    enum Stub {
        Pending,
        Panic,
    }

    #[async_trait::async_trait]
    impl CaptureSource for Stub {
        async fn list_targets(
            &self,
        ) -> Result<Vec<dstreamy_media::CaptureTarget>, dstreamy_media::MediaError> {
            Ok(Vec::new())
        }
        async fn start(
            &mut self,
            _: CaptureConfig,
        ) -> Result<dstreamy_media::FrameReceiver, dstreamy_media::MediaError> {
            match self {
                Self::Pending => std::future::pending().await,
                Self::Panic => panic!("capture backend bug"),
            }
        }
        async fn stop(&mut self) -> Result<(), dstreamy_media::MediaError> {
            Ok(())
        }
        fn capture_drops(&self) -> dstreamy_media::CaptureDrops {
            dstreamy_media::CaptureDrops::default()
        }
        fn request_keyframe(&self) {}
    }

    async fn start_direct(conn: &Conn, source: Stub) {
        conn.session
            .start(
                Some(MOCK_GUILD_ID.into()),
                MOCK_VOICE_CHANNEL_ID.into(),
                Box::new(source),
                dstreamy_discord::SessionOptions::new(dstreamy_discord::StreamOptions::new(
                    CaptureConfig::default(),
                )),
            )
            .await
            .unwrap();
    }

    async fn status_json(f: &Fixture) -> Value {
        serde_json::to_value(f.engine.status().await).unwrap()
    }

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
            ConfigPaths::in_dir(dir.path())
                .save_token(MOCK_TOKEN)
                .unwrap();
            let opts = Options {
                include_platform: false,
                allow_external,
                ..Options::default()
            };
            let engine = engine_in(dir.path(), &mock.gateway_url(), opts);
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
            let gate = self.engine.test.start_gate.lock().await;
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

    /// A session-task watch for a forwarder fed by hand; its closing is ignored for a task still alive.
    fn no_task() -> watch::Receiver<SessionState> {
        watch::channel(SessionState::Idle).1
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_source_that_fails_after_the_portal_step_leaves_its_token_saved() {
        let f = Fixture::new().await;
        let config_file = ConfigPaths::in_dir(f.dir.path()).config_file;
        dstreamy_core::Config {
            target: Some(dstreamy_core::CaptureTarget::Portal {
                restore_token: Some("spent".into()),
            }),
            ..dstreamy_core::Config::default()
        }
        .save(&config_file)
        .unwrap();
        let p = StartParams {
            target: capture::FAILING_PORTAL_TARGET.into(),
            ..params()
        };
        // Held so the source's failure is handled while the start is still committing.
        let gate = f.engine.test.start_commit_gate.lock().await;
        let engine = f.engine.clone();
        let start = tokio::spawn(async move { engine.start(p, None).await });
        timeout(T, async {
            loop {
                let config = dstreamy_core::Config::load(&config_file).unwrap();
                if capture::restore_token(&config).as_deref() == Some(capture::FRESH_PORTAL_TOKEN) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the token the failed start reported was never saved");
        drop(gate);
        let _ = start.await.unwrap();
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
            require_e2ee: None,
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

    /// A gateway that accepts TCP and never answers, like a blackholed network or a stuck proxy.
    async fn silent_gateway() -> (String, JoinHandle<()>) {
        let (url, task, _) = counted_silent_gateway().await;
        (url, task)
    }

    async fn counted_silent_gateway() -> (String, JoinHandle<()>, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = accepted.clone();
        let task = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                held.push(socket);
            }
        });
        (url, task, accepted)
    }

    fn engine_at(url: &str, connect_budget: Duration) -> (TempDir, Arc<Engine>) {
        let dir = tempfile::tempdir().unwrap();
        ConfigPaths::in_dir(dir.path())
            .save_token(MOCK_TOKEN)
            .unwrap();
        let opts = Options {
            include_platform: false,
            allow_external: true,
            connect_budget,
            ..Options::default()
        };
        let engine = engine_in(dir.path(), url, opts);
        (dir, engine)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_silent_gateway_fails_the_command_within_the_budget() {
        let (url, _gateway) = silent_gateway().await;
        let (_dir, engine) = engine_at(&url, Duration::from_millis(500));
        let failed = timeout(Duration::from_secs(5), engine.handle(Command::Guilds))
            .await
            .expect("the login outlived its budget")
            .unwrap_err();
        assert_eq!(failed.code, "gateway");
        assert!(
            failed.message.contains("did not answer"),
            "{}",
            failed.message
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_gateway_fails_fast_with_the_cause() {
        let window = Duration::from_secs(1);
        let dir = tempfile::tempdir().unwrap();
        ConfigPaths::in_dir(dir.path())
            .save_token(MOCK_TOKEN)
            .unwrap();
        let engine = engine_in(
            dir.path(),
            "ws://127.0.0.1:1",
            Options {
                include_platform: false,
                first_login_window: window,
                ..Options::default()
            },
        );
        let began = std::time::Instant::now();
        let failed = timeout(Duration::from_secs(15), engine.handle(Command::Guilds))
            .await
            .expect("the first login outlived its window")
            .unwrap_err();
        assert!(began.elapsed() < Duration::from_secs(15));
        assert_eq!(failed.code, "gateway");
        assert!(
            failed.message.contains("can't reach Discord"),
            "{}",
            failed.message
        );
        // Windows retries a refused connect for about 2 s, longer than the window, so the cut attempt times out.
        let cause = failed.message.to_lowercase();
        assert!(
            cause.contains("refused") || (cfg!(windows) && cause.contains("timed out")),
            "{}",
            failed.message
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_and_stop_do_not_wait_for_a_login_in_progress() {
        let (url, _gateway) = silent_gateway().await;
        let (_dir, engine) = engine_at(&url, CONNECT_BUDGET);
        let start = tokio::spawn({
            let engine = engine.clone();
            async move { engine.start(params(), None).await }
        });
        let guilds = tokio::spawn({
            let engine = engine.clone();
            async move { engine.handle(Command::Guilds).await }
        });
        timeout(T, async {
            while engine.lifecycle.try_lock().is_ok() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let stopped = timeout(Duration::from_secs(2), engine.stop())
            .await
            .expect("stop waited for the login");
        assert_eq!(stopped.unwrap_err().code, "not_running");
        let start = timeout(Duration::from_secs(2), start)
            .await
            .expect("stop did not cancel the start's login")
            .unwrap();
        assert_eq!(start.unwrap_err().code, "cancelled");
        assert_eq!(
            timeout(Duration::from_secs(2), engine.stop())
                .await
                .expect("an idle stop waited for the login")
                .unwrap_err()
                .code,
            "not_running"
        );
        timeout(Duration::from_secs(2), engine.shutdown())
            .await
            .expect("shutdown waited for the login");
        let guilds = timeout(Duration::from_secs(2), guilds)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(guilds.unwrap_err().code, "shutting_down");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stop_cancels_the_starts_received_before_it_whichever_runs_first() {
        let f = Fixture::new().await;
        let start = Command::Start(params());
        let started = f.engine.admit(&start);
        let stop = Command::Stop;
        let stopped = f.engine.admit(&stop);
        assert_eq!(
            f.engine
                .handle_admitted(stop, None, stopped)
                .await
                .unwrap_err()
                .code,
            "not_running"
        );
        let failed = f
            .engine
            .handle_admitted(start, None, started)
            .await
            .unwrap_err();
        assert_eq!(failed.code, "cancelled");
        assert!(!f.engine.is_live());

        let stop = Command::Stop;
        let stopped = f.engine.admit(&stop);
        let start = Command::Start(params());
        let started = f.engine.admit(&start);
        f.engine
            .handle_admitted(start, None, started)
            .await
            .expect("a stop received before the start cancelled it");
        assert!(f.engine.is_live());
        f.engine.handle_admitted(stop, None, stopped).await.unwrap();
        f.engine.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn logins_with_one_token_never_run_at_the_same_time() {
        let (url, _gateway, accepted) = counted_silent_gateway().await;
        let (_dir, engine) = engine_at(&url, CONNECT_BUDGET);
        let set = tokio::spawn({
            let engine = engine.clone();
            async move {
                engine
                    .handle(Command::SetToken {
                        token: MOCK_TOKEN.into(),
                    })
                    .await
            }
        });
        timeout(T, async {
            while accepted.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let guilds = tokio::spawn({
            let engine = engine.clone();
            async move { engine.handle(Command::Guilds).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            1,
            "a second login with the same token ran alongside the first"
        );
        timeout(Duration::from_secs(2), engine.shutdown())
            .await
            .unwrap();
        let _ = set.await;
        let _ = guilds.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_start_queued_behind_a_login_stays_within_one_budget() {
        let budget = Duration::from_secs(2);
        let (url, _gateway) = silent_gateway().await;
        let (_dir, engine) = engine_at(&url, budget);
        let guilds = tokio::spawn({
            let engine = engine.clone();
            async move { engine.handle(Command::Guilds).await }
        });
        timeout(T, async {
            while engine.lifecycle.try_lock().is_ok() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        let sent = std::time::Instant::now();
        let failed = engine.handle(Command::Start(params())).await.unwrap_err();
        let took = sent.elapsed();
        assert!(
            took < budget + Duration::from_millis(500),
            "the start waited for lifecycle and then a whole budget: {took:?}"
        );
        assert!(
            matches!(failed.code, "gateway" | "busy"),
            "{}: {}",
            failed.code,
            failed.message
        );
        let _ = guilds.await;
        engine.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_slow_set_token_neither_holds_the_lifecycle_nor_blocks_shutdown() {
        let (url, _gateway) = silent_gateway().await;
        let (_dir, engine) = engine_at(&url, CONNECT_BUDGET);
        let set = tokio::spawn({
            let engine = engine.clone();
            async move {
                engine
                    .handle(Command::SetToken {
                        token: "other".into(),
                    })
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            engine.lifecycle.try_lock().is_ok(),
            "set_token holds lifecycle"
        );
        assert!(
            timeout(Duration::from_secs(2), engine.select_account("default"))
                .await
                .is_ok()
        );
        set.abort();
        timeout(Duration::from_secs(2), engine.shutdown())
            .await
            .expect("shutdown waited for set_token");

        let (_dir, engine) = engine_at(&url, CONNECT_BUDGET);
        let set = tokio::spawn({
            let engine = engine.clone();
            async move {
                engine
                    .handle(Command::SetToken {
                        token: "other".into(),
                    })
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        timeout(Duration::from_secs(2), engine.shutdown())
            .await
            .expect("shutdown waited for set_token");
        let set = timeout(Duration::from_secs(2), set).await.unwrap().unwrap();
        assert_eq!(set.unwrap_err().code, "shutting_down");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_closed_client_is_replaced_and_keeps_counting_identifies() {
        let f = Fixture::new().await;
        let first = f.engine.conn().await.unwrap();
        assert!(f.mock.drop_gateway(Some(4004)));
        timeout(T, async {
            while !first.client.gateway().is_closed() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the gateway client never closed");

        let second = f.engine.conn().await.unwrap();
        assert_ne!(first.seq, second.seq);
        assert!(!second.client.gateway().is_closed());
        assert_eq!(f.engine.identify_ledger(MOCK_TOKEN).recent(), 2);
        assert!(
            !lock(&f.engine.identify_ledgers)
                .keys()
                .any(|k| k.as_slice() == MOCK_TOKEN.as_bytes())
        );

        f.engine.logout_account("default").await.unwrap();
        assert!(lock(&f.engine.identify_ledgers).is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_and_status_include_source_losses_accrued_during_shutdown() {
        #[derive(Default)]
        struct Source {
            frames: Option<tokio::sync::mpsc::Sender<dstreamy_media::MediaFrame>>,
            drops: dstreamy_media::CaptureDrops,
            self_stops: bool,
        }
        #[async_trait::async_trait]
        impl CaptureSource for Source {
            async fn list_targets(
                &self,
            ) -> Result<Vec<dstreamy_media::CaptureTarget>, dstreamy_media::MediaError>
            {
                Ok(Vec::new())
            }
            async fn start(
                &mut self,
                _: CaptureConfig,
            ) -> Result<dstreamy_media::FrameReceiver, dstreamy_media::MediaError> {
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                self.frames = (!self.self_stops).then_some(tx);
                self.drops = dstreamy_media::CaptureDrops::default();
                Ok(rx)
            }
            async fn stop(&mut self) -> Result<(), dstreamy_media::MediaError> {
                self.frames = None;
                self.drops = dstreamy_media::CaptureDrops {
                    video_frames: 17,
                    audio_frames: 19,
                };
                Ok(())
            }
            fn capture_drops(&self) -> dstreamy_media::CaptureDrops {
                self.drops
            }
            fn request_keyframe(&self) {}
        }
        for (self_stops, delayed_health) in [
            (false, None),
            (false, Some(false)),
            (false, Some(true)),
            (true, None),
        ] {
            let f = Fixture::new().await;
            let conn = f.engine.conn().await.unwrap();
            let mut options = dstreamy_discord::SessionOptions::new(
                dstreamy_discord::StreamOptions::new(CaptureConfig::default()),
            );
            if !self_stops {
                options.health_interval = if delayed_health == Some(true) {
                    Duration::from_millis(20)
                } else {
                    Duration::from_secs(3600)
                };
            }
            conn.session
                .start(
                    Some(MOCK_GUILD_ID.into()),
                    MOCK_VOICE_CHANNEL_ID.into(),
                    Box::new(Source {
                        self_stops,
                        ..Source::default()
                    }),
                    options,
                )
                .await
                .unwrap();
            let stats = if self_stops {
                f.wait_state(|s| matches!(s, SessionState::Stopped { .. }))
                    .await;
                assert!(f.engine.catch_up(&conn).await);
                status_json(&f).await["stats"].clone()
            } else {
                f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
                assert!(f.engine.catch_up(&conn).await);
                let mut events = conn.session.subscribe();
                let gate = if delayed_health.is_some() {
                    Some(f.engine.test.forward_gate.lock().await)
                } else {
                    None
                };
                if delayed_health == Some(true) {
                    timeout(T, async {
                        while !matches!(events.recv().await.unwrap(), SessionEvent::Health(_)) {}
                    })
                    .await
                    .unwrap();
                }
                let stopped = f.engine.stop().await;
                if delayed_health.is_some() {
                    assert_eq!(stopped.unwrap_err().code, "busy");
                    assert!(!lock(&f.engine.last_stats).terminal_consumed);
                    drop(gate);
                    assert!(f.engine.catch_up(&conn).await);
                    assert!(!lock(&f.engine.last_stats).terminal_consumed);
                    status_json(&f).await["stats"].clone()
                } else {
                    let reply = stopped.unwrap();
                    assert_eq!(status_json(&f).await["stats"], Value::Null);
                    drop(gate);
                    assert!(f.engine.catch_up(&conn).await);
                    assert_eq!(status_json(&f).await["stats"], Value::Null);
                    reply
                }
            };
            assert_eq!(stats["source_video_frames_dropped"], 17);
            assert_eq!(stats["source_audio_frames_dropped"], 19);
            if delayed_health.is_none() && !self_stops {
                f.engine.start(external(Some(0)), None).await.unwrap();
                timeout(T, async {
                    while conn.session.terminal_stats().is_none() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert!(f.engine.catch_up(&conn).await);
                assert!(
                    status_json(&f).await["stats"].is_object(),
                    "admitted start did not reset terminal consumption"
                );
            }
            f.engine.shutdown().await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stop_during_a_held_source_start_succeeds() {
        let f = Fixture::new().await;
        let conn = f.engine.conn().await.unwrap();
        start_direct(&conn, Stub::Pending).await;
        f.wait_state(live_state).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(f.engine.is_live(), "the held start ended by itself");
        let stopped = timeout(T, f.engine.stop()).await.unwrap();
        assert!(stopped.is_ok(), "{stopped:?}");
        assert!(!f.engine.is_live());
        assert!(lock(&f.engine.running).is_none());
        assert_eq!(
            f.engine.stop().await.unwrap_err().code,
            "not_running",
            "a second stop has nothing to stop"
        );
        drop(conn);
        f.engine.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_session_driven_directly_reports_its_task_ending() {
        let f = Fixture::new().await;
        let conn = f.engine.conn().await.unwrap();
        let mut events = crate::session::SessionEvents::new(&conn.session);
        start_direct(&conn, Stub::Panic).await;
        let stopped = timeout(T, async {
            loop {
                if let Ok(SessionEvent::State {
                    state: state @ SessionState::Stopped { .. },
                }) = events.recv().await
                {
                    return state;
                }
            }
        })
        .await
        .expect("stream and the TUI would wait forever");
        assert_eq!(
            stopped,
            SessionState::Stopped {
                reason: StopReason::TaskEnded
            }
        );
        assert!(crate::session::has_stopped(&conn.session));
        drop(conn);
        f.engine.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_session_task_that_dies_is_stopped_and_a_new_start_works() {
        let f = Fixture::new().await;
        let mut published = f.engine.subscribe_lossless();
        let conn = f.engine.conn().await.unwrap();
        let backlog = f.engine.test.forward_gate.lock().await;
        start_direct(&conn, Stub::Panic).await;
        timeout(T, async {
            while conn.session.state().has_changed().is_ok() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the session task never died");
        assert!(
            live_state(&conn.session.state().borrow()),
            "the dead task left a live state"
        );
        drop(conn);
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(backlog);
        let seen = published_until(&mut published, |s| {
            matches!(s, SessionState::Stopped { .. })
        })
        .await;
        let Some(EngineEvent::Session(SessionEvent::State { state })) = seen.last() else {
            panic!("{seen:?}");
        };
        assert_eq!(
            *state,
            SessionState::Stopped {
                reason: dstreamy_discord::StopReason::TaskEnded
            }
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        while let Ok(late) = published.try_recv() {
            assert!(
                !matches!(late, EngineEvent::Session(_)),
                "a dead run's event came after its stop: {late:?}"
            );
        }
        assert_eq!(*f.engine.health().borrow(), None);
        let status = status_json(&f).await;
        assert_eq!(status["session"]["state"], "stopped", "{status}");
        assert_eq!(
            status["session"]["reason"]["reason"], "task_ended",
            "{status}"
        );
        assert!(status["started_by"].is_null());
        assert!(!f.engine.is_live());
        assert_eq!(
            f.engine.handle(Command::Stop).await.unwrap_err().code,
            "not_running"
        );
        f.engine.start(params(), None).await.unwrap();
        f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
        f.engine.stop().await.unwrap();
        f.engine.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_session_that_gives_up_keeps_its_cause_until_the_next_start() {
        let f = Fixture::new().await;
        f.mock.close_voice_on_stream_create();
        let conn = f.engine.conn().await.unwrap();
        let mut options = dstreamy_discord::SessionOptions::new(
            dstreamy_discord::StreamOptions::new(CaptureConfig {
                width: 320,
                height: 180,
                fps: 15,
                ..CaptureConfig::default()
            }),
        );
        options.backoff = dstreamy_discord::Backoff {
            base: Duration::from_millis(20),
            max: Duration::from_millis(50),
            max_attempts: 2,
        };
        conn.session
            .start(
                Some(MOCK_GUILD_ID.into()),
                MOCK_VOICE_CHANNEL_ID.into(),
                Box::new(dstreamy_media::TestPatternSource::new()),
                options,
            )
            .await
            .unwrap();
        f.wait_state(|s| {
            matches!(
                s,
                SessionState::Stopped {
                    reason: dstreamy_discord::StopReason::GaveUp { .. }
                }
            )
        })
        .await;
        assert!(f.engine.catch_up(&conn).await);
        let status = status_json(&f).await;
        assert!(
            status["connect_error"]
                .as_str()
                .is_some_and(|e| e.contains("closed")),
            "the cause went with the stop: {status}"
        );
        drop(conn);
        f.engine.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stop_that_finds_the_run_stopped_on_its_own_keeps_its_reason() {
        let f = Fixture::new().await;
        f.engine.start(external(Some(30)), None).await.unwrap();
        f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
        let (handle, _) = f.engine.external().unwrap();
        let attachment = handle
            .attach(ProducerInfo {
                codec: dstreamy_media::VideoCodec::H264,
                width: 320,
                height: 180,
                fps: 15,
            })
            .unwrap();
        let conn = f.engine.current().unwrap();
        assert!(f.engine.catch_up(&conn).await);
        let gate = f.engine.test.forward_gate.lock().await;
        let parked = f.engine.test.stop_gate.lock().await;
        let stopping = tokio::spawn({
            let engine = f.engine.clone();
            async move { engine.stop().await }
        });
        timeout(T, async {
            while !f.engine.test.stop_entered.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            f.mock
                .voice_state_from_other_session(MOCK_GUILD_ID, Some(MOCK_VOICE_CHANNEL_ID))
        );
        f.wait_state(|s| matches!(s, SessionState::Stopped { .. }))
            .await;
        drop(parked);
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(gate);
        let _ = timeout(T, stopping).await.unwrap().unwrap();
        assert!(f.engine.catch_up(&conn).await);
        match attachment.close_reason() {
            Some(dstreamy_media::CloseReason::Stopped { code, .. }) => {
                assert_eq!(code, "voice_taken_over");
            }
            other => panic!("{other:?}"),
        }
        drop(conn);
        f.engine.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failing_first_connect_reports_reconnecting_with_its_cause() {
        let f = Fixture::new().await;
        f.mock.close_voice_on_stream_create();
        let mut health = f.engine.health();
        let mut published = f.engine.subscribe_lossless();
        f.engine.start(params(), None).await.unwrap();
        published_until(&mut published, |s| {
            matches!(s, SessionState::Reconnecting { .. })
        })
        .await;
        let streamed = std::sync::atomic::AtomicBool::new(false);
        timeout(
            T,
            health.wait_for(|h| {
                streamed.fetch_or(
                    h.is_some_and(|h| h.metrics.is_some()),
                    std::sync::atomic::Ordering::Relaxed,
                );
                h.is_some_and(|h| h.reconnecting)
            }),
        )
        .await
        .expect("health never showed the failing connect")
        .unwrap();
        assert!(!streamed.load(std::sync::atomic::Ordering::Relaxed));
        let status = status_json(&f).await;
        assert!(
            status["connect_error"]
                .as_str()
                .is_some_and(|e| e.contains("closed")),
            "{status}"
        );
        f.engine.stop().await.unwrap();
        assert_eq!(*f.engine.health().borrow(), None);
        assert!(status_json(&f).await["connect_error"].is_null());
        f.engine.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_is_not_acknowledged_before_its_terminal_event_is_published() {
        let f = Fixture::new().await;
        f.engine.start(params(), None).await.unwrap();
        f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
        let conn = f.engine.current().unwrap();
        assert!(f.engine.catch_up(&conn).await);
        let gate = f.engine.test.forward_gate.lock().await;
        let mut published = f.engine.subscribe_lossless();
        let stopped = f.engine.stop().await;
        assert!(
            stopped.is_err(),
            "successful Stop acknowledgement preceded its terminal event"
        );
        assert!(!conn.live());
        assert!(!lock(&f.engine.last_stats).terminal_consumed);
        assert!(
            !std::iter::from_fn(|| published.try_recv().ok()).any(|event| matches!(
                event,
                EngineEvent::Session(SessionEvent::State {
                    state: SessionState::Stopped { .. }
                })
            ))
        );
        drop(gate);
        assert!(f.engine.catch_up(&conn).await);
        let terminal: Vec<_> = std::iter::from_fn(|| published.try_recv().ok()).collect();
        assert!(terminal.iter().any(|event| matches!(
            event,
            EngineEvent::Session(SessionEvent::State {
                state: SessionState::Stopped { .. }
            })
        )));
        assert_eq!(f.engine.stop().await.unwrap_err().code, "not_running");
        f.engine.start(params(), None).await.unwrap();
        f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
        assert!(f.engine.catch_up(&conn).await);
        let next: Vec<_> = std::iter::from_fn(|| published.try_recv().ok()).collect();
        assert!(!next.iter().any(|event| matches!(
            event,
            EngineEvent::Session(SessionEvent::State {
                state: SessionState::Stopped { .. }
            })
        )));
        f.engine.stop().await.unwrap();
        f.engine.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_rejects_prior_terminal_delivery_instead_of_starting_a_cancelled_consumer() {
        use dstreamy_mock_discord::opcodes::gateway::OP_STREAM_CREATE;
        for (release_during_start, self_stops) in [(true, false), (false, false), (true, true)] {
            let f = Fixture::new().await;
            let mut published = f.engine.subscribe_lossless();
            f.engine
                .start(
                    if self_stops {
                        external(Some(1))
                    } else {
                        params()
                    },
                    None,
                )
                .await
                .unwrap();
            published_until(&mut published, |s| matches!(s, SessionState::Streaming)).await;
            let conn = f.engine.current().unwrap();
            assert!(f.engine.catch_up(&conn).await);
            while published.try_recv().is_ok() {}
            let gate = f.engine.test.forward_gate.lock().await;
            if self_stops {
                f.wait_state(|s| matches!(s, SessionState::Stopped { .. }))
                    .await;
            } else {
                assert!(conn.session.stop().await.unwrap().is_some());
            }
            let before = f
                .mock
                .received_ops()
                .iter()
                .filter(|op| op.is(None, OP_STREAM_CREATE))
                .count();
            let requested = f.engine.test.catch_ups_requested.load(Ordering::SeqCst);
            let engine = f.engine.clone();
            let attempted = tokio::spawn(async move { engine.start(params(), None).await });
            timeout(T, async {
                while f.engine.test.catch_ups_requested.load(Ordering::SeqCst) == requested {
                    assert!(!attempted.is_finished());
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let gate = if release_during_start {
                drop(gate);
                None
            } else {
                Some(gate)
            };
            let result = timeout(T, attempted).await.unwrap().unwrap();
            if result.is_ok() {
                f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
            }
            let after = f
                .mock
                .received_ops()
                .iter()
                .filter(|op| op.is(None, OP_STREAM_CREATE))
                .count();
            assert!(
                result.is_err(),
                "accepted a new remote stream while old terminal delivery was pending; released={release_during_start}, creates={before}->{after}"
            );
            assert_eq!(after, before, "rejected Start created a remote stream");
            assert!(!conn.live());
            drop(gate);
            assert!(f.engine.catch_up(&conn).await);
            let terminal: Vec<_> = std::iter::from_fn(|| published.try_recv().ok()).collect();
            assert!(terminal.iter().any(|event| matches!(
                event,
                EngineEvent::Session(SessionEvent::State {
                    state: SessionState::Stopped { .. }
                })
            )));
            assert!(!terminal.iter().any(|event| matches!(
                event,
                EngineEvent::Session(SessionEvent::State {
                    state: SessionState::Connecting | SessionState::Streaming
                })
            )));
            f.engine.start(params(), None).await.unwrap();
            let mut next =
                published_until(&mut published, |s| matches!(s, SessionState::Streaming)).await;
            assert!(f.engine.catch_up(&f.engine.current().unwrap()).await);
            next.extend(std::iter::from_fn(|| published.try_recv().ok()));
            assert!(!next.iter().any(|event| matches!(
                event,
                EngineEvent::Session(SessionEvent::State {
                    state: SessionState::Stopped { .. }
                })
            )));
            f.engine.stop().await.unwrap();
            f.engine.shutdown().await;
        }
    }

    /// The session updates its state watch before it broadcasts the event, so tests wait for the publication.
    async fn published_until(
        rx: &mut mpsc::Receiver<EngineEvent>,
        done: impl Fn(&SessionState) -> bool,
    ) -> Vec<EngineEvent> {
        let mut seen = Vec::new();
        timeout(T, async {
            loop {
                let event = rx.recv().await.expect("engine events closed");
                let last = matches!(&event, EngineEvent::Session(SessionEvent::State { state }) if done(state));
                seen.push(event);
                if last {
                    break;
                }
            }
        })
        .await
        .expect("the state was never published");
        seen
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_observes_terminal_delivery_before_it_acquires_lifecycle() {
        use dstreamy_mock_discord::opcodes::gateway::OP_STREAM_CREATE;
        let f = Fixture::new().await;
        f.engine.start(params(), None).await.unwrap();
        f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
        let conn = f.engine.current().unwrap();
        assert!(f.engine.catch_up(&conn).await);
        let forward = f.engine.test.forward_gate.lock().await;
        let lifecycle = f.engine.lifecycle.lock().await;
        let mut published = f.engine.subscribe_lossless();
        conn.session.stop().await.unwrap();
        let attempted = f.engine.start(params(), None);
        tokio::pin!(attempted);
        assert!(futures_util::poll!(&mut attempted).is_pending());
        drop(forward);
        assert!(f.engine.catch_up(&conn).await);
        assert!(
            std::iter::from_fn(|| published.try_recv().ok()).any(|event| matches!(
                event,
                EngineEvent::Session(SessionEvent::State {
                    state: SessionState::Stopped { .. }
                })
            ))
        );
        drop(lifecycle);
        assert_eq!(attempted.await.unwrap_err().code, "busy");
        assert!(!conn.live());
        assert_eq!(
            f.mock
                .received_ops()
                .iter()
                .filter(|op| op.is(None, OP_STREAM_CREATE))
                .count(),
            1
        );
        f.engine.start(params(), None).await.unwrap();
        f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
        f.engine.stop().await.unwrap();
        f.engine.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_forwarder_held_across_connection_replacement_cannot_publish_old_terminal_state() {
        let f = Fixture::new().await;
        f.engine.start(params(), None).await.unwrap();
        f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
        let old = f.engine.current().unwrap();
        assert!(f.engine.catch_up(&old).await);
        old.forwarder.abort();
        let portal = f.engine.portal.clone();
        let (locked, is_locked) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let holding = std::thread::spawn(move || {
            let _portal = lock(&portal);
            locked.send(()).unwrap();
            let _ = released.recv_timeout(3 * T);
        });
        is_locked.recv_timeout(T).unwrap();
        let receiver = old.session.subscribe();
        let (catch_up, catch_ups) = mpsc::unbounded_channel();
        let forwarding = tokio::spawn(f.engine.clone().forward(
            receiver,
            catch_ups,
            no_task(),
            old.seq,
        ));
        assert!(old.session.stop().await.unwrap().is_some());
        timeout(T, async {
            while f.engine.health.borrow().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let gate = f.engine.test.forward_gate.lock().await;
        release.send(()).unwrap();
        holding.join().unwrap();
        f.engine.stale_token.store(true, Ordering::Release);
        let current = f.engine.conn().await.unwrap();
        assert_ne!(current.seq, old.seq);
        let mut published = f.engine.subscribe_lossless();
        f.engine.start(params(), None).await.unwrap();
        f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
        drop(gate);
        let (done, processed) = oneshot::channel();
        catch_up.send(done).unwrap();
        timeout(T, processed).await.unwrap().unwrap();
        forwarding.abort();
        let _ = forwarding.await;
        let mut events =
            published_until(&mut published, |s| matches!(s, SessionState::Streaming)).await;
        assert!(f.engine.catch_up(&current).await);
        events.extend(std::iter::from_fn(|| published.try_recv().ok()));
        let states: Vec<_> = events
            .into_iter()
            .filter_map(|event| match event {
                EngineEvent::Session(SessionEvent::State { state }) => Some(state),
                _ => None,
            })
            .collect();
        assert!(
            !states
                .iter()
                .any(|state| matches!(state, SessionState::Stopped { .. })),
            "superseded terminal reached current consumer: {states:?}"
        );
        assert!(states.contains(&SessionState::Streaming));
        assert!(f.engine.health.borrow().is_some());
        f.engine.stop().await.unwrap();
        f.engine.shutdown().await;
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

    async fn status_place(f: &Fixture) -> [Value; 4] {
        let status = status_json(f).await;
        ["guild_id", "channel_id", "channel_name", "target"].map(|k| status["session"][k].clone())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn external_start_rejects_explicit_unsupported_transforms_before_connecting() {
        let f = Fixture::new().await;
        let crop =
            serde_json::from_value(json!({"x": 0, "y": 0, "width": 16, "height": 16})).unwrap();
        for p in [
            StartParams {
                crop: Some(crop),
                ..external(None)
            },
            StartParams {
                gain: Some(0.5),
                ..external(None)
            },
            StartParams {
                audio_source: Some("device".into()),
                ..external(None)
            },
        ] {
            let result = f.engine.start(p, None).await;
            assert!(
                matches!(
                    result,
                    Err(Failure {
                        code: "invalid_params",
                        ..
                    })
                ),
                "{result:?}"
            );
            assert!(
                f.mock.received_ops().is_empty(),
                "validation connected to Discord"
            );
        }
        f.engine.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn external_start_ignores_native_capture_defaults() {
        let f = Fixture::new().await;
        std::fs::write(
            f.dir.path().join("config.toml"),
            "audio_source = 'native-device'\n[stream]\naudio_gain = 0.5\n",
        )
        .unwrap();
        f.engine
            .start(
                StartParams {
                    gain: Some(1.0),
                    ..external(None)
                },
                None,
            )
            .await
            .unwrap();
        {
            let running = lock(&f.engine.running);
            let applied = running.as_ref().unwrap();
            assert_eq!(applied.stream.audio_gain, 1.0);
            assert_eq!(applied.audio_source, None);
        }
        f.engine.shutdown().await;
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
            while status_place(&f).await[1] != json!(MOCK_OTHER_VOICE_CHANNEL_ID) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("status did not update after the channel move");
        assert_eq!(
            status_place(&f).await,
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

    async fn status_viewers(f: &Fixture) -> Value {
        status_json(f).await["stats"]["viewers"].clone()
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
            assert_eq!(status_viewers(&f).await, json!(2));

            f.mock.ignore_stream_creates(usize::MAX);
            assert!(trigger(&f.mock));
            f.wait_state(|s| !matches!(s, SessionState::Streaming))
                .await;
            wait_viewers(&mut producer, None).await;
            assert_eq!(status_viewers(&f).await, Value::Null);
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
        timeout(T, f.engine.clone().forward(rx, catch_ups, no_task(), conn))
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
            status_place(&f).await[1],
            json!(MOCK_VOICE_CHANNEL_ID),
            "another connection's move"
        );
        forward_events(&f, conn, &[moved_to_gaming()]).await;
        assert_eq!(
            status_place(&f).await[1],
            json!(MOCK_OTHER_VOICE_CHANNEL_ID)
        );
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
        let forwarder = tokio::spawn(f.engine.clone().forward(rx, catch_ups, no_task(), u64::MAX));
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

    /// Runs `first` in Gaming with the forwarder parked, moves it to General, starts `second` and returns its moves.
    async fn moves_after_a_move_queued_behind(
        f: &Fixture,
        first: StartParams,
        second: StartParams,
    ) -> Vec<Place> {
        let conn = f.engine.conn().await.unwrap();
        let mut probe = conn.session.subscribe();
        assert!(f.engine.catch_up(&conn).await);
        let gate = f.engine.test.forward_gate.lock().await;
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
                .test
                .catch_ups_requested
                .load(std::sync::atomic::Ordering::SeqCst)
        };
        let before = requested();
        let engine = f.engine.clone();
        let restart = tokio::spawn(async move {
            if !self_stops {
                engine.stop().await.unwrap();
            }
            let retry = if second.target == EXTERNAL_TARGET_ID {
                gaming(external(None))
            } else {
                gaming(params())
            };
            let attempted = engine.start(second, None).await;
            if self_stops {
                assert_eq!(attempted.unwrap_err().code, "busy");
                engine.start(retry, None).await.unwrap();
            } else {
                attempted.unwrap();
            }
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
        assert_eq!(status_place(f).await[1], json!(MOCK_OTHER_VOICE_CHANNEL_ID));
        lock(&f.engine.test.moves_applied)
            .iter()
            .filter(|p| p.target == second_target)
            .cloned()
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stopped_runs_queued_move_never_moves_the_next_start() {
        let f = Fixture::new().await;
        let moves =
            moves_after_a_move_queued_behind(&f, gaming(params()), gaming(external(None))).await;
        assert!(moves.is_empty(), "{moves:?}");
        f.engine.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_self_stopped_runs_queued_move_never_moves_the_next_start() {
        let f = Fixture::new().await;
        let moves =
            moves_after_a_move_queued_behind(&f, gaming(external(Some(1))), gaming(params())).await;
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
        assert!(status_place(&f).await.iter().all(Value::is_null));
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
            status_place(&f).await.iter().all(Value::is_null),
            "{:?}",
            status_place(&f).await
        );
        f.engine.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_names_the_guild_channel_and_target_of_the_live_session() {
        let f = Fixture::new().await;
        f.engine.start(params(), None).await.unwrap();
        f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
        assert_eq!(
            status_place(&f).await,
            [
                json!(MOCK_GUILD_ID),
                json!(MOCK_VOICE_CHANNEL_ID),
                json!("General"),
                json!("test-pattern")
            ]
        );
        assert_eq!(status_json(&f).await["session"]["state"], "streaming");
        f.engine.stop().await.unwrap();
        let gone = status_json(&f).await;
        assert!(
            status_place(&f).await.iter().all(Value::is_null),
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
                .test
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
        let gate = f.engine.test.stop_gate.lock().await;
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
                .forward(rx, mpsc::unbounded_channel().1, no_task(), seq),
        )
        .await
        .unwrap();
        assert!(f.lock_is_free(), "a lagged forwarder kept the lock");
        f.engine.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_lagged_forwarder_publishes_the_terminal_state_it_skipped() {
        let f = Fixture::new().await;
        f.engine.start(params(), None).await.unwrap();
        f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
        let conn = f.engine.current().unwrap();
        assert!(f.engine.catch_up(&conn).await);
        assert!(f.engine.health().borrow().is_some());
        conn.forwarder.abort();
        assert!(
            f.mock
                .voice_state_from_other_session(MOCK_GUILD_ID, Some(MOCK_VOICE_CHANNEL_ID))
        );
        f.wait_state(|s| matches!(s, SessionState::Stopped { .. }))
            .await;
        let mut out = f.engine.subscribe_lossless();

        let (tx, rx) = broadcast::channel(3);
        for event in [
            SessionEvent::State {
                state: SessionState::Streaming,
            },
            SessionEvent::Health(super::snapshot(0, 0, 50)),
            SessionEvent::Resumed,
            SessionEvent::State {
                state: SessionState::Reconnecting { attempt: 1 },
            },
            SessionEvent::Health(super::snapshot(1, 0, 50)),
            SessionEvent::Resumed,
        ] {
            tx.send(event).unwrap();
        }
        drop(tx);
        timeout(
            T,
            f.engine
                .clone()
                .forward(rx, mpsc::unbounded_channel().1, no_task(), conn.seq),
        )
        .await
        .unwrap();
        let mut states = Vec::new();
        while let Ok(event) = out.try_recv() {
            if let EngineEvent::Session(SessionEvent::State { state }) = event {
                states.push(state);
            }
        }
        assert!(
            matches!(
                states.as_slice(),
                [
                    SessionState::Reconnecting { .. },
                    SessionState::Stopped { .. }
                ]
            ),
            "the current state must follow the retained backlog, once: {states:?}"
        );
        assert_eq!(*f.engine.health().borrow(), None);
        let stats = serde_json::to_value(f.engine.status().await.stats).unwrap();
        assert_eq!(
            stats,
            serde_json::to_value(conn.session.terminal_stats()).unwrap()
        );
        assert!(f.lock_is_free());
        drop(conn);
        f.engine.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_state_queued_right_after_a_resync_is_not_published_twice() {
        let f = Fixture::new().await;
        f.engine.start(params(), None).await.unwrap();
        f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
        let conn = f.engine.current().unwrap();
        assert!(f.engine.catch_up(&conn).await);
        conn.forwarder.abort();
        assert!(
            f.mock
                .voice_state_from_other_session(MOCK_GUILD_ID, Some(MOCK_VOICE_CHANNEL_ID))
        );
        f.wait_state(|s| matches!(s, SessionState::Stopped { .. }))
            .await;
        let stopped = conn.session.state().borrow().clone();
        let mut out = f.engine.subscribe_lossless();

        let (tx, rx) = broadcast::channel(1);
        for _ in 0..3 {
            tx.send(SessionEvent::Resumed).unwrap();
        }
        let forwarder = tokio::spawn(f.engine.clone().forward(
            rx,
            mpsc::unbounded_channel().1,
            no_task(),
            conn.seq,
        ));
        timeout(T, async {
            loop {
                if let Some(EngineEvent::Session(SessionEvent::State { .. })) = out.recv().await {
                    break;
                }
            }
        })
        .await
        .expect("the resync published nothing");
        tx.send(SessionEvent::State { state: stopped }).unwrap();
        drop(tx);
        timeout(T, forwarder).await.unwrap().unwrap();
        while let Ok(event) = out.try_recv() {
            assert!(
                !matches!(event, EngineEvent::Session(SessionEvent::State { .. })),
                "the resynced state was published again: {event:?}"
            );
        }
        drop(conn);
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
        let conn = f.engine.current().unwrap();
        let mut published = f.engine.subscribe_lossless();
        assert!(
            f.mock
                .voice_state_from_other_session(MOCK_GUILD_ID, Some(MOCK_VOICE_CHANNEL_ID))
        );
        published_until(&mut published, |s| {
            matches!(s, SessionState::Stopped { .. })
        })
        .await;
        drop(published);
        assert!(f.engine.catch_up(&conn).await);
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

        let gate = f.engine.test.stop_gate.lock().await;
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
    async fn an_update_queued_across_a_stop_and_start_does_not_touch_the_next_run() {
        let f = Fixture::new().await;
        f.engine.start(params(), None).await.unwrap();
        f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
        let gate = f.engine.test.update_gate.lock().await;
        let update = f.engine.update(UpdateParams {
            crop: None,
            fps: Some(Fps::Fps30),
            resolution: None,
            bitrate_kbps: None,
        });
        tokio::pin!(update);
        assert!(futures_util::poll!(&mut update).is_pending());
        f.engine.stop().await.unwrap();
        f.engine.start(params(), None).await.unwrap();
        f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
        drop(gate);
        let updated = timeout(T, &mut update).await.unwrap();
        let fps = lock(&f.engine.running).as_ref().unwrap().stream.fps;
        f.engine.shutdown().await;
        assert_eq!(
            updated.as_ref().map_err(|e| e.code),
            Err("not_running"),
            "old stream update succeeded on the next run: {updated:?}, next fps={fps:?}"
        );
        assert_eq!(fps, Fps::Fps15, "old stream update changed next run");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_start_waits_for_an_applying_update_that_a_stop_outran() {
        let f = Fixture::new().await;
        f.engine.start(params(), None).await.unwrap();
        f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
        let gate = f.engine.test.update_apply_gate.lock().await;
        let engine = f.engine.clone();
        let update = tokio::spawn(async move {
            engine
                .update(UpdateParams {
                    crop: None,
                    fps: Some(Fps::Fps30),
                    resolution: None,
                    bitrate_kbps: None,
                })
                .await
        });
        timeout(T, async {
            while f.engine.updating.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("update never took updating");
        f.engine.stop().await.unwrap();
        assert!(lock(&f.engine.running).is_none());
        let before = f.mock.received_ops().len();
        let engine = f.engine.clone();
        let mut start = tokio::spawn(async move { engine.start(params(), None).await });
        let early = timeout(Duration::from_millis(200), &mut start).await;
        let ran_early = early.is_ok();
        drop(gate);
        let updated = timeout(T, update).await.unwrap().unwrap();
        let started = match early {
            Ok(started) => started,
            Err(_) => timeout(T, start).await.unwrap(),
        };
        assert!(started.unwrap().is_ok());
        f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
        let fps = lock(&f.engine.running).as_ref().unwrap().stream.fps;
        let announced = announced_framerate_since(&f, before).await;
        f.engine.shutdown().await;
        assert_eq!(fps, Fps::Fps15);
        assert_eq!(
            announced,
            Some(15),
            "the session applied the old run's update"
        );
        assert_eq!(updated.map_err(|e| e.code), Err("not_running"));
        assert!(!ran_early, "a start ran while an update was applying");
    }

    async fn announced_framerate_since(f: &Fixture, op: usize) -> Option<u64> {
        use dstreamy_mock_discord::VoiceKind;
        use dstreamy_mock_discord::opcodes::voice::OP_VIDEO;
        f.mock
            .wait_for(T, |r| {
                r.ops
                    .iter()
                    .skip(op)
                    .filter(|o| o.is(Some(VoiceKind::Stream), OP_VIDEO))
                    .filter_map(|o| o.json()?["streams"][0]["max_framerate"].as_u64())
                    .next_back()
            })
            .await
    }

    fn fps30() -> UpdateParams {
        UpdateParams {
            crop: None,
            fps: Some(Fps::Fps30),
            resolution: None,
            bitrate_kbps: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_update_that_reconfigured_a_run_stopped_meanwhile_is_not_committed() {
        let f = Fixture::new().await;
        f.engine.start(params(), None).await.unwrap();
        f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
        let gate = f.engine.test.update_commit_gate.lock().await;
        let engine = f.engine.clone();
        let update = tokio::spawn(async move { engine.update(fps30()).await });
        timeout(T, async {
            while !f.engine.test.update_reconfigured.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("update never reached the commit");
        f.engine.stop().await.unwrap();
        drop(gate);
        let updated = timeout(T, update).await.unwrap().unwrap();
        let running = lock(&f.engine.running).is_some();
        f.engine.shutdown().await;
        assert_eq!(updated.map_err(|e| e.code), Err("not_running"));
        assert!(!running, "the stopped run's update was committed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_update_during_a_start_after_a_self_stopped_run_fails_at_once() {
        let f = Fixture::new().await;
        f.engine.start(params(), None).await.unwrap();
        f.wait_state(|s| matches!(s, SessionState::Streaming)).await;
        let conn = f.engine.current().unwrap();
        let mut published = f.engine.subscribe_lossless();
        assert!(
            f.mock
                .voice_state_from_other_session(MOCK_GUILD_ID, Some(MOCK_VOICE_CHANNEL_ID))
        );
        published_until(&mut published, |s| {
            matches!(s, SessionState::Stopped { .. })
        })
        .await;
        drop(published);
        assert!(f.engine.catch_up(&conn).await);
        drop(conn);
        assert!(lock(&f.engine.running).is_some());
        let gate = f.engine.test.start_commit_gate.lock().await;
        let engine = f.engine.clone();
        let start = tokio::spawn(async move { engine.start(params(), None).await });
        timeout(T, async {
            while !f.engine.current().is_some_and(|c| c.live()) {
                assert!(!start.is_finished(), "the start ended early");
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the start never reached the session");
        let updated = timeout(Duration::from_secs(1), f.engine.update(fps30()))
            .await
            .expect("the update waited for the start");
        assert_eq!(updated.map_err(|e| e.code), Err("not_running"));
        drop(gate);
        assert!(timeout(T, start).await.unwrap().unwrap().is_ok());
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
                .test
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

    #[tokio::test(flavor = "current_thread")]
    async fn held_store_lock_does_not_block_timers_or_other_engine_clients() {
        let f = Fixture::new().await;
        let accounts = f.engine.ctx.paths.accounts();
        let held = accounts.store_lock(Duration::ZERO).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        let started = std::time::Instant::now();
        let (_, progress) = tokio::join!(f.engine.logout_account("default"), async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            f.engine.handle(Command::Hello).await.unwrap();
            started.elapsed()
        });
        release.join().unwrap();
        assert!(
            progress < Duration::from_millis(150),
            "runtime worker blocked for {progress:?}"
        );
        assert!(!accounts.exists("default"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn canceled_account_transaction_finishes_before_the_next_selection() {
        let f = Fixture::new().await;
        let accounts = f.engine.ctx.paths.accounts();
        accounts.save("other", "other-token", None).unwrap();
        let held = accounts.store_lock(Duration::ZERO).unwrap();
        let engine = f.engine.clone();
        let first = tokio::spawn(async move { engine.select_account("other").await });
        timeout(T, async {
            while f.engine.lifecycle.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        first.abort();
        let _ = first.await;
        assert!(
            f.engine.lifecycle.try_lock().is_err(),
            "cancellation released the transaction's lifecycle lock"
        );
        let engine = f.engine.clone();
        let second = tokio::spawn(async move { engine.select_account("default").await });
        tokio::task::yield_now().await;
        assert!(!second.is_finished());
        drop(held);
        second.await.unwrap().unwrap();
        assert_eq!(accounts.default_name().unwrap().as_deref(), Some("default"));
        assert_eq!(
            accounts.load_token("other").unwrap().as_deref(),
            Some("other-token")
        );
    }

    #[tokio::test]
    async fn account_changes_report_a_busy_store_and_keep_the_account() {
        let f = Fixture::new().await;
        let accounts = f.engine.ctx.paths.accounts();
        let _held = accounts.store_lock(Duration::ZERO).unwrap();
        for result in [
            f.engine.select_account("default").await,
            f.engine.logout_account("default").await,
        ] {
            let error = result.unwrap_err();
            assert_eq!(error.code, "config", "{}", error.message);
            assert!(error.message.contains("changing the saved accounts"));
        }
        assert!(accounts.exists("default"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn selecting_an_insecure_account_returns_the_token_file_error() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new().await;
        let token_file = f.engine.ctx.paths.accounts().token_file("default");
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = f.engine.select_account("default").await.unwrap_err();
        assert_eq!(error.code, "config");
        assert!(error.message.contains("chmod 600"), "{}", error.message);
        let rows = f.engine.accounts().await.unwrap();
        assert!(
            rows["accounts"][0]["token_error"]
                .as_str()
                .unwrap()
                .contains("chmod 600")
        );
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

#[tokio::test]
async fn a_stalled_lossless_subscriber_is_disconnected_at_its_bound() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_in(
        dir.path(),
        "",
        Options {
            event_capacity: 4,
            ..Options::default()
        },
    );
    let receiver = engine.subscribe_lossless();
    for _ in 0..5 {
        engine.publish(EngineEvent::Session(SessionEvent::Resumed));
    }
    assert_eq!(
        receiver.len(),
        4,
        "subscriber backlog exceeded its capacity"
    );
    assert!(
        receiver.is_closed(),
        "overflow must disconnect, not silently skip an event"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_drained_lossless_subscriber_gets_every_event_the_broadcast_drops() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_in(
        dir.path(),
        "",
        Options {
            event_capacity: 4,
            ..Options::default()
        },
    );
    let mut lossless = engine.subscribe_lossless();
    let mut lossy = engine.events();
    for _ in 0..100 {
        engine.publish(EngineEvent::Session(SessionEvent::Resumed));
        assert!(matches!(
            lossless.recv().await,
            Some(EngineEvent::Session(SessionEvent::Resumed))
        ));
    }
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
    let f = Failure::from(CliError::Gateway(GatewayError::IdentifyLimit {
        limit: 100,
        retry_after_secs: 3 * 3600 + 11 * 60 + 5,
    }));
    assert_eq!(f.code, "identify_limit");
    assert!(
        f.message
            .contains("stopped logging in again after 100 logins in 24 hours to protect the account. Try again in about 3 h 12 min."),
        "{}",
        f.message
    );
}
