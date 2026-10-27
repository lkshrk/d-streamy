//! The control client's wait for a start in flight, against a fake service that scripts each `status`.
#![cfg(unix)]

use std::collections::VecDeque;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dstreamy_obs_producer::Endpoint;
use dstreamy_obs_producer::control::{
    Candidate, ControlError, Instance, ObsControl, Ownership, StartCertainty, StartRequest,
    Started, Stopped,
};
use dstreamy_obs_producer::dock::{Canvas, DockModel, DockSettings};
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

fn serving(started_by: Option<u64>) -> Value {
    json!({"session": {"state": "streaming", "guild_id": "g", "channel_id": "c",
        "channel_name": "General", "target": "external"}, "source": null, "stats": null,
        "started_by": started_by})
}

fn idle() -> Value {
    json!({"session": null, "source": null, "stats": null, "started_by": null})
}

fn script(
    connection: u64,
    hello: Value,
    statuses: Vec<Option<Value>>,
    answer_start: bool,
) -> Script {
    Script {
        connection,
        hello,
        statuses,
        answer_start,
    }
}

impl Fake {
    fn instance(&self) -> Instance {
        let meta = std::fs::metadata(&self.path).unwrap();
        Instance {
            dev: meta.dev(),
            ino: meta.ino(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        }
    }

    fn owned_by(&self, ids: &[u64]) -> Ownership {
        Ownership {
            connections: ids
                .iter()
                .map(|&id| Candidate { id, fps: Some(30) })
                .collect(),
            guild_id: "g".into(),
            channel_id: "c".into(),
            instance: Some(self.instance()),
        }
    }

    fn legacy(&self, ids: &[u64]) -> Ownership {
        Ownership {
            instance: None,
            ..self.owned_by(ids)
        }
    }

    fn replaced(&self, ids: &[u64]) -> Ownership {
        let old = self.instance();
        Ownership {
            instance: Some(Instance {
                ino: old.ino + 1,
                ..old
            }),
            ..self.owned_by(ids)
        }
    }

    fn wait_for_command(&self, name: &str) -> impl Future<Output = ()> + use<> {
        let commands = self.commands.clone();
        let name = name.to_owned();
        async move {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !commands.lock().unwrap().contains(&name) {
                assert!(Instant::now() < deadline, "the fake never saw {name}");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }
}

fn ids(o: &Ownership) -> Vec<u64> {
    o.connections.iter().map(|c| c.id).collect()
}

type Save = (String, Vec<String>);

/// The controller's journal: each checkpoint is stored serialized with the commands the service had seen so far.
#[derive(Clone)]
struct Journal {
    commands: Arc<Mutex<Vec<String>>>,
    saves: Arc<Mutex<Vec<Save>>>,
    fails: bool,
}

#[derive(serde::Deserialize)]
struct Stored {
    owned: Option<Ownership>,
    #[serde(default)]
    certainty: StartCertainty,
}

impl Journal {
    fn new(fake: &Fake) -> Self {
        Self {
            commands: fake.commands.clone(),
            saves: Arc::default(),
            fails: false,
        }
    }

    fn failing(fake: &Fake) -> Self {
        Self {
            fails: true,
            ..Self::new(fake)
        }
    }

    fn checkpoint(&self) -> impl FnMut(&Ownership) -> Result<(), ControlError> + use<> {
        let journal = self.clone();
        move |owned: &Ownership| {
            let record = json!({"owned": owned, "certainty": "pending"}).to_string();
            let seen = journal.commands.lock().unwrap().clone();
            journal.saves.lock().unwrap().push((record, seen));
            if journal.fails {
                Err(ControlError::Persistence("disk full".into()))
            } else {
                Ok(())
            }
        }
    }

    fn calls(&self) -> usize {
        self.saves.lock().unwrap().len()
    }

    fn commands_at(&self, call: usize) -> Vec<String> {
        self.saves.lock().unwrap()[call].1.clone()
    }

    fn reloaded(&self) -> (Ownership, StartCertainty) {
        let record = self.saves.lock().unwrap().last().unwrap().0.clone();
        let stored: Stored = serde_json::from_str(&record).unwrap();
        (stored.owned.unwrap(), stored.certainty)
    }
}

const PENDING: StartCertainty = StartCertainty::Pending;
const CONFIRMED: StartCertainty = StartCertainty::Confirmed;

#[tokio::test]
async fn pending_owner_is_saved_before_start_send() {
    let fake = Fake::scripted(vec![script(
        9,
        Value::Null,
        vec![Some(serving(Some(9)))],
        true,
    )]);
    let journal = Journal::new(&fake);
    let previous = fake.owned_by(&[1, 4]);

    let started = fake
        .control(Duration::from_secs(5))
        .start_persistent(&request(), Some(&previous), journal.checkpoint())
        .await
        .unwrap();

    assert_eq!(journal.calls(), 1);
    let (saved, certainty) = journal.reloaded();
    assert_eq!(ids(&saved), [1, 4, 9], "previous and new candidates");
    assert_eq!(certainty, PENDING);
    assert_eq!(saved.instance, Some(fake.instance()));
    assert_eq!(journal.commands_at(0), ["hello"], "no start yet");
    assert_eq!(fake.commands(), ["hello", "start", "status"]);
    assert_eq!(ids(&started.ownership.unwrap()), [9]);
}

#[tokio::test]
async fn failed_checkpoint_never_sends_start() {
    let fake = Fake::start(Value::Null, vec![]);
    let journal = Journal::failing(&fake);

    let failed = fake
        .control(Duration::from_secs(5))
        .start_persistent(&request(), None, journal.checkpoint())
        .await
        .unwrap_err();

    assert_eq!(failed.error, ControlError::Persistence("disk full".into()));
    assert_eq!(
        failed.ownership, None,
        "the previous journal stays authoritative"
    );
    assert_eq!(journal.calls(), 1);
    assert_eq!(fake.commands(), ["hello"]);
}

#[tokio::test]
async fn the_checkpoint_keeps_the_first_and_newest_seven_candidates() {
    let fake = Fake::scripted(vec![script(
        9,
        Value::Null,
        vec![Some(serving(Some(9)))],
        true,
    )]);
    let eight = fake.owned_by(&[11, 12, 13, 14, 15, 16, 17, 18]);
    let journal = Journal::new(&fake);
    fake.control(Duration::from_secs(5))
        .start_persistent(&request(), Some(&eight), journal.checkpoint())
        .await
        .unwrap();
    assert_eq!(ids(&journal.reloaded().0), [11, 13, 14, 15, 16, 17, 18, 9]);

    let fake = Fake::scripted(vec![script(
        9,
        Value::Null,
        vec![Some(serving(Some(9)))],
        true,
    )]);
    let seven = fake.owned_by(&[11, 12, 13, 14, 15, 16, 17]);
    let journal = Journal::new(&fake);
    fake.control(Duration::from_secs(5))
        .start_persistent(&request(), Some(&seven), journal.checkpoint())
        .await
        .unwrap();
    assert_eq!(ids(&journal.reloaded().0), [11, 12, 13, 14, 15, 16, 17, 9]);
}

#[tokio::test]
async fn cancel_after_checkpoint_retains_candidate() {
    let fake = Fake::scripted(vec![
        script(9, Value::Null, vec![], false),
        script(10, status(Some(9)), vec![], true),
    ]);
    let journal = Journal::new(&fake);
    let control = fake.control(Duration::from_secs(5));
    let task = tokio::spawn({
        let control = control.clone();
        let checkpoint = journal.checkpoint();
        async move { control.start_persistent(&request(), None, checkpoint).await }
    });
    fake.wait_for_command("start").await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());

    let (reloaded, certainty) = journal.reloaded();
    assert_eq!((ids(&reloaded), certainty), (vec![9], PENDING));
    assert_eq!(
        control.stop_reconciled(Some(&reloaded), certainty).await,
        Ok(Stopped::Stopped)
    );
    assert_eq!(fake.commands(), ["hello", "start", "hello", "stop"]);
}

#[tokio::test]
async fn a_lost_start_reply_is_recovered_by_service_instance_and_connection() {
    let fake = Fake::scripted(vec![
        script(9, Value::Null, vec![], false),
        script(10, status(Some(9)), vec![], true),
    ]);
    let journal = Journal::new(&fake);
    let control = fake.control(Duration::from_secs(5));
    let lost = control
        .clone()
        .with_start_timeout(Duration::from_millis(20))
        .start_persistent(&request(), None, journal.checkpoint())
        .await
        .unwrap_err();
    assert!(matches!(
        lost.error,
        ControlError::Timeout {
            command: "start",
            ..
        }
    ));
    let candidate = *lost.ownership.expect("a start that may still land");
    assert_eq!(candidate, journal.reloaded().0);
    assert_eq!(candidate, fake.owned_by(&[9]));

    assert_eq!(
        control.stop_reconciled(Some(&candidate), PENDING).await,
        Ok(Stopped::Stopped)
    );
}

#[tokio::test]
async fn a_lost_start_after_a_confirmed_session_pends_only_the_new_start() {
    let fake = Fake::scripted(vec![
        script(9, idle(), vec![], false),
        script(10, status(Some(9)), vec![], true),
    ]);
    let journal = Journal::new(&fake);
    let control = fake.control(Duration::from_secs(5));
    let confirmed = fake.owned_by(&[3]);
    let lost = control
        .clone()
        .with_start_timeout(Duration::from_millis(20))
        .start_reconciled(
            &request(),
            Some(&confirmed),
            CONFIRMED,
            journal.checkpoint(),
        )
        .await
        .unwrap_err();
    let candidate = *lost.ownership.expect("a start that may still land");
    assert_eq!(ids(&candidate), [9]);
    assert_eq!(candidate, journal.reloaded().0);

    assert_eq!(
        control.stop_reconciled(Some(&candidate), PENDING).await,
        Ok(Stopped::Stopped)
    );
}

#[tokio::test]
async fn legacy_start_persistent_still_merges_the_previous_candidates() {
    let fake = Fake::scripted(vec![script(9, idle(), vec![], true)]);
    let journal = Journal::new(&fake);
    fake.control(Duration::from_secs(5))
        .start_persistent(&request(), Some(&fake.owned_by(&[3])), journal.checkpoint())
        .await
        .unwrap();
    assert_eq!(ids(&journal.reloaded().0), [3, 9]);
}

#[tokio::test]
async fn idle_status_after_timed_out_start_keeps_pending_owner() {
    let fake = Fake::scripted(vec![
        script(9, Value::Null, vec![], false),
        script(10, idle(), vec![], true),
        script(11, status(Some(9)), vec![], true),
    ]);
    let journal = Journal::new(&fake);
    let control = fake.control(Duration::from_secs(5));
    let mut dock = DockModel::new(DockSettings {
        guild_id: Some("g".into()),
        channel_id: Some("c".into()),
        ..DockSettings::default()
    });
    let canvas = Canvas {
        fps_num: 30,
        fps_den: 1,
        output_width: 1280,
        output_height: 720,
    };
    let start = dock.start(canvas).unwrap();
    let lost = control
        .clone()
        .with_start_timeout(Duration::from_millis(20))
        .start_persistent(&start, dock.persisted(), journal.checkpoint())
        .await;
    dock.started(lost);
    let before = journal.reloaded().0;
    assert_eq!(dock.persisted(), Some(&before));

    let owned = dock.stop().expect("an owned idle dock stops");
    assert_eq!(
        control.stop_reconciled(owned.as_ref(), PENDING).await,
        Err(ControlError::UnresolvedStart)
    );
    dock.stopped(Err(ControlError::UnresolvedStart.to_string()));
    assert_eq!(dock.persisted(), Some(&before), "the journal is kept");
    assert!(!fake.commands().contains(&"stop".to_owned()));

    let owned = dock.stop().unwrap();
    assert_eq!(
        control.stop_reconciled(owned.as_ref(), PENDING).await,
        Ok(Stopped::Stopped)
    );
    dock.stopped(Ok(()));
    assert_eq!(dock.persisted(), None);
    assert_eq!(fake.commands().last().unwrap(), "stop");
}

#[tokio::test]
async fn a_relaunch_reads_the_saved_record_as_pending_and_stays_unresolved() {
    let fake = Fake::scripted(vec![
        script(10, idle(), vec![], true),
        script(11, status(Some(9)), vec![], true),
    ]);
    let record = json!({"owned": fake.owned_by(&[9])}).to_string();
    let stored: Stored = serde_json::from_str(&record).unwrap();
    assert_eq!(stored.certainty, PENDING, "no certainty in the record");
    let owned = stored.owned.unwrap();
    let control = fake.control(Duration::from_secs(5));
    assert_eq!(
        control
            .stop_reconciled(Some(&owned), stored.certainty)
            .await,
        Err(ControlError::UnresolvedStart)
    );
    assert_eq!(
        control
            .stop_reconciled(Some(&owned), stored.certainty)
            .await,
        Ok(Stopped::Stopped)
    );
}

#[tokio::test]
async fn a_start_that_stays_unnamed_keeps_a_pending_owner() {
    let fake = Fake::start(status(None), vec![Some(status(None)); 100]);
    let stopped = fake
        .control(Duration::from_millis(300))
        .stop_reconciled(Some(&fake.owned_by(&[9])), PENDING)
        .await;
    assert_eq!(stopped, Err(ControlError::StillStarting));
    assert!(!fake.commands().contains(&"stop".to_owned()));
}

#[tokio::test]
async fn a_pending_start_beside_a_different_live_owner_is_unresolved() {
    let fake = Fake::scripted(vec![
        script(1, serving(Some(77)), vec![], true),
        script(2, serving(Some(77)), vec![], true),
    ]);
    let owned = fake.owned_by(&[9]);
    let control = fake.control(Duration::from_secs(5));
    assert_eq!(
        control.stop_reconciled(Some(&owned), PENDING).await,
        Err(ControlError::UnresolvedStart)
    );
    assert_eq!(
        control.stop_reconciled(Some(&owned), CONFIRMED).await,
        Ok(Stopped::LeftRunning)
    );
    assert_eq!(fake.commands(), ["hello", "hello"]);
}

#[tokio::test]
async fn stopping_one_of_several_pending_candidates_keeps_the_others() {
    let fake = Fake::scripted(vec![
        script(1, idle(), vec![], true),
        script(2, serving(Some(4)), vec![], true),
        script(3, idle(), vec![], true),
    ]);
    let owned = fake.owned_by(&[1, 4]);
    let control = fake.control(Duration::from_secs(5));
    assert_eq!(
        control.stop_reconciled(Some(&owned), PENDING).await,
        Err(ControlError::UnresolvedStart)
    );
    assert_eq!(
        control.stop_reconciled(Some(&owned), PENDING).await,
        Err(ControlError::UnresolvedStart),
        "the one that landed was stopped, the other may still land"
    );
    assert_eq!(
        control.stop_reconciled(Some(&owned), PENDING).await,
        Err(ControlError::UnresolvedStart)
    );
    assert_eq!(fake.commands(), ["hello", "hello", "stop", "hello"]);
}

#[tokio::test]
async fn a_confirmed_owner_with_nothing_live_stops_normally() {
    let fake = Fake::scripted(vec![
        script(1, idle(), vec![], true),
        script(2, status(Some(9)), vec![], true),
    ]);
    let owned = fake.owned_by(&[9]);
    let control = fake.control(Duration::from_secs(5));
    assert_eq!(
        control.stop_reconciled(Some(&owned), CONFIRMED).await,
        Ok(Stopped::NotRunning)
    );
    assert_eq!(
        control.stop_reconciled(Some(&owned), CONFIRMED).await,
        Ok(Stopped::Stopped)
    );
    assert_eq!(
        control.stop_reconciled(None, CONFIRMED).await,
        Ok(Stopped::LeftRunning)
    );
}

#[tokio::test]
async fn a_replaced_service_clears_an_obsolete_pending_identity() {
    let fake = Fake::scripted(vec![
        script(1, idle(), vec![], true),
        script(2, serving(Some(9)), vec![], true),
    ]);
    let obsolete = fake.replaced(&[9, 12]);
    let control = fake.control(Duration::from_secs(5));
    assert_eq!(
        control.stop_reconciled(Some(&obsolete), PENDING).await,
        Ok(Stopped::NotRunning)
    );
    assert_eq!(
        control.stop_reconciled(Some(&obsolete), PENDING).await,
        Ok(Stopped::LeftRunning)
    );
    assert_eq!(fake.commands(), ["hello", "hello"]);
}

#[tokio::test]
async fn an_unknown_service_run_stays_unresolved_even_with_a_colliding_connection_id() {
    let fake = Fake::scripted(vec![
        script(1, idle(), vec![], true),
        script(2, serving(Some(9)), vec![], true),
    ]);
    let unknown = fake.legacy(&[9, 12]);
    let control = fake.control(Duration::from_secs(5));
    for _ in 0..2 {
        assert_eq!(
            control.stop_reconciled(Some(&unknown), PENDING).await,
            Err(ControlError::UnresolvedStart)
        );
    }
    assert_eq!(
        fake.commands(),
        Vec::<String>::new(),
        "nothing is sent or stopped"
    );

    let mut dock = DockModel::new(DockSettings {
        owned: Some(unknown.clone()),
        ..DockSettings::default()
    });
    let owned = dock.stop().unwrap();
    assert_eq!(owned.as_ref(), Some(&unknown));
    dock.stopped(Err(ControlError::UnresolvedStart.to_string()));
    assert_eq!(
        dock.persisted(),
        Some(&unknown),
        "no owner-clear transition"
    );
}

#[tokio::test]
async fn a_missing_service_ends_only_an_identified_pending_start() {
    let dir = tempfile::Builder::new().prefix("ds").tempdir().unwrap();
    let control = ObsControl::new(Endpoint::new(dir.path().join("d").join("s.sock")));
    let known = Ownership {
        connections: vec![Candidate { id: 9, fps: None }],
        guild_id: "g".into(),
        channel_id: "c".into(),
        instance: Some(Instance {
            dev: 1,
            ino: 2,
            mtime: 3,
            mtime_nsec: 4,
        }),
    };
    let unknown = Ownership {
        instance: None,
        ..known.clone()
    };
    assert_eq!(
        control.stop_reconciled(Some(&known), PENDING).await,
        Ok(Stopped::NotRunning)
    );
    assert_eq!(
        control.stop_reconciled(Some(&unknown), PENDING).await,
        Err(ControlError::UnresolvedStart)
    );
    assert_eq!(
        control.stop_reconciled(Some(&unknown), CONFIRMED).await,
        Ok(Stopped::NotRunning)
    );
}

#[tokio::test]
async fn a_pending_start_with_nothing_live_sends_no_new_start() {
    let fake = Fake::scripted(vec![
        script(1, idle(), vec![], true),
        script(2, idle(), vec![], true),
    ]);
    let journal = Journal::new(&fake);
    let control = fake.control(Duration::from_secs(5));
    for owned in [fake.owned_by(&[9]), fake.legacy(&[9])] {
        let failed = control
            .start_reconciled(&request(), Some(&owned), PENDING, journal.checkpoint())
            .await
            .unwrap_err();
        assert_eq!(failed.error, ControlError::UnresolvedStart);
        assert_eq!(failed.ownership, None);
    }
    assert_eq!(journal.calls(), 0);
    assert!(!fake.commands().contains(&"start".to_owned()));
}

#[tokio::test]
async fn a_pending_start_for_a_replaced_service_starts_fresh() {
    let fake = Fake::scripted(vec![script(9, idle(), vec![Some(serving(Some(9)))], true)]);
    let journal = Journal::new(&fake);
    let obsolete = fake.replaced(&[3, 4]);
    let started = fake
        .control(Duration::from_secs(5))
        .start_reconciled(&request(), Some(&obsolete), PENDING, journal.checkpoint())
        .await
        .unwrap();
    assert_eq!(
        ids(&journal.reloaded().0),
        [9],
        "the obsolete candidates are gone"
    );
    assert_eq!(started.ownership, Some(fake.owned_by(&[9])));
}

#[tokio::test]
async fn a_late_single_candidate_is_attached_and_several_are_not_collapsed() {
    let fake = Fake::scripted(vec![
        script(1, status(Some(9)), vec![], true),
        script(2, status(Some(9)), vec![], true),
    ]);
    let journal = Journal::new(&fake);
    let control = fake.control(Duration::from_secs(5));
    let single = fake.owned_by(&[9]);
    let started = control
        .start_reconciled(&request(), Some(&single), PENDING, journal.checkpoint())
        .await
        .unwrap();
    assert_eq!(started.ownership, Some(single));

    let several = fake.owned_by(&[9, 12]);
    let failed = control
        .start_reconciled(&request(), Some(&several), PENDING, journal.checkpoint())
        .await
        .unwrap_err();
    assert_eq!(failed.error, ControlError::UnresolvedStart);
    assert_eq!(journal.calls(), 0);
    assert_eq!(fake.commands(), ["hello", "hello"]);
}
