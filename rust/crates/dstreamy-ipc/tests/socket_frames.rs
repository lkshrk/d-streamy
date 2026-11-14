#![cfg(unix)]

mod common;

use dstreamy_ipc::{
    ControlCodec, Endpoint, Listener, Malformed, ProducerCodec, ProducerError, connect,
};
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio_util::codec::FramedRead;

async fn fragmented_socket(bytes: &[u8]) -> dstreamy_ipc::Stream {
    let dir = tempfile::tempdir().unwrap();
    let endpoint = Endpoint::new(dir.path().join("d/s.sock"));
    let listener = Listener::bind(&endpoint).await.unwrap();
    let mut client = connect(&endpoint).await.unwrap();
    let (server, _) = listener.accept().await.unwrap();
    let bytes = bytes.to_vec();
    tokio::spawn(async move {
        let _dir = dir;
        let _listener = listener;
        for byte in bytes {
            client.write_all(&[byte]).await.unwrap();
            tokio::task::yield_now().await;
        }
        client.shutdown().await.unwrap();
    });
    server
}

#[tokio::test]
async fn malformed_control_lines_on_a_socket_do_not_hide_the_next_request() {
    common::within(async {
        let stream =
            fragmented_socket(b"{broken\n\xff\n{\"id\":\"next\",\"cmd\":\"status\"}\n").await;
        let mut lines = FramedRead::new(stream, ControlCodec::new());
        assert!(matches!(
            lines.next().await.unwrap().unwrap(),
            Err(Malformed::NotJson(_))
        ));
        assert_eq!(
            lines.next().await.unwrap().unwrap(),
            Err(Malformed::NotUtf8)
        );
        assert_eq!(
            lines.next().await.unwrap().unwrap().unwrap(),
            serde_json::json!({"id":"next","cmd":"status"})
        );
        assert!(lines.next().await.is_none());
    })
    .await;
}

#[tokio::test]
async fn every_malformed_producer_fixture_fails_after_its_valid_prefix_over_a_socket() {
    common::within(async {
        let fixtures =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/producer");
        for name in [
            "oversized",
            "too_short",
            "unknown_kind",
            "reserved_flags",
            "end_of_stream_payload",
            "truncated",
        ] {
            let bytes = std::fs::read(fixtures.join(format!("err_{name}.bin"))).unwrap();
            let spec: serde_json::Value = serde_json::from_slice(
                &std::fs::read(fixtures.join(format!("err_{name}.json"))).unwrap(),
            )
            .unwrap();
            let stream = fragmented_socket(&bytes).await;
            let mut frames = FramedRead::new(stream, ProducerCodec);
            for _ in spec["frames"].as_array().unwrap() {
                frames.next().await.unwrap().unwrap();
            }
            let error = frames.next().await.unwrap().unwrap_err();
            let actual = match error {
                ProducerError::Oversized(_) => "oversized",
                ProducerError::TooShort(_) => "too_short",
                ProducerError::UnknownKind(_) => "unknown_kind",
                ProducerError::ReservedFlags(_) => "reserved_flags",
                ProducerError::EndOfStreamPayload(_) => "end_of_stream_payload",
                ProducerError::Truncated => "truncated",
                ProducerError::Io(e) => panic!("{name}: {e}"),
            };
            assert_eq!(actual, name, "{name}");
        }
    })
    .await;
}

#[tokio::test]
async fn half_closing_a_socket_at_every_byte_of_a_frame_reports_truncation() {
    common::within(async {
        let mut bytes = vec![0, 0, 0, 13, 1];
        bytes.extend_from_slice(&0i64.to_be_bytes());
        bytes.extend_from_slice(&[1, 0x65, 0x88, 0x84]);
        for end in 1..bytes.len() {
            let stream = fragmented_socket(&bytes[..end]).await;
            let mut frames = FramedRead::new(stream, ProducerCodec);
            assert!(
                matches!(frames.next().await, Some(Err(ProducerError::Truncated))),
                "closed at byte {end}"
            );
        }
        let stream = fragmented_socket(&bytes).await;
        let mut frames = FramedRead::new(stream, ProducerCodec);
        assert_eq!(
            frames.next().await.unwrap().unwrap().payload.as_ref(),
            &[0x65, 0x88, 0x84]
        );
        assert!(frames.next().await.is_none());
    })
    .await;
}
