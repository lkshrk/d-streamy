//! Its own process: SIGPIPE is set back to the default (terminate), as in a C host such as OBS.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dstreamy_obs_producer::{End, Endpoint, Hello, Level, Log, Packet, Ring, Track, attach, run};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

fn default_sigpipe() {
    // SAFETY: restores the default disposition, as a non-Rust host has it; no handler is installed.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
}

#[derive(Default)]
struct Collect(Mutex<Vec<String>>);

impl Log for Collect {
    fn log(&self, _: Level, message: &str) {
        self.0.lock().unwrap().push(message.to_owned());
    }
}

/// The client's own write path (no thread mask) to a service that closed: EPIPE, and the process lives.
#[tokio::test(flavor = "current_thread")]
async fn the_client_write_path_never_raises_sigpipe() {
    default_sigpipe();
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
    });
    let hello = Hello {
        video_codec: "h264".into(),
        width: 1280,
        height: 720,
        fps: 30,
        audio_codec: "opus".into(),
        sample_rate: 48_000,
        channels: 2,
        frame_ms: 20,
        client_name: "sigpipe-test".into(),
        client_version: "0".into(),
        obs_version: "0".into(),
    };
    let connection = attach(&Endpoint::new(&path), &hello).await.unwrap();
    service.await.unwrap();
    let ring = Ring::default();
    ring.push(Packet {
        track: Track::Audio,
        keyframe: true,
        pts: 0,
        dts: 0,
        timebase: (1, 48_000),
        data: vec![0xF8; 4096],
        arrived: Instant::now(),
    });
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
    assert!(matches!(end, End::Closed(_)), "{end:?}");
    let log = log.0.lock().unwrap();
    assert!(
        log.iter().any(|m| m.starts_with("writing to the service")),
        "the write to the closed service did not fail: {log:?}"
    );
}

/// Linux sends SIGPIPE to the writing thread, so the worker's mask covers a write path without MSG_NOSIGNAL.
#[cfg(target_os = "linux")]
#[test]
fn on_linux_the_thread_mask_turns_sigpipe_into_epipe() {
    use std::io::{ErrorKind, IoSlice, Write};
    use std::os::unix::net::UnixStream;

    default_sigpipe();
    let err = std::thread::spawn(|| {
        dstreamy_obs_producer::block_sigpipe_on_this_thread().unwrap();
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        drop(theirs);
        // writev, unlike std's `write`, passes no MSG_NOSIGNAL.
        ours.write_vectored(&[IoSlice::new(&[0; 64])]).unwrap_err()
    })
    .join()
    .unwrap();
    assert_eq!(err.kind(), ErrorKind::BrokenPipe);
}
