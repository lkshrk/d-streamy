//! The installed launcher as the control client sees it; one test, because the install is process-wide.
#![cfg(unix)]

use std::time::Duration;

use dstreamy_obs_producer::Endpoint;
use dstreamy_obs_producer::control::{Candidate, ObsControl, Ownership, Stopped};
use dstreamy_obs_producer::launch::{self, Launcher};

#[tokio::test]
async fn a_stop_never_starts_serve_but_other_calls_do() {
    let dir = tempfile::Builder::new().prefix("ds").tempdir().unwrap();
    let endpoint = Endpoint::new(dir.path().join("d").join("s.sock"));
    let marker = dir.path().join("started");
    let script = format!("touch '{}'; exit 1", marker.display());
    assert!(launch::install(
        Launcher::new("sh", ["-c", script.as_str()], endpoint.clone())
            .with_wait(Duration::from_millis(500))
    ));
    let control = ObsControl::new(endpoint);
    let ownership = Ownership {
        connections: vec![Candidate { id: 1, fps: None }],
        guild_id: "g".into(),
        channel_id: "c".into(),
        instance: None,
    };

    let stopped = control.stop(Some(&ownership)).await;
    assert_eq!(stopped, Ok(Stopped::NotRunning));
    assert!(!marker.exists(), "stop started serve");

    let err = control.status().await.unwrap_err();
    assert!(marker.exists(), "status did not start serve");
    assert!(err.to_string().contains("exited"), "{err}");
}
