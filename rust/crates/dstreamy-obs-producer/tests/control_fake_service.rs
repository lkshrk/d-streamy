//! The control client's wait for a start in flight, against a fake service that scripts each `status`.
#![cfg(unix)]

use std::collections::VecDeque;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dstreamy_obs_producer::Endpoint;
use dstreamy_obs_producer::control::{
    ControlError, Instance, ObsControl, Ownership, StartRequest, Started, Stopped,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

const CONNECTION: u64 = 1;

fn status(started_by: Option<u64>) -> Value {
    json!({"session": {"state": "connecting"}, "source": null, "stats": null, "started_by": started_by})
}

/// What the fake answers: `Some` is the next `status`, `None` leaves that `status` unanswered.
struct Fake {
    _dir: tempfile::TempDir,
    path: PathBuf,
    commands: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    fn start(hello: Value, statuses: Vec<Option<Value>>) -> Self {
        let dir = tempfile::Builder::new().prefix("ds").tempdir().unwrap();
        let private = dir.path().join("d");
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = private.join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let seen = commands.clone();
        tokio::spawn(async move {
            let mut statuses: VecDeque<Option<Value>> = statuses.into();
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let command: Value = serde_json::from_str(&line).unwrap();
                let cmd = command["cmd"].as_str().unwrap().to_owned();
                seen.lock().unwrap().push(cmd.clone());
                let data = match cmd.as_str() {
                    "hello" => json!({"protocol": 2, "connection": 9,
                        "features": ["start_grace_secs", "status_place"], "status": hello}),
                    "status" => match statuses.pop_front().flatten() {
                        Some(status) => status,
                        None => continue,
                    },
                    _ => Value::Null,
                };
                let reply = json!({"id": command["id"], "type": "result", "data": data});
                write
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        Self {
            _dir: dir,
            path,
            commands,
        }
    }

    fn control(&self, settle: Duration) -> ObsControl {
        ObsControl::new(Endpoint::new(&self.path)).with_settle_timeout(settle)
    }

    fn ownership(&self) -> Ownership {
        let meta = std::fs::metadata(&self.path).unwrap();
        Ownership {
            connections: vec![CONNECTION, 4],
            guild_id: "g".into(),
            channel_id: "c".into(),
            instance: Some(Instance {
                dev: meta.dev(),
                ino: meta.ino(),
                mtime: meta.mtime(),
                mtime_nsec: meta.mtime_nsec(),
            }),
            fps: Some(30),
        }
    }

    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }
}

fn request() -> StartRequest {
    StartRequest {
        guild_id: "g".into(),
        channel_id: "c".into(),
        width: 1280,
        height: 720,
        fps: 30,
        grace_secs: None,
    }
}

#[tokio::test]
async fn a_stop_waits_until_serve_names_our_start() {
    let fake = Fake::start(
        status(None),
        vec![Some(status(None)), Some(status(Some(CONNECTION)))],
    );
    let stopped = fake
        .control(Duration::from_secs(5))
        .stop(Some(&fake.ownership()))
        .await;
    assert_eq!(stopped, Ok(Stopped::Stopped));
    assert_eq!(fake.commands(), ["hello", "status", "status", "stop"]);
}

#[tokio::test]
async fn a_start_waits_until_serve_names_our_start_and_keeps_only_it() {
    let fake = Fake::start(status(None), vec![Some(status(Some(CONNECTION)))]);
    let ownership = fake.ownership();
    let started = fake
        .control(Duration::from_secs(5))
        .start(&request(), Some(&ownership))
        .await;
    assert_eq!(
        started,
        Ok(Started {
            ownership: Some(ownership.confirmed(CONNECTION)),
            channel: "c".into(),
        })
    );
    assert_eq!(fake.commands(), ["hello", "status"]);
}

#[tokio::test]
async fn a_start_that_stays_unnamed_gives_up_at_the_cap() {
    let fake = Fake::start(status(None), vec![Some(status(None)); 100]);
    let began = Instant::now();
    let stopped = fake
        .control(Duration::from_millis(300))
        .stop(Some(&fake.ownership()))
        .await;
    assert_eq!(stopped, Err(ControlError::StillStarting));
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "{:?}",
        began.elapsed()
    );
    assert!(!fake.commands().contains(&"stop".to_owned()));
}

#[tokio::test]
async fn an_unanswered_status_counts_against_the_same_cap() {
    let fake = Fake::start(status(None), vec![None]);
    let began = Instant::now();
    let started = fake
        .control(Duration::from_millis(300))
        .start(&request(), Some(&fake.ownership()))
        .await;
    assert_eq!(started.unwrap_err().error, ControlError::StillStarting);
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "{:?}",
        began.elapsed()
    );
}
