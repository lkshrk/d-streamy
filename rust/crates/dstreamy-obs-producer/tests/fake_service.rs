//! The producer client against a fake service socket the test controls byte by byte.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dstreamy_obs_producer::client::STOP_DEADLINE;
use dstreamy_obs_producer::{End, Endpoint, Hello, Level, Log, Packet, Ring, Track, attach, run};
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
        run(connection, &ring, || None, stop, &Quiet),
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
            std::future::pending::<()>(),
            &log,
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        end,
        End::Detached {
            reason: "stopped".into(),
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
