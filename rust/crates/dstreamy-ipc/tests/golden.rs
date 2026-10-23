//! Golden fixtures shared with the C plugin; `DSTREAMY_BLESS=1` rewrites the generated ones.

use std::fs;
use std::path::{Path, PathBuf};

use dstreamy_ipc::{ControlCodec, Frame, Kind, Malformed, ProducerCodec, ProducerError};
use serde_json::{Value, json};
use tokio_util::bytes::{Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn bless() -> bool {
    std::env::var_os("DSTREAMY_BLESS").is_some_and(|v| v == "1")
}

fn decode_lines(bytes: &[u8]) -> Vec<Result<Value, Malformed>> {
    let mut codec = ControlCodec::new();
    let mut buf = BytesMut::from(bytes);
    let mut out = Vec::new();
    while let Some(item) = codec.decode_eof(&mut buf).unwrap() {
        out.push(item);
    }
    out
}

fn control(name: &str) -> Vec<Result<Value, Malformed>> {
    decode_lines(&fs::read(fixtures().join("control").join(name)).unwrap())
}

#[test]
fn control_hello_lines() {
    let lines: Vec<Value> = control("hello.ndjson")
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        lines,
        [
            json!({"id": "1", "cmd": "hello", "role": "control", "protocol": 2}),
            json!({
                "id": "1", "cmd": "hello", "role": "producer", "protocol": 2,
                "video": {"codec": "h264", "width": 1280, "height": 720, "fps": 30},
                "audio": {"codec": "opus", "sample_rate": 48000, "channels": 2, "frame_ms": 20},
                "client": {"name": "obs-dstreamy", "version": "0.1.0", "obs_version": "32.2.2"}
            }),
        ]
    );
}

#[test]
fn producer_reverse_channel_events() {
    let lines: Vec<Value> = control("producer_events.ndjson")
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        lines,
        [
            json!({"attached": 1}),
            json!({"type": "keyframe_request"}),
            json!({"type": "detached", "reason": "incompatible"}),
        ]
    );
}

#[test]
fn malformed_lines_do_not_stop_decoding() {
    let lines = control("malformed.ndjson");
    assert_eq!(lines.len(), 6, "{lines:?}");
    assert!(matches!(lines[0], Err(Malformed::NotJson(_))));
    assert_eq!(lines[1], Err(Malformed::NotUtf8));
    assert!(matches!(lines[2], Err(Malformed::NotJson(_))));
    assert_eq!(lines[3], Ok(json!([1, 2])));
    assert_eq!(lines[4], Ok(json!({"id": "crlf", "cmd": "status"})));
    assert_eq!(lines[5], Ok(json!({"id": "after", "cmd": "status"})));
}

fn encode_lines(values: &[Value]) -> Vec<u8> {
    let mut buf = BytesMut::new();
    for value in values {
        ControlCodec::new().encode(value, &mut buf).unwrap();
    }
    buf.to_vec()
}

#[test]
fn canonical_control_fixtures_match_the_encoder() {
    for name in ["hello.ndjson", "producer_events.ndjson"] {
        let path = fixtures().join("control").join(name);
        let values: Vec<Value> = control(name).into_iter().map(Result::unwrap).collect();
        write_or_check(&path, &encode_lines(&values));
    }
}

fn payload(bytes: &[u8]) -> Bytes {
    Bytes::copy_from_slice(bytes)
}

fn valid_streams() -> Vec<(&'static str, Vec<Frame>)> {
    let sps_pps_idr = [
        0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1F, 0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80, 0, 0, 0, 1, 0x65,
        0x88, 0x84, 0x00,
    ];
    let p_slice = [0, 0, 0, 1, 0x41, 0x9A, 0x02];
    let silence = [0xF8, 0xFF, 0xFE];
    vec![
        (
            "av_stream",
            vec![
                Frame {
                    kind: Kind::Opus,
                    pts_us: -6_500,
                    keyframe: false,
                    payload: payload(&silence),
                },
                Frame {
                    kind: Kind::Video,
                    pts_us: 0,
                    keyframe: true,
                    payload: payload(&sps_pps_idr),
                },
                Frame {
                    kind: Kind::Opus,
                    pts_us: 13_500,
                    keyframe: false,
                    payload: payload(&silence),
                },
                Frame {
                    kind: Kind::Video,
                    pts_us: 33_333,
                    keyframe: false,
                    payload: payload(&p_slice),
                },
                Frame::end_of_stream(33_333),
            ],
        ),
        (
            "opus_keyframe_flag_ignored",
            vec![Frame {
                kind: Kind::Opus,
                pts_us: 20_000,
                keyframe: false,
                payload: payload(&silence),
            }],
        ),
        (
            "pts_extremes",
            vec![
                Frame {
                    kind: Kind::Opus,
                    pts_us: i64::MIN,
                    keyframe: false,
                    payload: Bytes::new(),
                },
                Frame {
                    kind: Kind::Video,
                    pts_us: i64::MAX,
                    keyframe: false,
                    payload: payload(&p_slice),
                },
            ],
        ),
    ]
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Video => "video",
        Kind::Opus => "opus",
        Kind::EndOfStream => "end_of_stream",
    }
}

fn describe(frames: &[Frame]) -> Value {
    let frames: Vec<Value> = frames
        .iter()
        .map(|f| {
            json!({
                "kind": kind_name(f.kind),
                "kind_byte": f.kind as u8,
                "pts_us": f.pts_us,
                "keyframe": f.keyframe,
                "payload_hex": hex(&f.payload),
            })
        })
        .collect();
    json!({ "frames": frames })
}

fn write_or_check(path: &Path, expected: &[u8]) {
    if bless() {
        fs::write(path, expected).unwrap();
    } else {
        let actual = fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(
            actual == expected,
            "{} differs from the encoder",
            path.display()
        );
    }
}

fn pretty(value: &Value) -> Vec<u8> {
    let mut text = serde_json::to_vec_pretty(value).unwrap();
    text.push(b'\n');
    text
}

#[test]
fn producer_fixtures_match_the_encoder() {
    let dir = fixtures().join("producer");
    for (name, frames) in valid_streams() {
        if name == "opus_keyframe_flag_ignored" {
            continue;
        }
        let mut bin = BytesMut::new();
        for frame in &frames {
            ProducerCodec.encode(frame, &mut bin).unwrap();
        }
        write_or_check(&dir.join(format!("{name}.bin")), &bin);
        write_or_check(
            &dir.join(format!("{name}.json")),
            &pretty(&describe(&frames)),
        );
    }
}

#[test]
fn opus_keyframe_flag_fixture_is_hand_built() {
    let bin = fs::read(fixtures().join("producer/opus_keyframe_flag_ignored.bin")).unwrap();
    assert_eq!(bin[13], 1, "the fixture no longer sets flag bit 0 on audio");
    let (_, frames) = valid_streams()
        .into_iter()
        .find(|(n, _)| *n == "opus_keyframe_flag_ignored")
        .unwrap();
    let json = fs::read(fixtures().join("producer/opus_keyframe_flag_ignored.json")).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&json).unwrap(),
        describe(&frames)
    );
}

#[test]
fn producer_fixtures_decode_to_their_description() {
    let dir = fixtures().join("producer");
    let mut seen = 0;
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "bin") {
            continue;
        }
        seen += 1;
        let spec: Value =
            serde_json::from_slice(&fs::read(path.with_extension("json")).unwrap()).unwrap();
        let mut buf = BytesMut::from(&fs::read(&path).unwrap()[..]);
        let mut frames = Vec::new();
        let result = loop {
            match ProducerCodec.decode_eof(&mut buf) {
                Ok(Some(frame)) => frames.push(frame),
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        let decoded: Vec<Value> = describe(&frames)["frames"].as_array().unwrap().clone();
        assert_eq!(
            &decoded,
            spec["frames"].as_array().unwrap(),
            "{}",
            path.display()
        );
        for (frame, expected) in frames.iter().zip(spec["frames"].as_array().unwrap()) {
            assert_eq!(
                &frame.payload[..],
                unhex(expected["payload_hex"].as_str().unwrap())
            );
        }
        match (&result, spec.get("error").and_then(Value::as_str)) {
            (Ok(()), None) => {}
            (Err(err), Some(expected)) => {
                assert_eq!(error_name(err), expected, "{}", path.display());
            }
            (result, expected) => panic!("{}: {result:?}, expected {expected:?}", path.display()),
        }
    }
    assert!(seen >= 6, "only {seen} producer fixtures");
}

fn error_name(err: &ProducerError) -> &'static str {
    match err {
        ProducerError::Oversized(_) => "oversized",
        ProducerError::TooShort(_) => "too_short",
        ProducerError::UnknownKind(_) => "unknown_kind",
        ProducerError::ReservedFlags(_) => "reserved_flags",
        ProducerError::EndOfStreamPayload(_) => "end_of_stream_payload",
        ProducerError::Truncated => "truncated",
        ProducerError::Io(_) => "io",
    }
}
