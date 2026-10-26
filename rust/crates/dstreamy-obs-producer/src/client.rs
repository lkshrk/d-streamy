//! The producer side of the `dstreamy serve` IPC: `hello`, then frames out and events in.

use std::borrow::Cow;
use std::fmt;
use std::future::Future;
use std::time::{Duration, Instant};

use dstreamy_ipc::producer::{HEADER_LEN, MAX_FRAME_LEN};
use dstreamy_ipc::{ControlCodec, Endpoint, Frame, IpcError, Kind, ProducerCodec};
use dstreamy_obs_packet::{self as packet, Converter, PacketError};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::io::{ReadHalf, WriteHalf};
use tokio_util::bytes::Bytes;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::ring::{Packet, Ring, Track};
use crate::types::{End, Hello, Level, Log};

/// Above serve's 5 s wait for the session to announce the producer's video.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to read pending events after a failed write, so a `detached` already sent is not missed.
pub const PENDING_EVENTS: Duration = Duration::from_secs(1);
/// Service text shown in OBS is cut to this many characters.
pub const MAX_SERVICE_TEXT: usize = 300;
/// After a stop, the queued packets and end of stream get this long before the connection is dropped.
pub const STOP_DEADLINE: Duration = Duration::from_millis(1500);

type Lines = FramedRead<ReadHalf<dstreamy_ipc::Stream>, ControlCodec>;
type Sink = FramedWrite<WriteHalf<dstreamy_ipc::Stream>, ProducerCodec>;

#[derive(Debug)]
pub enum StartError {
    Connect(IpcError),
    Refused { code: String, message: String },
    Protocol(String),
}

impl StartError {
    pub fn is_not_running(&self) -> bool {
        matches!(self, Self::Connect(IpcError::NotRunning(_)))
    }
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(e @ IpcError::NotRunning(_)) => {
                write!(f, "The D-Streamy service is not running ({e})")
            }
            Self::Connect(e) => write!(f, "Cannot connect to the D-Streamy service: {e}"),
            Self::Refused { code, message } => {
                write!(
                    f,
                    "The D-Streamy service refused the stream ({code}): {message}"
                )
            }
            Self::Protocol(m) => write!(f, "The D-Streamy service answered unexpectedly: {m}"),
        }
    }
}

impl std::error::Error for StartError {}

/// An attachment the service accepted.
pub struct Connection {
    lines: Lines,
    sink: Sink,
    attachment: u64,
}

impl Connection {
    pub fn attachment(&self) -> u64 {
        self.attachment
    }
}

pub async fn attach(endpoint: &Endpoint, hello: &Hello) -> Result<Connection, StartError> {
    let stream = dstreamy_ipc::connect(endpoint)
        .await
        .map_err(StartError::Connect)?;
    let (read, write) = tokio::io::split(stream);
    let mut lines = FramedRead::new(read, ControlCodec::new());
    let mut hello_writer = FramedWrite::new(write, ControlCodec::new());
    hello_writer
        .send(hello.to_json())
        .await
        .map_err(|e| StartError::Protocol(format!("sending hello: {e}")))?;
    let reply = match tokio::time::timeout(REPLY_TIMEOUT, lines.next()).await {
        Err(_) => {
            return Err(StartError::Protocol(format!(
                "no reply to hello within {} s",
                REPLY_TIMEOUT.as_secs()
            )));
        }
        Ok(Some(Ok(Ok(reply)))) => reply,
        Ok(Some(Ok(Err(e)))) => return Err(StartError::Protocol(format!("hello reply: {e}"))),
        Ok(Some(Err(e))) => return Err(StartError::Protocol(format!("reading hello reply: {e}"))),
        Ok(None) => {
            return Err(StartError::Protocol(
                "connection closed before the hello reply".into(),
            ));
        }
    };
    if let Some(attachment) = reply.get("attached").and_then(Value::as_u64) {
        let sink = FramedWrite::new(hello_writer.into_inner(), ProducerCodec::new());
        return Ok(Connection {
            lines,
            sink,
            attachment,
        });
    }
    match (reply["type"].as_str(), reply["code"].as_str()) {
        (Some("error"), Some(code)) => Err(StartError::Refused {
            code: service_text(code),
            message: service_text(reply["message"].as_str().unwrap_or_default()),
        }),
        _ => Err(StartError::Protocol(service_text(&format!(
            "hello reply {reply}"
        )))),
    }
}

#[derive(Debug)]
enum WriterEnd<T> {
    Stopped(T),
    Failed(String),
    Refused(String),
}

/// Sends the ring's packets until `stop` fires or the service ends the attachment.
pub async fn run<T>(
    connection: Connection,
    ring: &Ring,
    video_extra_data: impl FnMut() -> Option<Vec<u8>>,
    stop: impl Future<Output = T>,
    log: &dyn Log,
) -> End<T> {
    let Connection { lines, sink, .. } = connection;
    let reader = read_events(lines, log);
    let writer = write_packets(sink, ring, video_extra_data, stop, log);
    tokio::pin!(reader, writer);
    tokio::select! {
        biased;
        written = &mut writer => match written {
            WriterEnd::Stopped(t) => End::Stopped(t),
            WriterEnd::Refused(e) => End::Refused(e),
            WriterEnd::Failed(e) => {
                log.log(Level::Debug, &format!("{e}; reading pending events"));
                tokio::time::timeout(PENDING_EVENTS, reader)
                    .await
                    .unwrap_or(End::Closed(e))
            }
        },
        ended = &mut reader => ended,
    }
}

async fn read_events<T>(mut lines: Lines, log: &dyn Log) -> End<T> {
    let mut malformed = 0u64;
    let end = loop {
        let event = match lines.next().await {
            Some(Ok(Ok(event))) => event,
            Some(Ok(Err(e))) => {
                malformed += 1;
                if malformed == 1 {
                    log.log(
                        Level::Warn,
                        &format!("ignoring a malformed event line: {e}"),
                    );
                }
                continue;
            }
            Some(Err(e)) => break End::Closed(format!("reading from the service: {e}")),
            None => break End::Closed("the service closed the connection".into()),
        };
        match event["type"].as_str() {
            Some("keyframe_request") => log.log(
                Level::Debug,
                "keyframe requested; the encoder's keyframe interval answers it",
            ),
            Some("detached") => {
                break End::Detached {
                    reason: service_text(event["reason"].as_str().unwrap_or("unknown")),
                    message: service_text(event["message"].as_str().unwrap_or_default()),
                };
            }
            _ => log.log(
                Level::Debug,
                &service_text(&format!("service event {event}")),
            ),
        }
    };
    if malformed > 1 {
        log.log(
            Level::Warn,
            &format!("ignored {malformed} malformed event lines"),
        );
    }
    end
}

/// Service text as OBS may show it: no control or bidi-override characters, at most `MAX_SERVICE_TEXT` characters.
fn service_text(text: &str) -> String {
    let bidi = |c: char| matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}');
    let mut out: String = text
        .chars()
        .take(MAX_SERVICE_TEXT)
        .map(|c| if c.is_control() || bidi(c) { ' ' } else { c })
        .collect();
    if text.chars().nth(MAX_SERVICE_TEXT).is_some() {
        out.push('…');
    }
    out
}

async fn write_packets<T>(
    mut sink: Sink,
    ring: &Ring,
    mut video_extra_data: impl FnMut() -> Option<Vec<u8>>,
    stop: impl Future<Output = T>,
    log: &dyn Log,
) -> WriterEnd<T> {
    let mut converter = Converter::new();
    let mut last_pts_us = 0;
    tokio::pin!(stop);
    let stopped = loop {
        let packet = tokio::select! {
            biased;
            t = &mut stop => break t,
            packet = ring.next() => packet,
        };
        let Some(packet) = packet else {
            return WriterEnd::Failed("the packet ring closed".into());
        };
        let arrived = packet.arrived;
        let frame = match frame_for(packet, &mut converter, &mut video_extra_data) {
            Ok(frame) => frame,
            Err(e) => return WriterEnd::Refused(e),
        };
        if frame.kind == Kind::Video {
            last_pts_us = frame.pts_us;
        }
        let keyframe_pts_us = frame.keyframe.then_some(frame.pts_us);
        let sent = tokio::select! {
            biased;
            t = &mut stop => break t,
            sent = sink.send(frame) => sent,
        };
        if let Err(e) = sent {
            return WriterEnd::Failed(format!("writing to the service: {e}"));
        }
        if let Some(pts_us) = keyframe_pts_us {
            log.log(
                Level::Debug,
                &format!(
                    "sent keyframe pts_us={pts_us} arrived_ns={}",
                    monotonic_ns(arrived)
                ),
            );
        }
    };
    ring.close();
    let finish = async {
        while let Some(packet) = ring.pop() {
            let frame = match frame_for(packet, &mut converter, &mut video_extra_data) {
                Ok(frame) => frame,
                Err(e) => {
                    log.log(Level::Warn, &format!("not sending a queued packet: {e}"));
                    break;
                }
            };
            if frame.kind == Kind::Video {
                last_pts_us = frame.pts_us;
            }
            sink.feed(frame).await?;
        }
        sink.send(Frame::end_of_stream(last_pts_us)).await
    };
    match tokio::time::timeout(STOP_DEADLINE, finish).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log.log(Level::Warn, &format!("finishing the stream: {e}")),
        Err(_) => log.log(
            Level::Warn,
            &format!(
                "the service read nothing for {} ms after the stop; dropping the connection",
                STOP_DEADLINE.as_millis()
            ),
        ),
    }
    WriterEnd::Stopped(stopped)
}

/// Blocks SIGPIPE for this thread; only Linux raises it on the writing thread (macOS signals the process).
#[cfg(target_os = "linux")]
pub fn block_sigpipe_on_this_thread() -> std::io::Result<()> {
    // SAFETY: sigemptyset/sigaddset initialise a local set; pthread_sigmask reads it and changes only this thread.
    let rc = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGPIPE);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut())
    };
    match rc {
        0 => Ok(()),
        e => Err(std::io::Error::from_raw_os_error(e)),
    }
}

/// CLOCK_MONOTONIC in ns at `at`, comparable across processes (std `Instant` uses it on Linux).
fn monotonic_ns(at: Instant) -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let now = Instant::now();
    // SAFETY: `ts` is a valid out pointer.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return 0;
    }
    let now_ns = ts.tv_sec * 1_000_000_000 + ts.tv_nsec;
    now_ns - i64::try_from(now.saturating_duration_since(at).as_nanos()).unwrap_or(0)
}

/// A packet as a frame the protocol can carry; conversion and size refusals become the stop message.
fn frame_for(
    packet: Packet,
    converter: &mut Converter,
    video_extra_data: &mut impl FnMut() -> Option<Vec<u8>>,
) -> Result<Frame, String> {
    let frame = convert(packet, converter, video_extra_data).map_err(|e| e.to_string())?;
    let len = u64::from(HEADER_LEN) + frame.payload.len() as u64;
    if len > u64::from(MAX_FRAME_LEN) {
        return Err(format!(
            "a {} packet of {} bytes is over the {MAX_FRAME_LEN}-byte frame limit",
            if frame.kind == Kind::Video {
                "video"
            } else {
                "audio"
            },
            frame.payload.len()
        ));
    }
    Ok(frame)
}

/// One OBS packet to a producer frame; the video extra data is read once the first video packet exists.
fn convert(
    packet: Packet,
    converter: &mut Converter,
    video_extra_data: &mut impl FnMut() -> Option<Vec<u8>>,
) -> Result<Frame, PacketError> {
    let Packet {
        track,
        keyframe,
        pts,
        dts,
        timebase,
        data,
        arrived: _,
    } = packet;
    match track {
        Track::Video => {
            if !converter.has_video_extra_data()
                && let Some(extra) = video_extra_data()
            {
                converter.set_video_extra_data(&extra)?;
            }
            let video = converter.convert_video(&data, keyframe, pts, dts, timebase)?;
            let (pts_us, keyframe) = (video.pts_us, video.keyframe);
            let payload = match video.data {
                Cow::Owned(with_parameter_sets) => with_parameter_sets,
                Cow::Borrowed(_) => data,
            };
            Ok(Frame {
                kind: Kind::Video,
                pts_us,
                keyframe,
                payload: Bytes::from(payload),
            })
        }
        Track::Audio => Ok(Frame {
            kind: Kind::Opus,
            pts_us: packet::convert_audio(pts, dts, timebase)?,
            keyframe: false,
            payload: Bytes::from(data),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_without_parameter_sets_takes_the_extra_data_and_moves_the_payload() {
        let sps_pps = [0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce];
        let idr = vec![0, 0, 0, 1, 0x65, 0x88];
        let mut converter = Converter::new();
        let mut asked = 0;
        let mut extra = || {
            asked += 1;
            Some(sps_pps.to_vec())
        };
        let packet = |data: Vec<u8>, keyframe| Packet {
            track: Track::Video,
            keyframe,
            pts: 3,
            dts: 3,
            timebase: (1, 30),
            data,
            arrived: Instant::now(),
        };
        let frame = convert(packet(idr.clone(), true), &mut converter, &mut extra).unwrap();
        assert_eq!(&frame.payload[..sps_pps.len()], &sps_pps);
        assert_eq!(&frame.payload[sps_pps.len()..], &idr[..]);
        assert_eq!(frame.pts_us, 100_000);
        assert!(frame.keyframe);
        let p = vec![0, 0, 0, 1, 0x41, 0x9a];
        let frame = convert(packet(p.clone(), false), &mut converter, &mut extra).unwrap();
        assert_eq!(&frame.payload[..], &p[..]);
        assert_eq!(asked, 1, "extra data is read once");
    }

    #[test]
    fn a_packet_over_the_frame_limit_is_refused_before_writing() {
        let mut converter = Converter::new();
        let packet = Packet {
            track: Track::Audio,
            keyframe: true,
            pts: 0,
            dts: 0,
            timebase: (1, 48_000),
            data: vec![0; (MAX_FRAME_LEN - HEADER_LEN) as usize + 1],
            arrived: Instant::now(),
        };
        let refused = frame_for(packet, &mut converter, &mut || None).unwrap_err();
        assert!(
            refused.contains("over the 8388608-byte frame limit"),
            "{refused}"
        );
        let fits = Packet {
            track: Track::Audio,
            keyframe: true,
            pts: 0,
            dts: 0,
            timebase: (1, 48_000),
            data: vec![0; (MAX_FRAME_LEN - HEADER_LEN) as usize],
            arrived: Instant::now(),
        };
        assert!(frame_for(fits, &mut converter, &mut || None).is_ok());
    }

    #[test]
    fn service_text_drops_controls_and_bidi_overrides_and_is_capped() {
        assert_eq!(service_text("a\nb\u{1b}[31mc\u{202E}d"), "a b [31mc d");
        let long = "x".repeat(MAX_SERVICE_TEXT + 5);
        let cut = service_text(&long);
        assert_eq!(cut.chars().count(), MAX_SERVICE_TEXT + 1);
        assert!(cut.ends_with('…'));
        assert_eq!(service_text("ok"), "ok");
    }

    #[test]
    fn b_frames_are_refused() {
        let mut converter = Converter::new();
        let packet = Packet {
            track: Track::Video,
            keyframe: false,
            pts: 4,
            dts: 3,
            timebase: (1, 30),
            data: vec![0, 0, 0, 1, 0x41],
            arrived: Instant::now(),
        };
        assert_eq!(
            convert(packet, &mut converter, &mut || None).unwrap_err(),
            PacketError::BFrames
        );
    }
}
