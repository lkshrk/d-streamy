//! A service that accepts the producer and then never reads must not hold up a stop.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use dstreamy_obs_producer::client::STOP_DEADLINE;
use dstreamy_obs_producer::{End, Endpoint, Hello, Level, Log, Packet, Ring, Track, attach, run};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

struct Quiet;

impl Log for Quiet {
    fn log(&self, _: Level, _: &str) {}
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
    let run_dir = tempfile::Builder::new().prefix("ds").tempdir().unwrap();
    let dir = run_dir.path().join("d");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.join("s.sock");
    let listener = UnixListener::bind(&path).unwrap();
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
