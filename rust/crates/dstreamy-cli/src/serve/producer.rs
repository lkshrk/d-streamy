//! The `producer` role (ADR 0004): binary `ProducerCodec` frames in, NDJSON events out.

use std::time::Duration;

use dstreamy_core::{Fps, Resolution};
use dstreamy_ipc::{ControlCodec, Frame, Kind, ProducerCodec, ProducerError};
use dstreamy_media::{
    AttachError, Attachment, CloseReason, Ingest, ProducerFrame, ProducerInfo, SourceState,
    VideoCodec,
};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;
use tokio::time::timeout;
use tokio_util::codec::{Encoder, FramedRead, FramedWrite};

use super::Phase;
use crate::engine::{Failure, SourceView};

const OPUS_SAMPLE_RATE: u64 = 48_000;
const OPUS_CHANNELS: u64 = 2;
const OPUS_FRAME_MS: u64 = 20;

fn invalid(what: &str) -> Failure {
    crate::engine::invalid_params(what)
}

fn incompatible(message: String) -> Failure {
    Failure::new("incompatible", message)
}

fn object<'a>(
    fields: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a Map<String, Value>, Failure> {
    fields
        .get(key)
        .and_then(Value::as_object)
        .ok_or_else(|| invalid(&format!("missing object field \"{key}\"")))
}

fn text<'a>(fields: &'a Map<String, Value>, path: &str) -> Result<&'a str, Failure> {
    let key = path.rsplit('.').next().unwrap_or(path);
    fields
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(&format!("missing string field \"{path}\"")))
}

fn number(fields: &Map<String, Value>, path: &str) -> Result<u64, Failure> {
    let key = path.rsplit('.').next().unwrap_or(path);
    fields
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid(&format!("missing integer field \"{path}\"")))
}

fn expect(path: &str, got: u64, want: u64) -> Result<(), Failure> {
    if got == want {
        return Ok(());
    }
    Err(incompatible(format!("{path} {got}: must be {want}")))
}

/// The declared media of a producer `hello`: H.264 at a size `Resolution` takes and an fps `Fps` takes, Opus 48 kHz stereo 20 ms.
pub(super) fn hello(fields: &Map<String, Value>) -> Result<ProducerInfo, Failure> {
    let video = object(fields, "video")?;
    let audio = object(fields, "audio")?;
    let codec = text(video, "video.codec")?;
    let width = number(video, "video.width")?;
    let height = number(video, "video.height")?;
    let fps = number(video, "video.fps")?;
    let audio_codec = text(audio, "audio.codec")?;
    let sample_rate = number(audio, "audio.sample_rate")?;
    let channels = number(audio, "audio.channels")?;
    let frame_ms = number(audio, "audio.frame_ms")?;
    if codec != "h264" {
        return Err(incompatible(format!(
            "video.codec {codec:?}: only h264 is streamed"
        )));
    }
    let (w, h) = (clamp(width), clamp(height));
    if let Err(e) = Resolution::new(w, h) {
        let field = if Resolution::new(w, 2).is_err() {
            "video.width"
        } else {
            "video.height"
        };
        return Err(incompatible(format!(
            "{field}: {e}; the size must be even and at most {}x{}",
            Resolution::MAX_WIDTH,
            Resolution::MAX_HEIGHT
        )));
    }
    let fps = Fps::try_from(clamp(fps))
        .map_err(|_| incompatible(format!("video.fps {fps}: must be 15, 30 or 60")))?;
    if audio_codec != "opus" {
        return Err(incompatible(format!(
            "audio.codec {audio_codec:?}: audio encoder must be Opus"
        )));
    }
    expect("audio.sample_rate", sample_rate, OPUS_SAMPLE_RATE)?;
    expect("audio.channels", channels, OPUS_CHANNELS)?;
    expect("audio.frame_ms", frame_ms, OPUS_FRAME_MS)?;
    Ok(ProducerInfo {
        codec: VideoCodec::H264,
        fps: fps.get(),
        width: w,
        height: h,
    })
}

fn clamp(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

pub(super) fn attach_failure(e: AttachError) -> Failure {
    match e {
        AttachError::NoSession => Failure::new(
            "no_session",
            "no session with target external is live; start one first",
        ),
        AttachError::Busy => Failure::new("busy", "another producer is attached"),
        AttachError::Incompatible(message) => incompatible(message),
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Event {
    KeyframeRequest,
    Source {
        #[serde(flatten)]
        view: SourceView,
    },
    Detached {
        reason: &'static str,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Detach {
    Stopped(String),
    Shutdown,
    Incompatible(String),
    Oversized(String),
}

impl Detach {
    fn event(&self) -> Event {
        let (reason, message) = match self {
            Self::Stopped(m) => ("stopped", m.clone()),
            Self::Shutdown => ("shutdown", "the service is shutting down".to_owned()),
            Self::Incompatible(m) => ("incompatible", m.clone()),
            Self::Oversized(m) => ("oversized", m.clone()),
        };
        Event::Detached { reason, message }
    }

    fn close_reason(&self) -> CloseReason {
        match self {
            Self::Incompatible(m) => CloseReason::Incompatible(m.clone()),
            Self::Oversized(m) => CloseReason::Failed(m.clone()),
            Self::Stopped(_) | Self::Shutdown => CloseReason::Ended,
        }
    }
}

/// `Ok(None)` is end of stream.
fn admit(ingest: &mut Ingest, frame: Frame) -> Result<Option<ProducerFrame>, String> {
    match frame.kind {
        Kind::Video => {
            ingest
                .video(&frame.payload, frame.pts_us, frame.keyframe)
                .map_err(|e| e.to_string())?;
            Ok(Some(ProducerFrame::Video {
                data: frame.payload.into(),
                pts_us: frame.pts_us,
                keyframe: frame.keyframe,
            }))
        }
        Kind::Opus => {
            ingest
                .audio(&frame.payload, frame.pts_us)
                .map_err(|e| e.to_string())?;
            Ok(Some(ProducerFrame::Audio {
                data: frame.payload.into(),
                pts_us: frame.pts_us,
            }))
        }
        Kind::EndOfStream => Ok(None),
    }
}

fn queue<W>(writer: &mut FramedWrite<W, ControlCodec>, event: &Event) {
    if let Err(e) = ControlCodec::new().encode(event, writer.write_buffer_mut()) {
        tracing::error!("encoding a producer event: {e}");
    }
}

/// Feeds an attachment from `reader` and reports to `writer` until either side ends it.
pub(super) async fn run<R, W>(
    attachment: Attachment,
    mut states: watch::Receiver<SourceState>,
    mut reader: FramedRead<R, ProducerCodec>,
    mut writer: FramedWrite<W, ControlCodec>,
    mut phase: watch::Receiver<Phase>,
    close_flush: Duration,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut requests = attachment.keyframe_requests();
    let mut ingest = Ingest::default();
    let (mut keyframe_due, mut source_due, mut states_open) = (false, true, true);
    let current = phase.clone();
    let stopped = |attachment: &Attachment| {
        if *current.borrow() != Phase::Running {
            return Detach::Shutdown;
        }
        match attachment.close_reason() {
            Some(CloseReason::Incompatible(m)) => Detach::Incompatible(m),
            Some(reason) => Detach::Stopped(reason.to_string()),
            None => Detach::Stopped(CloseReason::SourceGone.to_string()),
        }
    };
    let end = loop {
        // Events wait while earlier ones are unsent, so a producer that never reads costs one line of buffer.
        if writer.write_buffer().is_empty() {
            if std::mem::take(&mut keyframe_due) {
                queue(&mut writer, &Event::KeyframeRequest);
            } else if std::mem::take(&mut source_due) {
                let view = SourceView::from(&*states.borrow_and_update());
                queue(&mut writer, &Event::Source { view });
            }
        }
        let unflushed = !writer.write_buffer().is_empty();
        tokio::select! {
            biased;
            _ = phase.wait_for(|p| *p != Phase::Running) => break Some(Detach::Shutdown),
            changed = requests.changed() => match changed {
                Ok(()) => keyframe_due = true,
                Err(_) => break Some(stopped(&attachment)),
            },
            changed = states.changed(), if states_open => match changed {
                Ok(()) => source_due = true,
                Err(_) => states_open = false,
            },
            flushed = SinkExt::<&Event>::flush(&mut writer), if unflushed => {
                if flushed.is_err() {
                    break None;
                }
            }
            frame = reader.next() => match frame {
                None | Some(Err(ProducerError::Io(_) | ProducerError::Truncated)) => break None,
                Some(Err(e @ ProducerError::Oversized(_))) => break Some(Detach::Oversized(e.to_string())),
                Some(Err(e)) => break Some(Detach::Incompatible(e.to_string())),
                Some(Ok(frame)) => match admit(&mut ingest, frame) {
                    Ok(Some(media)) => {
                        if attachment.send(media).is_err() {
                            break Some(stopped(&attachment));
                        }
                    }
                    Ok(None) => {
                        attachment.close(CloseReason::Ended);
                        return;
                    }
                    Err(message) => break Some(Detach::Incompatible(message)),
                },
            },
        }
    };
    let Some(detach) = end else {
        return;
    };
    tracing::info!(
        attachment = attachment.id(),
        ?detach,
        "detaching the producer"
    );
    attachment.close(detach.close_reason());
    queue(&mut writer, &detach.event());
    let _ = timeout(close_flush, SinkExt::<&Event>::flush(&mut writer)).await;
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use dstreamy_ipc::producer::MAX_FRAME_LEN;
    use dstreamy_media::{
        CaptureConfig, CaptureSource, ExternalOptions, ExternalSource, FrameReceiver,
    };
    use serde_json::json;
    use tokio::io::{AsyncWriteExt, DuplexStream};
    use tokio_util::bytes::{BufMut, Bytes, BytesMut};
    use tokio_util::codec::Decoder;

    use super::*;

    const T: Duration = Duration::from_secs(10);
    const SPS_PPS_IDR: &[u8] = &[
        0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1F, 0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80, 0, 0, 0, 1, 0x65,
        0x88, 0x84, 0x00,
    ];
    const P_SLICE: &[u8] = &[0, 0, 0, 1, 0x41, 0x9A, 0x02];
    const SILENCE: &[u8] = &[0xF8, 0xFF, 0xFE];

    fn producer_hello() -> Value {
        json!({
            "id": "1", "cmd": "hello", "role": "producer", "protocol": 2,
            "video": {"codec": "h264", "width": 1280, "height": 720, "fps": 30},
            "audio": {"codec": "opus", "sample_rate": 48000, "channels": 2, "frame_ms": 20},
            "client": {"name": "obs-dstreamy", "version": "0.1.0", "obs_version": "32.2.2"}
        })
    }

    fn checked(mut line: Value, path: &[&str], value: Value) -> Result<ProducerInfo, Failure> {
        let mut at = &mut line;
        for key in &path[..path.len() - 1] {
            at = &mut at[*key];
        }
        at[path[path.len() - 1]] = value;
        hello(line.as_object().unwrap())
    }

    #[test]
    fn a_producer_hello_declares_its_video() {
        let info = hello(producer_hello().as_object().unwrap()).unwrap();
        assert_eq!(
            info,
            ProducerInfo {
                codec: VideoCodec::H264,
                fps: 30,
                width: 1280,
                height: 720
            }
        );
    }

    #[test]
    fn an_incompatible_hello_names_the_field() {
        for (path, value, field) in [
            (&["video", "codec"][..], json!("h265"), "video.codec"),
            (&["video", "width"], json!(1281), "video.width"),
            (&["video", "width"], json!(1922), "video.width"),
            (&["video", "width"], json!(0), "video.width"),
            (&["video", "width"], json!(u64::MAX), "video.width"),
            (&["video", "height"], json!(1082), "video.height"),
            (&["video", "height"], json!(721), "video.height"),
            (&["video", "fps"], json!(25), "video.fps"),
            (&["video", "fps"], json!(0), "video.fps"),
            (&["audio", "codec"], json!("aac"), "audio.codec"),
            (&["audio", "sample_rate"], json!(44100), "audio.sample_rate"),
            (&["audio", "channels"], json!(1), "audio.channels"),
            (&["audio", "frame_ms"], json!(10), "audio.frame_ms"),
        ] {
            let f = checked(producer_hello(), path, value.clone()).unwrap_err();
            assert_eq!(f.code, "incompatible", "{path:?} = {value}: {}", f.message);
            assert!(
                f.message.starts_with(field),
                "{path:?} = {value}: {}",
                f.message
            );
        }
        let aac = checked(producer_hello(), &["audio", "codec"], json!("aac")).unwrap_err();
        assert!(
            aac.message.contains("audio encoder must be Opus"),
            "{}",
            aac.message
        );
    }

    #[test]
    fn a_hello_missing_a_field_is_invalid_params() {
        for (path, value) in [
            (&["video"][..], Value::Null),
            (&["audio"], json!("opus")),
            (&["video", "width"], json!("1280")),
            (&["video", "fps"], json!(-30)),
            (&["audio", "channels"], Value::Null),
            (&["video", "codec"], json!(264)),
        ] {
            let f = checked(producer_hello(), path, value.clone()).unwrap_err();
            assert_eq!(
                f.code, "invalid_params",
                "{path:?} = {value}: {}",
                f.message
            );
        }
    }

    fn fixture(name: &str) -> Vec<Value> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../dstreamy-ipc/tests/fixtures/control")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn encoded(event: &Event) -> Value {
        let mut buf = BytesMut::new();
        ControlCodec::new().encode(event, &mut buf).unwrap();
        serde_json::from_slice(&buf).unwrap()
    }

    #[test]
    fn events_match_the_shared_fixture() {
        let events: Vec<Value> = [
            Event::KeyframeRequest,
            Event::Source {
                view: SourceView::from(&SourceState::Producing { attachment: 1 }),
            },
            Event::Source {
                view: SourceView::from(&SourceState::Detached),
            },
            Detach::Stopped("no media within the grace period".into()).event(),
            Detach::Shutdown.event(),
            Detach::Incompatible(
                "B-frames are enabled: video pts 0 µs does not follow 33333 µs".into(),
            )
            .event(),
            Detach::Oversized("frame length 8388609 exceeds the 8388608-byte limit".into()).event(),
        ]
        .iter()
        .map(encoded)
        .collect();
        let mut expected = fixture("producer_events.ndjson");
        expected.retain(|v| v.get("attached").is_none());
        assert_eq!(events, expected);
    }

    #[test]
    fn hello_replies_match_the_shared_fixture() {
        for case in fixture("producer_hello_replies.ndjson") {
            let name = case["case"].as_str().unwrap();
            let reply = &case["reply"];
            let failure = match name {
                "attached" => {
                    assert!(hello(case["request"].as_object().unwrap()).is_ok());
                    continue;
                }
                "no_session" => attach_failure(AttachError::NoSession),
                "busy" => attach_failure(AttachError::Busy),
                "unsupported_protocol" => match super::super::handshake(&case["request"]) {
                    Err((_, f)) => f,
                    Ok(_) => panic!("{name} was accepted"),
                },
                _ => hello(case["request"].as_object().unwrap()).unwrap_err(),
            };
            assert_eq!(reply["code"], failure.code, "{name}");
            assert_eq!(reply["message"], failure.message, "{name}");
        }
    }

    #[test]
    fn an_aac_hello_is_refused_before_any_packet() {
        let mut line = producer_hello();
        line["audio"]["codec"] = json!("aac");
        let f = hello(line.as_object().unwrap()).unwrap_err();
        assert_eq!(f.code, "incompatible");
        assert!(
            f.message.contains("audio encoder must be Opus"),
            "{}",
            f.message
        );
    }

    struct Rig {
        source: ExternalSource,
        _frames: FrameReceiver,
        client: DuplexStream,
        events: tokio::task::JoinHandle<()>,
        phase: watch::Sender<Phase>,
    }

    /// A started source with one attachment served over a duplex whose buffers hold `buffer` bytes.
    async fn rig(buffer: usize) -> Rig {
        let mut source = ExternalSource::new(ExternalOptions::default()).unwrap();
        let handle = source.handle();
        let attachment = handle
            .attach(ProducerInfo {
                codec: VideoCodec::H264,
                fps: 30,
                width: 320,
                height: 180,
            })
            .unwrap();
        let frames = source
            .start(CaptureConfig {
                width: 320,
                height: 180,
                fps: 30,
                ..CaptureConfig::default()
            })
            .await
            .unwrap();
        let (client, service) = tokio::io::duplex(buffer);
        let (read, write) = tokio::io::split(service);
        let (phase, phase_rx) = watch::channel(Phase::Running);
        let events = tokio::spawn(run(
            attachment,
            handle.state(),
            FramedRead::new(read, ProducerCodec::new()),
            FramedWrite::new(write, ControlCodec::new()),
            phase_rx,
            Duration::from_millis(100),
        ));
        Rig {
            source,
            _frames: frames,
            client,
            events,
            phase,
        }
    }

    fn frame(kind: Kind, pts_us: i64, keyframe: bool, payload: &[u8]) -> BytesMut {
        let mut buf = BytesMut::new();
        ProducerCodec::new()
            .encode(
                Frame {
                    kind,
                    pts_us,
                    keyframe,
                    payload: Bytes::copy_from_slice(payload),
                },
                &mut buf,
            )
            .unwrap();
        buf
    }

    async fn read_events(client: &mut DuplexStream) -> Vec<Value> {
        let mut out = Vec::new();
        let mut reader = FramedRead::new(client, ControlCodec::new());
        while let Ok(Some(Ok(Ok(line)))) = timeout(T, reader.next()).await {
            out.push(line);
        }
        out
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn frames_are_read_while_the_producer_never_reads_its_events() {
        let mut r = rig(64).await;
        let handle = r.source.handle();
        let count = 2_000;
        let writes = async {
            r.client
                .write_all(&frame(Kind::Video, 0, true, SPS_PPS_IDR))
                .await
                .unwrap();
            for i in 1..count {
                let video = frame(Kind::Video, i * 33_333, false, P_SLICE);
                let audio = frame(Kind::Opus, i * 20_000, false, SILENCE);
                r.client.write_all(&video).await.unwrap();
                r.client.write_all(&audio).await.unwrap();
            }
        };
        timeout(T, writes)
            .await
            .expect("the service stopped reading frames while its events were unread");
        let stats = handle.stats().unwrap();
        let seen = stats.lost.skipped + stats.lost.dropped_video + stats.lost.dropped_audio;
        assert!(seen > 0, "{stats:?}");
        r.phase.send_replace(Phase::Stopping);
        let events = read_events(&mut r.client).await;
        assert!(
            events.contains(&json!({"type": "keyframe_request"})),
            "{events:?}"
        );
        timeout(T, r.events).await.unwrap().unwrap();
    }

    async fn detached_after(sent: &[BytesMut]) -> (Value, SourceState) {
        let mut r = rig(1 << 16).await;
        let mut states = r.source.handle().state();
        for bytes in sent {
            r.client.write_all(bytes).await.unwrap();
        }
        let events = read_events(&mut r.client).await;
        timeout(T, r.events).await.unwrap().unwrap();
        let state = *states.borrow_and_update();
        let detached = events
            .into_iter()
            .find(|e| e["type"] == "detached")
            .expect("no detached event");
        (detached, state)
    }

    /// Every `producer/ingest_*` fixture: its name, its bytes and the `detached` it expects.
    fn ingest_fixtures() -> Vec<(String, Vec<u8>, Value)> {
        let dir =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../dstreamy-ipc/tests/fixtures/producer");
        let mut out: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.extension().is_some_and(|e| e == "bin")
                    && p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("ingest_")
            })
            .map(|p| {
                let spec: Value =
                    serde_json::from_slice(&std::fs::read(p.with_extension("json")).unwrap())
                        .unwrap();
                let name = p.file_stem().unwrap().to_string_lossy().into_owned();
                (name, std::fs::read(&p).unwrap(), spec["detached"].clone())
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    #[test]
    fn every_ingest_fixture_is_refused_with_its_message() {
        let fixtures = ingest_fixtures();
        assert_eq!(fixtures.len(), 5);
        for (name, bytes, detached) in fixtures {
            assert_eq!(detached["reason"], "incompatible", "{name}");
            let mut buf = BytesMut::from(&bytes[..]);
            let mut ingest = Ingest::default();
            let mut refused = None;
            while let Some(frame) = ProducerCodec::new().decode_eof(&mut buf).unwrap() {
                if let Err(message) = admit(&mut ingest, frame) {
                    refused = Some(message);
                    break;
                }
            }
            let message = refused.unwrap_or_else(|| panic!("{name} was accepted"));
            let want = detached["message_contains"].as_str().unwrap();
            assert!(message.contains(want), "{name}: {message}");
        }
    }

    #[tokio::test]
    async fn an_ingest_fixture_sent_to_serve_detaches_with_its_message() {
        let (name, bytes, want) = ingest_fixtures()
            .into_iter()
            .find(|(n, ..)| n == "ingest_pts_ne_dts")
            .unwrap();
        let (detached, _) = detached_after(&[BytesMut::from(&bytes[..])]).await;
        assert_eq!(detached["reason"], want["reason"], "{name}");
        let contains = want["message_contains"].as_str().unwrap();
        assert!(
            detached["message"].as_str().unwrap().contains(contains),
            "{detached}"
        );
    }

    fn keyframe() -> BytesMut {
        frame(Kind::Video, 0, true, SPS_PPS_IDR)
    }

    #[tokio::test]
    async fn an_ingest_failure_detaches_incompatible_and_the_source_waits_in_grace() {
        let cases: [(&str, Vec<BytesMut>, &str); 6] = [
            (
                "no start code",
                vec![keyframe(), frame(Kind::Video, 33_333, false, &P_SLICE[4..])],
                "not Annex B",
            ),
            (
                "IDR without flag",
                vec![keyframe(), frame(Kind::Video, 33_333, false, SPS_PPS_IDR)],
                "IDR slice in a frame not flagged",
            ),
            (
                "keyframe without SPS",
                vec![frame(Kind::Video, 0, true, &SPS_PPS_IDR[8..])],
                "no SPS",
            ),
            (
                "pts not dts",
                vec![
                    keyframe(),
                    frame(Kind::Video, 66_666, false, P_SLICE),
                    frame(Kind::Video, 33_333, false, P_SLICE),
                ],
                "B-frames are enabled",
            ),
            (
                "opus 40 ms",
                vec![keyframe(), frame(Kind::Opus, 0, false, &[0xF9, 0xFF, 0xFE])],
                "not 20 ms",
            ),
            (
                "reserved flag",
                vec![{
                    let mut b = keyframe();
                    b[13] = 0b10;
                    b
                }],
                "reserved flag",
            ),
        ];
        for (name, sent, message) in cases {
            let (detached, state) = detached_after(&sent).await;
            assert_eq!(detached["reason"], "incompatible", "{name}: {detached}");
            assert!(
                detached["message"].as_str().unwrap().contains(message),
                "{name}: {detached}"
            );
            assert!(
                matches!(state, SourceState::Interrupted { .. }),
                "{name}: {state:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_oversized_frame_detaches_oversized() {
        let mut header = BytesMut::new();
        header.put_u32(MAX_FRAME_LEN + 1);
        let (detached, state) = detached_after(&[keyframe(), header]).await;
        assert_eq!(detached["reason"], "oversized", "{detached}");
        assert!(
            matches!(state, SourceState::Interrupted { .. }),
            "{state:?}"
        );
    }

    #[tokio::test]
    async fn a_service_shutdown_detaches_shutdown() {
        let mut r = rig(1 << 16).await;
        r.phase.send_replace(Phase::Stopping);
        let events = read_events(&mut r.client).await;
        assert_eq!(
            events.last().map(|e| (&e["type"], &e["reason"])),
            Some((&json!("detached"), &json!("shutdown"))),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn a_session_that_ends_detaches_stopped() {
        let mut r = rig(1 << 16).await;
        r.source.handle().end();
        let events = read_events(&mut r.client).await;
        assert_eq!(
            events.last().map(|e| (&e["type"], &e["reason"])),
            Some((&json!("detached"), &json!("stopped"))),
            "{events:?}"
        );
        timeout(T, r.events).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn end_of_stream_closes_without_an_event() {
        let mut r = rig(1 << 16).await;
        let mut states = r.source.handle().state();
        r.client.write_all(&keyframe()).await.unwrap();
        r.client
            .write_all(&frame(Kind::EndOfStream, 0, false, &[]))
            .await
            .unwrap();
        let events = read_events(&mut r.client).await;
        assert!(events.iter().all(|e| e["type"] != "detached"), "{events:?}");
        timeout(T, r.events).await.unwrap().unwrap();
        assert!(matches!(
            *states.borrow_and_update(),
            SourceState::Interrupted { .. }
        ));
    }
}
