use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use dstreamy_core::{Codec, Config, CropRect, Fps, HealthLevel, Resolution, StreamConfig};
use dstreamy_discord::{
    DiscordClient, Session, SessionEvent, SessionOptions, SessionState, StopReason, StreamOptions,
};
use dstreamy_media::{CaptureConfig, VideoCodec};
#[cfg(unix)]
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::timeout;

use crate::discord::{self, Persistence};
use crate::output::Out;
use crate::{CliError, Ctx, capture};

const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Exit status after a second interrupt cut the shutdown short, as for a shell's SIGINT.
const FORCED_EXIT: i32 = 130;

#[derive(Debug, Args)]
pub struct StreamArgs {
    /// Guild (server) id, see `dstreamy guilds`
    #[arg(long)]
    pub guild: String,
    /// Voice channel id, see `dstreamy channels <GUILD>`
    #[arg(long)]
    pub channel: String,
    /// Capture target id from `dstreamy targets`, or `test-pattern`
    #[arg(long)]
    pub target: String,
    /// Crop rectangle in source pixels
    #[arg(long, value_name = "X,Y,W,H", value_parser = parse_crop)]
    pub crop: Option<CropRect>,
    /// Video codec: h264 or h265
    #[arg(long, value_parser = parse_codec)]
    pub codec: Option<Codec>,
    /// Frames per second: 15, 30 or 60
    #[arg(long, value_parser = parse_fps)]
    pub fps: Option<Fps>,
    /// Maximum output size: WIDTHxHEIGHT, 720p, 900p or 1080p
    #[arg(long, value_name = "RES")]
    pub res: Option<Resolution>,
    /// Video bitrate in kbps (1000-15000)
    #[arg(long, value_name = "KBPS")]
    pub bitrate: Option<u32>,
    /// Audio gain (0.0-3.0)
    #[arg(long)]
    pub gain: Option<f32>,
}

fn parse_crop(s: &str) -> Result<CropRect, String> {
    let parts: Vec<u32> = s
        .split(',')
        .map(|p| p.trim().parse::<u32>())
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{e}"))?;
    match parts[..] {
        [x, y, w, h] if w > 0 && h > 0 => Ok(CropRect::new(x, y, w, h)),
        [_, _, _, _] => Err("width and height must be positive".to_owned()),
        _ => Err("expected four numbers X,Y,W,H".to_owned()),
    }
}

fn parse_codec(s: &str) -> Result<Codec, String> {
    s.to_ascii_uppercase()
        .parse()
        .map_err(|_| "expected h264 or h265".to_owned())
}

fn parse_fps(s: &str) -> Result<Fps, String> {
    let n: u32 = s.parse().map_err(|e| format!("{e}"))?;
    Fps::try_from(n).map_err(|e| e.to_string())
}

impl StreamArgs {
    fn stream_config(&self, base: StreamConfig) -> Result<StreamConfig, CliError> {
        let config = StreamConfig {
            resolution: self.res.unwrap_or(base.resolution),
            fps: self.fps.unwrap_or(base.fps),
            bitrate_kbps: self.bitrate.unwrap_or(base.bitrate_kbps),
            codec: self.codec.unwrap_or(base.codec),
            audio_gain: self.gain.unwrap_or(base.audio_gain),
        };
        config.validate()?;
        Ok(config)
    }

    fn capture_config(&self, stream: &StreamConfig, crop: Option<CropRect>) -> CaptureConfig {
        CaptureConfig {
            target_id: Some(self.target.clone()),
            crop: crop.map(|c| dstreamy_media::CropRect {
                x: c.x,
                y: c.y,
                width: c.width,
                height: c.height,
            }),
            width: stream.resolution.width(),
            height: stream.resolution.height(),
            fps: stream.fps.get(),
            bitrate_bps: stream.bitrate_kbps * 1000,
            codec: match stream.codec {
                Codec::H264 => VideoCodec::H264,
                Codec::H265 => VideoCodec::H265,
            },
            audio: true,
            audio_gain: stream.audio_gain,
        }
    }
}

enum Outcome {
    Interrupted,
    Stopped(StopReason),
}

pub async fn run(ctx: &Ctx, out: &Out, args: StreamArgs) -> Result<ExitCode, CliError> {
    let config = Config::load(&ctx.paths.config_file)?;
    let stream = args.stream_config(config.stream)?;
    let capture = args.capture_config(&stream, args.crop.or(config.crop));
    let source = capture::source_for(&args.target)?;

    let client = Arc::new(discord::connect(ctx, Persistence::Long).await?);
    let channels = match discord::voice_channels(&client, &args.guild).await {
        Ok(channels) => channels,
        Err(e) => {
            close(client).await;
            return Err(e);
        }
    };
    if !channels.iter().any(|c| c.id == args.channel) {
        close(client).await;
        return Err(CliError::UnknownChannel {
            guild: args.guild,
            channel: args.channel,
        });
    }

    let session = Session::new(client.clone());
    let mut events = session.subscribe();
    let options = SessionOptions::new(StreamOptions::new(capture));
    if let Err(e) = session
        .start(Some(args.guild), args.channel, source, options)
        .await
    {
        session.shutdown().await;
        close(client).await;
        return Err(CliError::Session(e.error.to_string()));
    }

    let mut signals = Signals::new();
    let outcome = loop {
        tokio::select! {
            () = signals.recv() => break Outcome::Interrupted,
            event = events.recv() => match event {
                Ok(event) => {
                    print_event(out, &event);
                    if let SessionEvent::State { state: SessionState::Stopped { reason } } = event {
                        break Outcome::Stopped(reason);
                    }
                    if out.stdout_broken() {
                        break Outcome::Interrupted;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "session events lagged");
                    if let SessionState::Stopped { reason } = session.state().borrow().clone() {
                        break Outcome::Stopped(reason);
                    }
                }
                Err(RecvError::Closed) => unreachable!("the session holds the event sender"),
            },
        }
    };

    tokio::spawn(async move {
        signals.recv().await;
        std::process::exit(FORCED_EXIT);
    });
    if !out.json {
        out.note("stopping…");
    }
    session.shutdown().await;
    while let Ok(event) = events.try_recv() {
        print_event(out, &event);
    }
    close(client).await;

    match outcome {
        Outcome::Interrupted
        | Outcome::Stopped(StopReason::Requested | StopReason::CaptureEnded) => {
            Ok(ExitCode::SUCCESS)
        }
        Outcome::Stopped(reason) => Err(CliError::Stopped(reason.to_string())),
    }
}

async fn close(client: Arc<DiscordClient>) {
    match Arc::try_unwrap(client) {
        Ok(client) => {
            let _ = timeout(CLOSE_TIMEOUT, client.close()).await;
        }
        Err(_) => tracing::debug!("client still shared; dropping it without a close frame"),
    }
}

/// SIGINT and SIGTERM, registered once so that a press during shutdown is never lost.
struct Signals {
    #[cfg(unix)]
    streams: Option<(Signal, Signal)>,
}

impl Signals {
    fn new() -> Self {
        #[cfg(unix)]
        {
            let streams = signal(SignalKind::interrupt())
                .and_then(|int| Ok((int, signal(SignalKind::terminate())?)))
                .ok();
            Self { streams }
        }
        #[cfg(not(unix))]
        Self {}
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        if let Some((int, term)) = &mut self.streams {
            tokio::select! {
                _ = int.recv() => {}
                _ = term.recv() => {}
            }
            return;
        }
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn level(level: HealthLevel) -> &'static str {
    match level {
        HealthLevel::Good => "good",
        HealthLevel::Warn => "warn",
        HealthLevel::Bad => "bad",
    }
}

fn print_event(out: &Out, event: &SessionEvent) {
    if out.json {
        out.json_line(event);
        return;
    }
    match event {
        SessionEvent::State { state } => out.line(format_args!("state: {state}")),
        SessionEvent::Health(h) => out.line(format_args!(
            "{} fps  {} kbps  video {} ({})  audio {} ({})  dropped {}  viewer keyframe requests {}",
            h.metrics.fps,
            h.metrics.bitrate_kbps,
            level(h.health.video_level),
            h.health.video_score,
            level(h.health.audio_level),
            h.health.audio_score,
            h.stats.video_frames_dropped,
            h.stats.viewer_keyframe_requests,
        )),
        SessionEvent::ChannelMoved { channel_id, .. } => {
            out.line(format_args!("moved to channel {channel_id}"));
        }
        SessionEvent::Resumed => out.line("connection resumed"),
        SessionEvent::Error {
            message,
            fatal: true,
        } => out.error(message),
        SessionEvent::Error { message, .. } => out.warn(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_parses_four_numbers() {
        assert_eq!(
            parse_crop("10, 20,800,600").unwrap(),
            CropRect::new(10, 20, 800, 600)
        );
        for bad in ["1,2,3", "1,2,3,4,5", "a,b,c,d", "0,0,0,10", "-1,0,10,10"] {
            assert!(parse_crop(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn codec_and_fps_parse_leniently_but_validate() {
        assert_eq!(parse_codec("h265").unwrap(), Codec::H265);
        assert_eq!(parse_codec("H264").unwrap(), Codec::H264);
        assert!(parse_codec("vp8").is_err());
        assert_eq!(parse_fps("60").unwrap(), Fps::Fps60);
        assert!(parse_fps("24").is_err());
    }

    fn args() -> StreamArgs {
        StreamArgs {
            guild: "g".into(),
            channel: "c".into(),
            target: "test-pattern".into(),
            crop: None,
            codec: None,
            fps: None,
            res: None,
            bitrate: None,
            gain: None,
        }
    }

    #[test]
    fn flags_override_the_config_file_and_are_validated() {
        let base = StreamConfig {
            bitrate_kbps: 4_000,
            audio_gain: 0.5,
            ..StreamConfig::default()
        };
        let merged = args().stream_config(base).unwrap();
        assert_eq!(merged, base);

        let a = StreamArgs {
            res: Some(Resolution::P720),
            bitrate: Some(9_000),
            gain: Some(2.0),
            ..args()
        };
        let merged = a.stream_config(base).unwrap();
        assert_eq!(merged.resolution, Resolution::P720);
        assert_eq!(merged.bitrate_kbps, 9_000);
        assert_eq!(merged.audio_gain, 2.0);
        assert_eq!(merged.fps, base.fps);

        for bad in [
            StreamArgs {
                bitrate: Some(50_000),
                ..args()
            },
            StreamArgs {
                gain: Some(4.0),
                ..args()
            },
        ] {
            assert!(bad.stream_config(base).is_err());
        }
    }

    #[test]
    fn capture_config_carries_every_setting() {
        let stream = StreamConfig {
            resolution: Resolution::P900,
            fps: Fps::Fps60,
            bitrate_kbps: 8_000,
            codec: Codec::H265,
            audio_gain: 1.5,
        };
        let c = args().capture_config(&stream, Some(CropRect::new(1, 2, 30, 40)));
        assert_eq!(c.target_id.as_deref(), Some("test-pattern"));
        assert_eq!((c.width, c.height, c.fps), (1600, 900, 60));
        assert_eq!(c.bitrate_bps, 8_000_000);
        assert_eq!(c.codec, VideoCodec::H265);
        assert_eq!(c.audio_gain, 1.5);
        let crop = c.crop.unwrap();
        assert_eq!((crop.x, crop.y, crop.width, crop.height), (1, 2, 30, 40));
    }
}
