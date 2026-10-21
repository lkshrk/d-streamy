use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use dstreamy_core::{Codec, Config, CropRect, Fps, Resolution, StreamConfig};
use dstreamy_discord::{
    DiscordClient, Session, SessionError, SessionEvent, SessionOptions, SessionState, StopReason,
    StreamOptions,
};
use dstreamy_media::{CaptureConfig, CaptureSource, VideoCodec};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::timeout;

use crate::discord::{self, Persistence};
use crate::output::Out;
use crate::signals::Signals;
use crate::{CliError, Ctx, capture};

pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Exit status after a second interrupt cut the shutdown short, as for a shell's SIGINT.
pub const FORCED_EXIT: u8 = 130;

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
    /// Video codec: h264, or h265 (experimental: most viewers see no video)
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

pub fn parse_crop(s: &str) -> Result<CropRect, String> {
    let parts: Vec<u32> = s
        .split(',')
        .map(|p| p.trim().parse::<u32>())
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{e}"))?;
    match parts[..] {
        [x, y, w, h] => CropRect::new(x, y, w, h)
            .validate()
            .map_err(|e| e.to_string()),
        _ => Err("expected four numbers X,Y,W,H".to_owned()),
    }
}

pub fn parse_codec(s: &str) -> Result<Codec, String> {
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
        capture_config(&self.target, stream, crop)
    }
}

pub fn capture_config(
    target: &str,
    stream: &StreamConfig,
    crop: Option<CropRect>,
) -> CaptureConfig {
    CaptureConfig {
        target_id: Some(target.to_owned()),
        crop,
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

enum Outcome {
    Interrupted,
    Stopped(StopReason),
}

pub async fn run(ctx: &Ctx, out: &Out, args: StreamArgs) -> Result<ExitCode, CliError> {
    run_with(ctx, out, args, Signals::new).await
}

/// `signals` is called once the stream is live, so an earlier Ctrl-C still kills the process.
pub async fn run_with(
    ctx: &Ctx,
    out: &Out,
    args: StreamArgs,
    signals: impl FnOnce() -> Signals,
) -> Result<ExitCode, CliError> {
    let config = Config::load(&ctx.paths.config_file)?;
    let stream = args.stream_config(config.stream)?;
    if let Some(warning) = stream.codec.viewer_warning() {
        out.warn(warning);
    }
    let capture = args.capture_config(&stream, args.crop.or(config.crop));
    let restore_token = capture::restore_token(&config);
    let capture::Picked {
        source,
        portal_token,
    } = capture::source_for(&args.target, restore_token.clone())?;

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
    if let Err(e) = start(&session, args.guild, args.channel, source, capture).await {
        teardown(session, client).await;
        return Err(e);
    }

    let mut signals = signals();
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

    if !out.json {
        out.note("stopping…");
    }
    let forced = tokio::select! {
        biased;
        () = signals.recv() => true,
        () = teardown(session, client) => false,
    };
    if forced {
        return Ok(ExitCode::from(FORCED_EXIT));
    }
    while let Ok(event) = events.try_recv() {
        print_event(out, &event);
    }
    if let Err(e) =
        capture::remember_portal_token(&ctx.paths, restore_token.as_deref(), &portal_token)
    {
        out.warn(format_args!("saving the portal restore token: {e}"));
    }

    match outcome {
        Outcome::Stopped(reason) if !stopped_cleanly(&reason) => {
            Err(CliError::Stopped(reason.to_string()))
        }
        Outcome::Interrupted | Outcome::Stopped(_) => Ok(ExitCode::SUCCESS),
    }
}

pub async fn start(
    session: &Session,
    guild: String,
    channel: String,
    source: Box<dyn CaptureSource>,
    capture: CaptureConfig,
) -> Result<(), CliError> {
    let options = SessionOptions::new(StreamOptions::new(capture));
    session
        .start(Some(guild), channel, source, options)
        .await
        .map_err(|e| match e.error {
            SessionError::AlreadyRunning => CliError::AlreadyStreaming,
            error => CliError::Session(error.to_string()),
        })
}

pub async fn teardown(session: Session, client: Arc<DiscordClient>) {
    session.shutdown().await;
    close(client).await;
}

async fn close(client: Arc<DiscordClient>) {
    match Arc::try_unwrap(client) {
        Ok(client) => {
            let _ = timeout(CLOSE_TIMEOUT, client.close()).await;
        }
        Err(_) => tracing::debug!("client still shared; dropping it without a close frame"),
    }
}

pub fn stopped_cleanly(reason: &StopReason) -> bool {
    matches!(reason, StopReason::Requested | StopReason::CaptureEnded)
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
            h.health.video_level.label(),
            h.health.video_score,
            h.health.audio_level.label(),
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
        assert_eq!(c.crop, Some(CropRect::new(1, 2, 30, 40)));
    }
}
