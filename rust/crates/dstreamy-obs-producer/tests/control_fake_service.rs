//! The control client's wait for a start in flight, against a fake service that scripts each `status`.
#![cfg(unix)]

use std::collections::VecDeque;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dstreamy_obs_producer::Endpoint;
use dstreamy_obs_producer::control::{
    Candidate, ControlError, Instance, ObsControl, Ownership, StartRequest, Started, Stopped,
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

struct Script {
    connection: u64,
    hello: Value,
    statuses: Vec<Option<Value>>,
    answer_start: bool,
}

impl Fake {
    fn start(hello: Value, statuses: Vec<Option<Value>>) -> Self {
        Self::scripted(vec![Script {
            connection: 9,
            hello,
            statuses,
            answer_start: true,
        }])
    }

    fn two_unanswered_starts_then_first_wins() -> Self {
        Self::scripted(
            (1..=4)
                .map(|connection| Script {
                    connection,
                    hello: if connection <= 2 {
                        Value::Null
                    } else {
                        status(Some(CONNECTION))
                    },
                    statuses: vec![],
                    answer_start: connection > 2,
                })
                .collect(),
        )
    }

    fn scripted(scripts: Vec<Script>) -> Self {
        let dir = tempfile::Builder::new().prefix("ds").tempdir().unwrap();
        let private = dir.path().join("d");
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = private.join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let seen = commands.clone();
        tokio::spawn(async move {
            for Script {
                connection,
                hello,
                statuses,
                answer_start,
            } in scripts
            {
                let mut statuses: VecDeque<Option<Value>> = statuses.into();
                let (stream, _) = listener.accept().await.unwrap();
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let command: Value = serde_json::from_str(&line).unwrap();
                    let cmd = command["cmd"].as_str().unwrap().to_owned();
                    seen.lock().unwrap().push(cmd.clone());
                    let data = match cmd.as_str() {
                        "hello" => json!({"protocol": 2, "connection": connection,
                            "features": ["start_grace_secs", "status_place"], "status": hello}),
                        "status" => match statuses.pop_front().flatten() {
                            Some(status) => status,
                            None => continue,
                        },
                        "start" if !answer_start => continue,
                        _ => Value::Null,
                    };
                    let reply = json!({"id": command["id"], "type": "result", "data": data});
                    write
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .unwrap();
                }
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
            connections: vec![
                Candidate {
                    id: CONNECTION,
                    fps: Some(30),
                },
                Candidate {
                    id: 4,
                    fps: Some(60),
                },
            ],
            guild_id: "g".into(),
            channel_id: "c".into(),
            instance: Some(Instance {
                dev: meta.dev(),
                ino: meta.ino(),
                mtime: meta.mtime(),
                mtime_nsec: meta.mtime_nsec(),
            }),
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
async fn two_lost_starts_in_a_row_still_own_the_session() {
    let fake = Fake::two_unanswered_starts_then_first_wins();
    let control =
        || ObsControl::new(Endpoint::new(&fake.path)).with_start_timeout(Duration::from_millis(10));

    let first = control().start(&request(), None).await.unwrap_err();
    assert!(matches!(
        first.error,
        ControlError::Timeout {
            command: "start",
            ..
        }
    ));
    let first = *first.ownership.expect("the first candidate");
    assert_eq!(first.connections[0].id, 1);

    let second = control().start(&request(), Some(&first)).await.unwrap_err();
    assert!(matches!(
        second.error,
        ControlError::Timeout {
            command: "start",
            ..
        }
    ));
    let ownership = *second.ownership.expect("both candidates");
    assert_eq!(
        ownership
            .connections
            .iter()
            .map(|candidate| candidate.id)
            .collect::<Vec<_>>(),
        [1, 2]
    );

    let third = control().start(&request(), Some(&ownership)).await.unwrap();
    assert_eq!(third.session_fps(), Some(30));
    let confirmed = third.ownership.expect("the first start won");
    assert_eq!(confirmed.connections.len(), 1);
    assert_eq!(confirmed.connections[0].id, 1);
    assert_eq!(control().stop(Some(&confirmed)).await, Ok(Stopped::Stopped));
    assert_eq!(
        fake.commands(),
        ["hello", "start", "hello", "start", "hello", "hello", "stop"]
    );
}

fn unanswered_start() -> Fake {
    Fake::scripted(vec![Script {
        connection: 9,
        hello: Value::Null,
        statuses: vec![],
        answer_start: false,
    }])
}

#[tokio::test]
async fn a_start_abandoned_after_it_was_sent_keeps_its_candidate() {
    let fake = unanswered_start();
    let failed = fake
        .control(Duration::from_secs(5))
        .start_until(
            &request(),
            None,
            tokio::time::sleep(Duration::from_millis(300)),
        )
        .await
        .unwrap_err();
    assert_eq!(failed.error, ControlError::Abandoned);
    let candidate = *failed
        .ownership
        .expect("serve may still carry the start out");
    assert_eq!(
        candidate.connections,
        [Candidate {
            id: 9,
            fps: Some(30)
        }]
    );
    assert_eq!(candidate.instance, fake.ownership().instance);
    assert_eq!(fake.commands(), ["hello", "start"]);
}

#[tokio::test]
async fn a_start_abandoned_before_it_was_sent_claims_nothing() {
    let fake = unanswered_start();
    let failed = fake
        .control(Duration::from_secs(5))
        .start_until(&request(), None, std::future::ready(()))
        .await
        .unwrap_err();
    assert_eq!(failed.error, ControlError::Abandoned);
    assert_eq!(failed.ownership, None);
    assert!(!fake.commands().contains(&"start".to_owned()));
}

#[tokio::test]
async fn a_start_answered_before_it_is_abandoned_is_kept() {
    let fake = Fake::start(Value::Null, vec![Some(status(Some(9)))]);
    let started = fake
        .control(Duration::from_secs(5))
        .start_until(&request(), None, std::future::pending())
        .await
        .unwrap();
    assert_eq!(started.ownership.unwrap().connections[0].id, 9);
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
    assert_eq!(
        started.unwrap().session_fps(),
        Some(30),
        "the winner's fps, not the newest candidate's"
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

/// Answers every connection's `hello` with `features`, and each other command with `answer(cmd)`.
fn answering(features: &[&str], answer: fn(&str) -> Value) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new().prefix("ds").tempdir().unwrap();
    let private = dir.path().join("d");
    std::fs::create_dir(&private).unwrap();
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = private.join("s.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let features: Vec<String> = features.iter().map(|f| (*f).to_owned()).collect();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let features = features.clone();
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let command: Value = serde_json::from_str(&line).unwrap();
                    let mut reply = match command["cmd"].as_str().unwrap() {
                        "hello" => {
                            json!({"type": "result", "data": {"protocol": 2, "connection": 1,
                            "features": features, "status": {"session": null}}})
                        }
                        cmd => answer(cmd),
                    };
                    reply["id"] = command["id"].clone();
                    write
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .unwrap();
                }
            });
        }
    });
    (dir, path)
}

const OLD_FEATURES: [&str; 2] = ["start_grace_secs", "status_place"];
const LOGIN_FEATURES: [&str; 3] = ["start_grace_secs", "status_place", "browser_login"];

fn not_logged_in(_: &str) -> Value {
    json!({"type": "error", "code": "not_logged_in", "message": "not logged in"})
}

#[tokio::test]
async fn a_missing_login_offers_the_window_only_where_serve_has_it() {
    let (_dir, path) = answering(&LOGIN_FEATURES, not_logged_in);
    let control = ObsControl::new(Endpoint::new(&path));
    assert_eq!(
        control.guilds().await,
        Err(ControlError::NotLoggedIn { window: true })
    );
    let (_dir, path) = answering(&OLD_FEATURES, not_logged_in);
    let control = ObsControl::new(Endpoint::new(&path));
    assert_eq!(
        control.guilds().await,
        Err(ControlError::NotLoggedIn { window: false })
    );
    assert_eq!(
        control.login().await,
        Err(ControlError::Outdated {
            feature: "browser_login"
        }),
        "an older serve is never sent a login"
    );
}

#[tokio::test]
async fn the_login_s_outcome_comes_back_as_its_error() {
    let (_dir, path) = answering(&LOGIN_FEATURES, |cmd| match cmd {
        "login" => json!({"type": "error", "code": "login_cancelled", "message": "closed"}),
        _ => json!({"type": "result", "data": null}),
    });
    let control = ObsControl::new(Endpoint::new(&path));
    assert_eq!(control.login().await, Err(ControlError::LoginCancelled));
    let (_dir, path) = answering(
        &LOGIN_FEATURES,
        |_| json!({"type": "error", "code": "login_failed", "message": "no display"}),
    );
    let err = ObsControl::new(Endpoint::new(&path))
        .login()
        .await
        .unwrap_err();
    assert_eq!(err, ControlError::LoginFailed("no display".into()));
    assert_eq!(
        err.to_string(),
        "The login didn't finish (no display); try again"
    );
    let (_dir, path) = answering(
        &LOGIN_FEATURES,
        |_| json!({"type": "result", "data": {"username": "mock", "global_name": null}}),
    );
    assert_eq!(
        ObsControl::new(Endpoint::new(&path)).login().await,
        Ok(Some("mock".into()))
    );
}
