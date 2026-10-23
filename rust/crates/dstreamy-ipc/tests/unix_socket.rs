#![cfg(unix)]

use std::fs::{self, Permissions};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use dstreamy_ipc::{
    ControlCodec, Endpoint, Frame, IpcError, Kind, Listener, ProducerCodec, connect,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::bytes::Bytes;
use tokio_util::codec::{Framed, FramedRead};

fn endpoint_in(root: &Path) -> Endpoint {
    Endpoint::new(root.join("dstreamy").join("serve.sock"))
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

#[tokio::test]
async fn bind_connect_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let endpoint = endpoint_in(tmp.path());
    let listener = Listener::bind(&endpoint).await.unwrap();
    assert_eq!(mode(endpoint.as_path().parent().unwrap()), 0o700);

    let server = tokio::spawn(async move {
        let (stream, peer) = listener.accept().await.unwrap();
        let mut control = Framed::new(stream, ControlCodec::new());
        let hello = control.next().await.unwrap().unwrap().unwrap();
        control.send(json!({"protocol": 2})).await.unwrap();
        let mut frames = FramedRead::new(control.into_inner(), ProducerCodec);
        let frame = frames.next().await.unwrap().unwrap();
        assert!(frames.next().await.is_none());
        (peer, hello, frame, listener.rejected_peers())
    });

    let stream = connect(&endpoint).await.unwrap();
    let mut control = Framed::new(stream, ControlCodec::new());
    let hello = json!({"id": "1", "cmd": "hello", "role": "producer", "protocol": 2});
    control.send(&hello).await.unwrap();
    let reply: Value = control.next().await.unwrap().unwrap().unwrap();
    assert_eq!(reply, json!({"protocol": 2}));
    let frame = Frame {
        kind: Kind::Video,
        pts_us: 33_333,
        keyframe: true,
        payload: Bytes::from_static(&[0, 0, 0, 1, 0x65, 0x88]),
    };
    let mut producer = Framed::new(control.into_inner(), ProducerCodec);
    producer.send(frame.clone()).await.unwrap();
    SinkExt::<Frame>::close(&mut producer).await.unwrap();
    drop(producer);

    let (peer, got_hello, got_frame, rejected) = server.await.unwrap();
    assert_eq!(got_hello, hello);
    assert_eq!(got_frame, frame);
    assert_eq!(rejected, 0);
    assert!(peer.is_current_user());
    assert_eq!(peer.pid(), Some(std::process::id()));
}

#[tokio::test]
async fn oversized_control_line_keeps_the_connection_usable() {
    let tmp = tempfile::tempdir().unwrap();
    let endpoint = endpoint_in(tmp.path());
    let listener = Listener::bind(&endpoint).await.unwrap();
    let client = tokio::spawn({
        let endpoint = endpoint.clone();
        async move {
            let mut stream = connect(&endpoint).await.unwrap();
            let big = vec![b'a'; dstreamy_ipc::control::MAX_LINE * 2];
            stream.write_all(&big).await.unwrap();
            stream.write_all(b"\n{\"id\":\"2\"}\n").await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });
    let (stream, _) = listener.accept().await.unwrap();
    let items: Vec<_> = FramedRead::new(stream, ControlCodec::new())
        .map(Result::unwrap)
        .collect()
        .await;
    client.await.unwrap();
    assert_eq!(
        items,
        [
            Err(dstreamy_ipc::Malformed::Oversized),
            Ok(json!({"id": "2"}))
        ]
    );
}

#[tokio::test]
async fn second_bind_is_already_running() {
    let tmp = tempfile::tempdir().unwrap();
    let endpoint = endpoint_in(tmp.path());
    let first = Listener::bind(&endpoint).await.unwrap();
    let err = Listener::bind(&endpoint).await.err().unwrap();
    assert!(matches!(err, IpcError::AlreadyRunning(_)), "{err}");
    assert!(
        endpoint.as_path().exists(),
        "the second bind removed the live socket"
    );
    let (mut probe, _) = first.accept().await.unwrap();
    let mut buf = [0u8; 1];
    assert_eq!(
        probe.read(&mut buf).await.unwrap(),
        0,
        "the probe sent data"
    );
    let (accepted, connected) = tokio::join!(first.accept(), connect(&endpoint));
    accepted.unwrap();
    connected.unwrap();
    assert_eq!(first.rejected_peers(), 0);
}

#[tokio::test]
async fn stale_socket_is_replaced() {
    let tmp = tempfile::tempdir().unwrap();
    let endpoint = endpoint_in(tmp.path());
    drop(Listener::bind(&endpoint).await.unwrap());
    assert!(
        endpoint.as_path().exists(),
        "dropping a listener left no socket file"
    );
    let err = connect(&endpoint).await.err().unwrap();
    assert!(matches!(err, IpcError::NotRunning(_)), "{err}");

    let listener = Listener::bind(&endpoint).await.unwrap();
    let client = connect(&endpoint);
    let (accepted, connected) = tokio::join!(listener.accept(), client);
    accepted.unwrap();
    connected.unwrap();
}

#[tokio::test]
async fn non_socket_file_is_not_replaced() {
    let tmp = tempfile::tempdir().unwrap();
    let endpoint = endpoint_in(tmp.path());
    let dir = endpoint.as_path().parent().unwrap();
    fs::create_dir(dir).unwrap();
    fs::set_permissions(dir, Permissions::from_mode(0o700)).unwrap();
    fs::write(endpoint.as_path(), b"keep me").unwrap();
    let err = Listener::bind(&endpoint).await.err().unwrap();
    assert!(matches!(err, IpcError::NotASocket(_)), "{err}");
    assert_eq!(fs::read(endpoint.as_path()).unwrap(), b"keep me");
}

#[tokio::test]
async fn wrong_directory_mode_is_refused() {
    for bad in [0o755, 0o750, 0o701, 0o710, 0o1700] {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = endpoint_in(tmp.path());
        let dir = endpoint.as_path().parent().unwrap();
        fs::create_dir(dir).unwrap();
        fs::set_permissions(dir, Permissions::from_mode(bad)).unwrap();
        let err = Listener::bind(&endpoint).await.err().unwrap();
        assert!(
            matches!(err, IpcError::InsecureDirectory { .. }),
            "{bad:o}: {err}"
        );
        assert_eq!(mode(dir), bad, "bind changed an existing directory");
        assert!(!endpoint.as_path().exists());
    }
}

#[tokio::test]
async fn symlinked_directory_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("elsewhere");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink(&target, tmp.path().join("dstreamy")).unwrap();
    let err = Listener::bind(&endpoint_in(tmp.path()))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, IpcError::InsecureDirectory { .. }), "{err}");
}

#[tokio::test]
async fn existing_private_directory_is_reused() {
    let tmp = tempfile::tempdir().unwrap();
    let endpoint = endpoint_in(tmp.path());
    let dir = endpoint.as_path().parent().unwrap();
    fs::create_dir(dir).unwrap();
    fs::set_permissions(dir, Permissions::from_mode(0o700)).unwrap();
    Listener::bind(&endpoint).await.unwrap();
}

#[tokio::test]
async fn over_long_socket_path_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let endpoint = endpoint_in(&tmp.path().join("p".repeat(110)));
    let err = Listener::bind(&endpoint).await.err().unwrap();
    assert!(matches!(err, IpcError::PathTooLong(_)), "{err}");
    let err = connect(&endpoint).await.err().unwrap();
    assert!(matches!(err, IpcError::PathTooLong(_)), "{err}");
}

#[tokio::test]
async fn concurrent_binds_leave_exactly_one_live_listener() {
    for _ in 0..20 {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = endpoint_in(tmp.path());
        drop(Listener::bind(&endpoint).await.unwrap());
        let (a, b) = tokio::join!(
            tokio::spawn({
                let e = endpoint.clone();
                async move { Listener::bind(&e).await }
            }),
            tokio::spawn({
                let e = endpoint.clone();
                async move { Listener::bind(&e).await }
            }),
        );
        let results = [a.unwrap(), b.unwrap()];
        let live: Vec<&Listener> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
        assert_eq!(
            live.len(),
            1,
            "{:?}",
            results.iter().map(|r| r.as_ref().err()).collect::<Vec<_>>()
        );
        for err in results.iter().filter_map(|r| r.as_ref().err()) {
            assert!(matches!(err, IpcError::AlreadyRunning(_)), "{err}");
        }
        let (accepted, connected) = tokio::join!(live[0].accept(), connect(&endpoint));
        accepted.unwrap();
        connected.unwrap();
    }
}

#[tokio::test]
async fn client_refuses_a_directory_that_is_not_private() {
    let tmp = tempfile::tempdir().unwrap();
    let endpoint = endpoint_in(tmp.path());
    let listener = Listener::bind(&endpoint).await.unwrap();
    let dir = endpoint.as_path().parent().unwrap();
    for bad in [0o755, 0o777, 0o711] {
        fs::set_permissions(dir, Permissions::from_mode(bad)).unwrap();
        let err = connect(&endpoint).await.err().unwrap();
        assert!(
            matches!(err, IpcError::InsecureDirectory { .. }),
            "{bad:o}: {err}"
        );
    }
    fs::set_permissions(dir, Permissions::from_mode(0o700)).unwrap();
    let (accepted, connected) = tokio::join!(listener.accept(), connect(&endpoint));
    accepted.unwrap();
    connected.unwrap();
}

#[tokio::test]
async fn client_refuses_a_symlinked_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let real = endpoint_in(&tmp.path().join("real"));
    let _listener = Listener::bind(&real).await.unwrap();
    let link_root = tmp.path().join("link");
    fs::create_dir(&link_root).unwrap();
    std::os::unix::fs::symlink(real.as_path().parent().unwrap(), link_root.join("dstreamy"))
        .unwrap();
    let err = connect(&endpoint_in(&link_root)).await.err().unwrap();
    assert!(matches!(err, IpcError::InsecureDirectory { .. }), "{err}");
}

#[tokio::test]
async fn client_reports_a_missing_directory_as_not_running() {
    let tmp = tempfile::tempdir().unwrap();
    let err = connect(&endpoint_in(tmp.path())).await.err().unwrap();
    assert!(matches!(err, IpcError::NotRunning(_)), "{err}");
}
