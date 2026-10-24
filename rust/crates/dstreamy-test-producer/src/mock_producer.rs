//! `mock-producer`: a test producer for `dstreamy serve` (ADR 0004 producer role). Not shipped.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{ArgGroup, Parser, ValueEnum};
use dstreamy_ipc::{ControlCodec, Endpoint, Frame, Kind, ProducerCodec};
use dstreamy_media::{
    CaptureConfig, CaptureSource, FrameReceiver, MediaFrame, TestPatternSource, VideoCodec, annexb,
};
use dstreamy_test_producer::trace::{Converted, Replay, Trace};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::time::{Instant, sleep, sleep_until};
use tokio_util::bytes::{BufMut, Bytes, BytesMut};
use tokio_util::codec::{FramedRead, FramedWrite};

const RECONNECT_DELAY: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Corrupt {
    NoStartCode,
    IdrWithoutFlag,
    KeyframeWithoutSps,
    PtsNeDts,
    #[value(name = "opus-40ms")]
    Opus40ms,
    Oversized,
}

#[derive(Debug, Parser)]
#[command(
    name = "mock-producer",
    about = "Feeds `dstreamy serve` a test pattern or a recorded OBS trace as a producer",
    group(ArgGroup::new("media").required(true).args(["pattern", "replay"]))
)]
struct Args {
    #[arg(long)]
    endpoint: Option<PathBuf>,
    /// Encodes `TestPatternSource` live.
    #[arg(long)]
    pattern: bool,
    /// Replays an `obs-harness` trace at real-time pace through `obs/src/packet.c`; its size and codecs replace the flags.
    #[arg(long, value_name = "TRACE_DIR")]
    replay: Option<PathBuf>,
    #[arg(long, default_value_t = 320)]
    width: u32,
    #[arg(long, default_value_t = 180)]
    height: u32,
    #[arg(long, default_value_t = 30)]
    fps: u32,
    #[arg(long, default_value_t = 400)]
    bitrate_kbps: u32,
    /// Sends `factor` times faster than real time.
    #[arg(long, value_name = "FACTOR")]
    burst: Option<u32>,
    /// Pads every keyframe with filler data to this many bytes.
    #[arg(long, value_name = "BYTES")]
    pad_keyframes: Option<usize>,
    /// Stops after this long; runs until killed otherwise.
    #[arg(long, value_name = "MS")]
    duration: Option<u64>,
    /// Stops once this many frame payload bytes, video and audio, were sent.
    #[arg(long, value_name = "BYTES")]
    until_bytes: Option<u64>,
    #[arg(long, value_name = "MS")]
    disconnect_after: Option<u64>,
    /// Stops sending, but stays connected, for `stall_for` ms.
    #[arg(long, value_name = "MS")]
    stall_after: Option<u64>,
    #[arg(long, value_name = "MS", default_value_t = 3_000)]
    stall_for: u64,
    #[arg(long, value_enum)]
    corrupt: Option<Corrupt>,
    /// Sends `--corrupt` this long after attaching.
    #[arg(long, value_name = "MS", default_value_t = 500)]
    corrupt_after: u64,
    /// Connects again after each disconnect, `--reconnect` times at most.
    #[arg(long, value_name = "N", default_value_t = 0)]
    reconnect: u32,
}

type Lines = FramedRead<ReadHalf<dstreamy_ipc::Stream>, ControlCodec>;
type Sink = FramedWrite<WriteHalf<dstreamy_ipc::Stream>, ProducerCodec>;

#[derive(Debug)]
enum Outcome {
    Finished,
    Disconnected,
    Detached(Value),
}

fn hello(args: &Args, trace: Option<&Trace>) -> Result<Value, String> {
    let (video, audio) = match trace {
        Some(t) => (
            json!({"codec": "h264", "width": t.width()?, "height": t.height()?, "fps": t.fps()?}),
            json!({
                "codec": t.audio_codec(), "sample_rate": t.sample_rate()?, "channels": 2,
                "frame_ms": t.audio_frame_size()? * 1000 / t.sample_rate()?.max(1),
            }),
        ),
        None => (
            json!({"codec": "h264", "width": args.width, "height": args.height, "fps": args.fps}),
            json!({"codec": "opus", "sample_rate": 48000, "channels": 2, "frame_ms": 20}),
        ),
    };
    let obs_version = trace.and_then(|t| t.meta["libobs"].as_str().map(str::to_owned));
    Ok(json!({
        "id": "1", "cmd": "hello", "role": "producer", "protocol": 2,
        "video": video,
        "audio": audio,
        "client": {"name": "mock-producer", "version": env!("CARGO_PKG_VERSION"), "obs_version": obs_version},
    }))
}

/// Prints a line that tests read from stdout.
fn report(value: Value) {
    println!("{value}");
}

fn frame(kind: Kind, pts_us: i64, keyframe: bool, payload: Vec<u8>) -> Frame {
    Frame {
        kind,
        pts_us,
        keyframe,
        payload: Bytes::from(payload),
    }
}

fn media(frame: MediaFrame) -> Frame {
    let (kind, pts, keyframe, data) = match frame {
        MediaFrame::Video(v) => (Kind::Video, v.pts_us, v.keyframe, v.data),
        MediaFrame::Audio(a) => (Kind::Opus, a.pts_us, false, a.data),
    };
    self::frame(kind, i64::try_from(pts).unwrap_or(i64::MAX), keyframe, data)
}

/// Where frames come from: the live pattern, an OBS trace, or for `--burst` one encoded loop replayed with rising pts.
enum Media {
    Live(TestPatternSource, FrameReceiver),
    Replay {
        replay: Replay,
        origin: Instant,
        first_dts_us: Option<i64>,
        pending: Option<(Instant, Frame)>,
    },
    Burst {
        clip: Vec<Frame>,
        span_us: i64,
        next: usize,
        lap: i64,
        factor: u32,
        origin: Instant,
    },
}

/// One keyframe interval of pattern media, encoded once; the encoder is too slow to burst live.
async fn clip(config: CaptureConfig) -> Result<(Vec<Frame>, i64), String> {
    let span_us = i64::from(config.keyframe_interval_frames()) * 1_000_000 / i64::from(config.fps);
    let mut pattern = TestPatternSource::unpaced();
    let mut frames = pattern.start(config).await.map_err(|e| e.to_string())?;
    let mut clip = Vec::new();
    while let Some(frame) = frames.recv().await {
        let frame = media(frame);
        if frame.pts_us >= span_us {
            break;
        }
        clip.push(frame);
    }
    let _ = pattern.stop().await;
    Ok((clip, span_us))
}

impl Media {
    async fn new(config: CaptureConfig, burst: Option<u32>) -> Result<Self, String> {
        match burst {
            Some(factor) => {
                let (clip, span_us) = clip(config).await?;
                Ok(Self::Burst {
                    clip,
                    span_us,
                    next: 0,
                    lap: 0,
                    factor: factor.max(1),
                    origin: Instant::now(),
                })
            }
            None => {
                let mut pattern = TestPatternSource::new();
                let frames = pattern.start(config).await.map_err(|e| e.to_string())?;
                Ok(Self::Live(pattern, frames))
            }
        }
    }

    /// `None` when the media ends; an error when `packet.c` refuses a trace packet.
    async fn next(&mut self) -> Option<Result<Frame, String>> {
        match self {
            Self::Live(_, frames) => frames.recv().await.map(|f| Ok(media(f))),
            Self::Replay {
                replay,
                origin,
                first_dts_us,
                pending,
            } => {
                // `select!` may drop this future while it sleeps, so the converted packet waits in `pending`.
                if pending.is_none() {
                    let replayed = match replay.next()? {
                        Ok(r) => r,
                        Err(e) => return Some(Err(e.to_string())),
                    };
                    let first = *first_dts_us.get_or_insert(replayed.dts_usec);
                    let due = *origin
                        + Duration::from_micros(
                            u64::try_from(replayed.dts_usec - first).unwrap_or(0),
                        );
                    let frame = match replayed.frame {
                        Converted::Video {
                            data,
                            pts_us,
                            keyframe,
                        } => frame(Kind::Video, pts_us, keyframe, data),
                        Converted::Audio { data, pts_us } => frame(Kind::Opus, pts_us, false, data),
                    };
                    *pending = Some((due, frame));
                }
                let due = pending.as_ref().map(|(due, _)| *due)?;
                sleep_until(due).await;
                pending.take().map(|(_, frame)| Ok(frame))
            }
            Self::Burst {
                clip,
                span_us,
                next,
                lap,
                factor,
                origin,
            } => {
                let mut frame = clip.get(*next)?.clone();
                frame.pts_us += *lap * *span_us;
                let due = u64::try_from(frame.pts_us).unwrap_or(0) / u64::from(*factor);
                sleep_until(*origin + Duration::from_micros(due)).await;
                *next += 1;
                if *next == clip.len() {
                    *next = 0;
                    *lap += 1;
                }
                Some(Ok(frame))
            }
        }
    }

    fn request_keyframe(&self) {
        if let Self::Live(pattern, _) = self {
            pattern.request_keyframe();
        }
    }

    /// Skips what piled up during a stall and restarts at a keyframe.
    fn resume(&mut self) {
        match self {
            Self::Live(pattern, frames) => {
                while frames.try_recv().is_ok() {}
                pattern.request_keyframe();
            }
            Self::Replay { .. } | Self::Burst { .. } => {}
        }
    }

    async fn stop(self) {
        if let Self::Live(mut pattern, _) = self {
            let _ = pattern.stop().await;
        }
    }
}

/// Appends a filler data NAL unit (H.264 Table 7-1, type 12) so the frame is `size` bytes.
fn padded(mut frame: Frame, size: usize) -> Frame {
    let mut data = frame.payload.to_vec();
    let fill = size.saturating_sub(data.len() + annexb::START_CODE.len() + 2);
    if fill > 0 {
        data.extend_from_slice(&annexb::START_CODE);
        data.push(12);
        data.resize(data.len() + fill, 0xFF);
        data.push(0x80);
    }
    frame.payload = Bytes::from(data);
    frame
}

/// A frame the service must refuse for `kind`, built from the last frames sent.
fn corrupted(kind: Corrupt, last_video_pts: i64, keyframe: &[u8]) -> Result<BytesMut, String> {
    let mut buf = BytesMut::new();
    let nalus: Vec<&[u8]> = annexb::nal_units(keyframe).collect();
    let mut without_sps = Vec::new();
    for n in nalus
        .iter()
        .filter(|n| annexb::h264_nal_type(n) != Some(annexb::H264_NAL_SPS))
    {
        annexb::append_nalu(&mut without_sps, n);
    }
    let encoded = |f: Frame, buf: &mut BytesMut| {
        tokio_util::codec::Encoder::encode(&mut ProducerCodec, f, buf).map_err(|e| e.to_string())
    };
    let next = last_video_pts + 1;
    match kind {
        Corrupt::NoStartCode => {
            let stripped = keyframe
                .strip_prefix(&annexb::START_CODE)
                .ok_or("keyframe lacks a start code")?;
            encoded(frame(Kind::Video, next, true, stripped.to_vec()), &mut buf)?;
        }
        Corrupt::IdrWithoutFlag => {
            encoded(frame(Kind::Video, next, false, keyframe.to_vec()), &mut buf)?
        }
        Corrupt::KeyframeWithoutSps => {
            encoded(frame(Kind::Video, next, true, without_sps), &mut buf)?
        }
        Corrupt::PtsNeDts => encoded(
            frame(Kind::Video, last_video_pts - 1, true, keyframe.to_vec()),
            &mut buf,
        )?,
        // RFC 6716 §3.1: config 31 (CELT FB 20 ms) with code 1, two frames.
        Corrupt::Opus40ms => encoded(frame(Kind::Opus, 0, false, vec![0xF9, 0, 0]), &mut buf)?,
        Corrupt::Oversized => {
            buf.put_u32(dstreamy_ipc::producer::MAX_FRAME_LEN + 1);
            buf.put_u8(Kind::Video as u8);
        }
    }
    Ok(buf)
}

async fn connect(endpoint: &Endpoint, hello: &Value) -> Result<Option<(Lines, Sink)>, String> {
    let stream = dstreamy_ipc::connect(endpoint)
        .await
        .map_err(|e| format!("connecting to {endpoint}: {e}"))?;
    let (read, write) = tokio::io::split(stream);
    let mut lines = FramedRead::new(read, ControlCodec::new());
    let mut hello_writer = FramedWrite::new(write, ControlCodec::new());
    hello_writer
        .send(hello.clone())
        .await
        .map_err(|e| format!("sending hello: {e}"))?;
    let reply = match lines.next().await {
        Some(Ok(Ok(reply))) => reply,
        other => return Err(format!("no hello reply: {other:?}")),
    };
    report(json!({"hello": reply}));
    if reply.get("attached").is_none() {
        return Ok(None);
    }
    let write = hello_writer.into_inner();
    Ok(Some((lines, FramedWrite::new(write, ProducerCodec::new()))))
}

/// Sends test-pattern media until the duration, a fault, or the service ends the attachment.
async fn attach(
    args: &Args,
    trace: Option<&Trace>,
    mut lines: Lines,
    mut sink: Sink,
    deadline: Option<Instant>,
) -> Result<Outcome, String> {
    let mut source = match trace {
        Some(t) => Media::Replay {
            replay: t.clone().replay(),
            origin: Instant::now(),
            first_dts_us: None,
            pending: None,
        },
        None => {
            let config = CaptureConfig {
                width: args.width,
                height: args.height,
                fps: args.fps,
                bitrate_bps: args.bitrate_kbps * 1000,
                codec: VideoCodec::H264,
                ..CaptureConfig::default()
            };
            Media::new(config, args.burst).await?
        }
    };
    let started = Instant::now();
    let at = |ms: Option<u64>| ms.map(|ms| started + Duration::from_millis(ms));
    let disconnect_at = at(args.disconnect_after);
    let mut stall_at = at(args.stall_after);
    let mut corrupt_at = args.corrupt.and(at(Some(args.corrupt_after)));
    let mut last_video_pts = 0i64;
    let mut last_keyframe: Option<Vec<u8>> = None;
    let mut sent_bytes = 0u64;
    let outcome = loop {
        let wake = [deadline, disconnect_at, stall_at, corrupt_at]
            .into_iter()
            .flatten()
            .min();
        tokio::select! {
            biased;
            line = lines.next() => match line {
                Some(Ok(Ok(event))) => {
                    report(json!({"event": event}));
                    if event["type"] == "keyframe_request" {
                        source.request_keyframe();
                    }
                    if event["type"] == "detached" {
                        break Outcome::Detached(event);
                    }
                }
                Some(Ok(Err(e))) => return Err(format!("malformed event: {e}")),
                None | Some(Err(_)) => break Outcome::Disconnected,
            },
            () = sleep_until_opt(wake) => {
                let now = Instant::now();
                if deadline.is_some_and(|d| now >= d) {
                    let _ = sink.send(Frame::end_of_stream(last_video_pts)).await;
                    break Outcome::Finished;
                }
                if disconnect_at.is_some_and(|d| now >= d) {
                    report(json!({"fault": "disconnect"}));
                    break Outcome::Disconnected;
                }
                if stall_at.is_some_and(|d| now >= d) {
                    stall_at = None;
                    report(json!({"fault": "stall"}));
                    sleep(Duration::from_millis(args.stall_for)).await;
                    source.resume();
                    report(json!({"fault": "resume"}));
                }
                if corrupt_at.is_some_and(|d| now >= d)
                    && let (Some(kind), Some(keyframe)) = (args.corrupt, last_keyframe.as_deref())
                {
                    corrupt_at = None;
                    let bytes = corrupted(kind, last_video_pts, keyframe)?;
                    report(json!({"fault": "corrupt", "kind": format!("{kind:?}")}));
                    let io = sink.get_mut();
                    if io.write_all(&bytes).await.is_err() || io.flush().await.is_err() {
                        break pending_detached(&mut lines).await;
                    }
                }
            }
            next = source.next() => {
                let frame = match next {
                    Some(Ok(frame)) => frame,
                    Some(Err(e)) => {
                        report(json!({"refused": e}));
                        source.stop().await;
                        return Err(format!("conversion refused a trace packet: {e}"));
                    }
                    None if trace.is_some() => {
                        let _ = sink.send(Frame::end_of_stream(last_video_pts)).await;
                        break Outcome::Finished;
                    }
                    None => return Err("the test pattern ended".into()),
                };
                let frame = match args.pad_keyframes {
                    Some(size) if frame.keyframe => padded(frame, size),
                    _ => frame,
                };
                if frame.kind == Kind::Video {
                    last_video_pts = frame.pts_us;
                    if frame.keyframe {
                        last_keyframe = Some(frame.payload.to_vec());
                    }
                }
                sent_bytes += frame.payload.len() as u64;
                if sink.send(frame).await.is_err() {
                    break pending_detached(&mut lines).await;
                }
                if args.until_bytes.is_some_and(|n| sent_bytes >= n) {
                    let _ = sink.send(Frame::end_of_stream(last_video_pts)).await;
                    break Outcome::Finished;
                }
            }
        }
    };
    report(json!({"sent_bytes": sent_bytes}));
    source.stop().await;
    Ok(outcome)
}

/// After a failed write the service may already have sent `detached` before closing; read it if so.
async fn pending_detached(lines: &mut Lines) -> Outcome {
    let drain = async {
        while let Some(Ok(Ok(event))) = lines.next().await {
            report(json!({"event": event}));
            if event["type"] == "detached" {
                return Outcome::Detached(event);
            }
        }
        Outcome::Disconnected
    };
    tokio::time::timeout(Duration::from_secs(1), drain)
        .await
        .unwrap_or(Outcome::Disconnected)
}

async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(at) => sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn run(args: Args) -> Result<(), String> {
    let endpoint = match &args.endpoint {
        Some(path) => Endpoint::new(path),
        None => Endpoint::default_for_user().map_err(|e| e.to_string())?,
    };
    let deadline = args
        .duration
        .map(|ms| Instant::now() + Duration::from_millis(ms));
    let trace = args.replay.as_deref().map(Trace::load).transpose()?;
    let hello = hello(&args, trace.as_ref())?;
    let mut left = args.reconnect;
    loop {
        let Some((lines, sink)) = connect(&endpoint, &hello).await? else {
            return Err("the service refused the producer".into());
        };
        let outcome = attach(&args, trace.as_ref(), lines, sink, deadline).await?;
        report(json!({"outcome": format!("{outcome:?}")}));
        match outcome {
            Outcome::Disconnected if left > 0 => {
                left -= 1;
                sleep(RECONNECT_DELAY).await;
            }
            Outcome::Finished | Outcome::Disconnected => return Ok(()),
            Outcome::Detached(event) => return Err(format!("detached: {event}")),
        }
    }
}

fn main() -> ExitCode {
    let args = Args::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("mock-producer: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mock-producer: {e}");
            ExitCode::FAILURE
        }
    }
}
