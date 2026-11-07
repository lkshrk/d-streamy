//! The producer client against a fake service socket the test controls byte by byte.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dstreamy_obs_producer::client::STOP_DEADLINE;
use dstreamy_obs_producer::live::ServiceLevel;
use dstreamy_obs_producer::{
    End, Endpoint, Hello, Level, Log, Packet, Reports, Ring, Track, attach, run,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

struct Quiet;

impl Log for Quiet {
    fn log(&self, _: Level, _: &str) {}
}

#[derive(Default)]
struct Collect(Mutex<Vec<String>>);

impl Log for Collect {
    fn log(&self, _: Level, message: &str) {
        self.0.lock().unwrap().push(message.to_owned());
    }
}

/// A listening socket in a private directory, as `dstreamy_ipc::connect` requires.
fn socket() -> (tempfile::TempDir, PathBuf, UnixListener) {
    let run_dir = tempfile::Builder::new().prefix("ds").tempdir().unwrap();
    let dir = run_dir.path().join("d");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.join("s.sock");
    let listener = UnixListener::bind(&path).unwrap();
    (run_dir, path, listener)
}

fn hello() -> Hello {
    Hello {
        video_codec: "h264".into(),
        width: 1280,
        height: 720,
        fps: 30,
        audio_codec: "opus".into(),
        sample_rate: 48_000,
        channels: 2,
        frame_ms: 20,
        client_name: "stall-test".into(),
        client_version: "0".into(),
        obs_version: "0".into(),
    }
}

fn audio(pts: i64, len: usize) -> Packet {
    Packet {
        track: Track::Audio,
        keyframe: true,
        pts,
        dts: pts,
        timebase: (1, 48_000),
        data: vec![0xF8; len],
        arrived: Instant::now(),
    }
}

#[tokio::test]
async fn a_service_that_stops_reading_cannot_hold_up_a_stop() {
    let (_dir, path, listener) = socket();
    let service = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut hello = String::new();
        BufReader::new(read).read_line(&mut hello).await.unwrap();
        write.write_all(b"{\"attached\":1}\n").await.unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
    });

    let connection = attach(&Endpoint::new(&path), &hello()).await.unwrap();
    let ring = Ring::new(100_000, 1 << 30);
    for i in 0..64 {
        ring.push(audio(i * 960, 1 << 20));
    }
    let stop = tokio::time::sleep(Duration::from_millis(500));
    let started = Instant::now();
    let end = tokio::time::timeout(
        STOP_DEADLINE + Duration::from_secs(5),
        run(connection, &ring, || None, Reports::ignored(), stop, &Quiet),
    )
    .await
    .expect("run did not return after the stop");
    let took = started.elapsed();
    assert_eq!(end, End::Stopped(()));
    assert!(
        took < Duration::from_millis(500) + STOP_DEADLINE + Duration::from_millis(500),
        "stop took {took:?}"
    );
    service.abort();
}

#[tokio::test]
async fn a_detached_already_sent_is_read_after_the_write_fails() {
    let (_dir, path, listener) = socket();
    let service = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut hello = String::new();
        BufReader::new(read).read_line(&mut hello).await.unwrap();
        write
            .write_all(
                b"{\"attached\":1}\n{\"message\":\"no media\",\"reason\":\"stopped\",\"type\":\"detached\"}\n",
            )
            .await
            .unwrap();
    });
    let connection = attach(&Endpoint::new(&path), &hello()).await.unwrap();
    service.await.unwrap();
    let ring = Ring::default();
    ring.push(audio(0, 3));
    let log = Collect::default();
    let end = tokio::time::timeout(
        Duration::from_secs(10),
        run(
            connection,
            &ring,
            || None,
            Reports::ignored(),
            std::future::pending::<()>(),
            &log,
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        end,
        End::Detached {
            reason: dstreamy_obs_producer::DetachReason::Stopped,
            message: "no media".into()
        }
    );
    let log = log.0.lock().unwrap();
    assert!(
        log.iter()
            .any(|m| m.starts_with("writing to the service")
                && m.ends_with("reading pending events")),
        "the write did not fail first: {log:?}"
    );
}

#[tokio::test]
async fn viewer_counts_from_the_service_are_reported() {
    let (_dir, path, listener) = socket();
    let service = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut hello = String::new();
        BufReader::new(read).read_line(&mut hello).await.unwrap();
        write
            .write_all(
                concat!(
                    "{\"attached\":1}\n",
                    "{\"type\":\"viewers\",\"count\":3}\n",
                    "{\"type\":\"viewers\",\"count\":null}\n",
                    "{\"type\":\"viewers\",\"count\":0}\n",
                    "{\"type\":\"health\",\"reconnecting\":false,\"reconnects\":2,\"dropped_video\":4,\"audio_per_sec\":49,\"audio_dropouts\":1,\"audio_level\":\"good\",\"video_level\":\"warn\",\"fps\":59,\"jitter_p95_ms\":18,\"jitter_max_ms\":41,\"bitrate_kbps\":5800,\"future\":1}\n",
                    "{\"type\":\"health\"}\n",
                    "{\"type\":\"something_new\"}\n",
                    "{\"message\":\"bye\",\"reason\":\"stopped\",\"type\":\"detached\"}\n",
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    let connection = attach(&Endpoint::new(&path), &hello()).await.unwrap();
    let ring = Ring::default();
    let seen = Mutex::new(Vec::new());
    let health = Mutex::new(Vec::new());
    let end = tokio::time::timeout(
        Duration::from_secs(10),
        run(
            connection,
            &ring,
            || None,
            Reports {
                viewers: &|n| seen.lock().unwrap().push(n),
                health: &|h| health.lock().unwrap().push(h),
            },
            std::future::pending::<()>(),
            &Quiet,
        ),
    )
    .await
    .unwrap();
    assert!(matches!(end, End::Detached { .. }), "{end:?}");
    assert_eq!(*seen.lock().unwrap(), [Some(3), None, Some(0)]);
    let health = health.lock().unwrap();
    assert_eq!(health.len(), 2, "{health:?}");
    let first = health[0].expect("the first event has counters");
    assert_eq!(
        (first.reconnecting, first.reconnects, first.dropped_video),
        (false, 2, 4)
    );
    assert_eq!(
        (
            first.audio_per_sec,
            first.audio_dropouts,
            first.fps,
            first.jitter_p95_ms
        ),
        (Some(49), Some(1), Some(59), Some(18))
    );
    assert_eq!(first.audio_level, Some(ServiceLevel::Good));
    assert_eq!(first.video_level, Some(ServiceLevel::Warn));
    assert_eq!(health[1], None, "no stream runs");
    service.abort();
}

#[tokio::test]
async fn a_closed_socket_reattaches_and_keeps_capturing_until_stop() {
    let (_dir, path, listener) = socket();
    let (attached, reattached) = tokio::sync::oneshot::channel();
    let service = tokio::spawn(async move {
        let mut attached = Some(attached);
        for attempt in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut read = BufReader::new(read);
            let mut line = String::new();
            read.read_line(&mut line).await.unwrap();
            assert!(line.contains("producer"));
            write
                .write_all(format!("{{\"attached\":{attempt}}}\n").as_bytes())
                .await
                .unwrap();
            if attempt == 1 {
                attached.take().unwrap().send(()).unwrap();
                std::future::pending::<()>().await;
                return;
            }
        }
    });
    let endpoint = Endpoint::new(&path);
    let connection = attach(&endpoint, &hello()).await.unwrap();
    let ring = Ring::default();
    let end = tokio::time::timeout(
        Duration::from_secs(5),
        dstreamy_obs_producer::client::run_reconnecting(
            (&endpoint, &hello()),
            connection,
            &ring,
            || None,
            Reports::ignored(),
            reattached,
            &Quiet,
        ),
    )
    .await
    .expect("the producer did not reattach");
    assert!(matches!(end, End::Stopped(Ok(()))), "{end:?}");
    service.abort();
}

#[tokio::test]
async fn peer_eof_during_a_consumed_stop_drain_keeps_the_stop_outcome() {
    struct DrainStarted(Mutex<Option<tokio::sync::oneshot::Sender<()>>>);
    impl Log for DrainStarted {
        fn log(&self, _: Level, _: &str) {}
        fn stop_drain_started(&self, queued: usize) {
            assert!(queued > 0);
            self.0.lock().unwrap().take().unwrap().send(()).unwrap();
        }
    }
    let (_dir, path, listener) = socket();
    let (draining, drain_started) = tokio::sync::oneshot::channel();
    let service = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        let mut line = String::new();
        read.read_line(&mut line).await.unwrap();
        write.write_all(b"{\"attached\":1}\n").await.unwrap();
        drain_started.await.unwrap();
        write.shutdown().await.unwrap();
        std::future::pending::<()>().await;
    });
    let endpoint = Endpoint::new(&path);
    let connection = attach(&endpoint, &hello()).await.unwrap();
    let ring = Ring::new(100, 128 << 20);
    for i in 0..64 {
        ring.push(audio(i * 960, 1 << 20));
    }
    let stop = async { String::from("shutdown") };
    let log = DrainStarted(Mutex::new(Some(draining)));
    let end = tokio::time::timeout(
        STOP_DEADLINE + Duration::from_secs(2),
        dstreamy_obs_producer::client::run_reconnecting(
            (&endpoint, &hello()),
            connection,
            &ring,
            || None,
            Reports::ignored(),
            stop,
            &log,
        ),
    )
    .await
    .expect("stop must stay bounded");
    assert_eq!(end, End::Stopped(String::from("shutdown")));
    assert!(
        ring.stats().queued > 0,
        "the unread peer interrupted a nonempty drain"
    );
    service.abort();
    let _ = service.await;
}

#[tokio::test]
async fn reattachment_sends_a_queued_gop_and_audio_without_replaying_the_old_idr() {
    use dstreamy_ipc::{Kind, ProducerCodec};
    use futures_util::StreamExt;
    use tokio_util::codec::FramedRead;
    let video = |pts, keyframe| Packet {
        track: Track::Video,
        pts,
        dts: pts,
        keyframe,
        timebase: (1, 30),
        data: vec![0, 0, 0, 1, if keyframe { 0x65 } else { 0x41 }, 0x88],
        arrived: Instant::now(),
    };
    let (_dir, path, listener) = socket();
    let (complete, stop) = tokio::sync::oneshot::channel();
    let service = tokio::spawn(async move {
        let mut after = Vec::new();
        for attempt in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut read = BufReader::new(read);
            let mut line = String::new();
            read.read_line(&mut line).await.unwrap();
            write
                .write_all(format!("{{\"attached\":{attempt}}}\n").as_bytes())
                .await
                .unwrap();
            let mut frames = FramedRead::new(read, ProducerCodec::new());
            if attempt == 0 {
                let first = frames.next().await.unwrap().unwrap();
                assert!(first.keyframe);
                assert_eq!(first.pts_us, 0);
            } else {
                for _ in 0..3 {
                    after.push(frames.next().await.unwrap().unwrap());
                }
                complete.send(()).unwrap();
                return after;
            }
        }
        unreachable!()
    });
    let endpoint = Endpoint::new(&path);
    let connection = attach(&endpoint, &hello()).await.unwrap();
    let ring = Ring::default();
    ring.push(video(0, true));
    let mut reset_count = 0;
    let health = |state: Option<dstreamy_obs_producer::live::ServiceHealth>| {
        assert!(state.is_none());
        ring.push(video(29, false));
        ring.push(video(30, true));
        ring.push(video(31, false));
        ring.push(audio(48_000, 3));
    };
    let end = tokio::time::timeout(
        Duration::from_secs(5),
        dstreamy_obs_producer::client::run_reconnecting(
            (&endpoint, &hello()),
            connection,
            &ring,
            || {
                reset_count += 1;
                Some(vec![0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce])
            },
            Reports {
                viewers: &|_| {},
                health: &health,
            },
            async {
                stop.await.unwrap();
            },
            &Quiet,
        ),
    )
    .await
    .unwrap();
    assert_eq!(end, End::Stopped(()));
    let after = service.await.unwrap();
    assert_eq!(
        after
            .iter()
            .map(|f| (f.kind, f.pts_us, f.keyframe))
            .collect::<Vec<_>>(),
        [
            (Kind::Video, 1_000_000, true),
            (Kind::Video, 1_033_333, false),
            (Kind::Opus, 1_000_000, false)
        ]
    );
    assert!(after[0].payload.windows(5).any(|w| w == [0, 0, 0, 1, 0x67]));
    assert!(after[0].payload.windows(5).any(|w| w == [0, 0, 0, 1, 0x68]));
    assert_eq!(
        reset_count, 2,
        "fresh conversion state restores parameter sets after reattach"
    );
    assert_eq!(
        ring.stats().dropped_video,
        1,
        "only the stale delta before the queued IDR is discarded"
    );
}
