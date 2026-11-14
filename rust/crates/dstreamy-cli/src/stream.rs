use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use dstreamy_core::{Codec, Config, CropRect, Fps, Resolution, StreamConfig};
use dstreamy_discord::{HealthSnapshot, Session, SessionEvent, SessionState, StopReason};
use dstreamy_media::CaptureConfig;
use tokio::sync::broadcast::error::RecvError;

use crate::discord::{self, Persistence};
use crate::output::Out;
use crate::session::{SessionEvents, capture_config, start, teardown};
use crate::session::{close, stopped_cleanly};
use crate::signals::Signals;
use crate::{CliError, Ctx, capture};

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
    /// Capture target id from `dstreamy targets`
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
    /// Audio device to capture, see `dstreamy audio-sources`; by default what plays on the default output
    #[arg(long, value_name = "ID")]
    pub audio_source: Option<String>,
    /// Refuse to stream unless Discord provides end-to-end encryption
    #[arg(long)]
    pub require_e2ee: bool,
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
            require_e2ee: self.require_e2ee || base.require_e2ee,
        };
        config.validate()?;
        Ok(config)
    }

    fn capture_config(&self, stream: &StreamConfig, config: &Config) -> CaptureConfig {
        capture_config(
            &self.target,
            stream,
            self.crop.or(config.crop),
            self.audio_source
                .clone()
                .or_else(|| config.audio_source.clone()),
        )
    }
}

/// How long captured audio may stay silent before the stream warns about it.
pub const SILENCE_WARNING: Duration = Duration::from_secs(10);
const SILENCE_CHECK: Duration = Duration::from_secs(1);

#[derive(Debug, PartialEq, Eq)]
pub enum AudioNotice {
    Warn(String),
    Note(String),
}

/// Warns once per stretch of silence that lasts [`SILENCE_WARNING`], and once per loss of the audio device.
#[derive(Debug, Default)]
pub struct SilenceWarning {
    warned: bool,
    losses_seen: u64,
    owe_back_note: bool,
}

impl SilenceWarning {
    /// `losses` and `recoveries` count every loss and its end so far; a burst between two checks is reported once, by its net state.
    pub fn check(
        &mut self,
        silent_for: Option<Duration>,
        losses: u64,
        recoveries: u64,
        source: Option<&str>,
    ) -> Option<AudioNotice> {
        let device = || {
            source.map_or_else(
                || "the default output".to_owned(),
                |id| format!("audio source {id}"),
            )
        };
        let new_losses = losses.saturating_sub(self.losses_seen);
        let new_loss = !self.owe_back_note && new_losses > 0;
        let back = self.owe_back_note && recoveries >= losses;
        if new_loss || back {
            self.losses_seen = losses;
            self.owe_back_note = new_loss;
            self.warned = true;
            let times = if new_losses > 1 {
                format!(" (lost {new_losses} times since the last check)")
            } else {
                String::new()
            };
            return Some(if new_loss {
                AudioNotice::Warn(match source {
                    Some(id) => format!(
                        "audio source {id} disappeared, so viewers hear silence; video keeps \
                         streaming and audio resumes from the same device when it is back, never \
                         from another one. Restart or reconnect it, or stop and pick another from \
                         `dstreamy audio-sources`{times}"
                    ),
                    None => format!(
                        "audio capture from the default output was lost, so viewers hear \
                         silence; video keeps streaming and audio resumes from whatever the \
                         default output is once capture works again{times}"
                    ),
                })
            } else {
                AudioNotice::Note(format!("{} is back; viewers hear it again", device()))
            });
        }
        if self.owe_back_note || losses > self.losses_seen {
            return None;
        }
        let silent = silent_for.is_some_and(|d| d >= SILENCE_WARNING);
        if !silent {
            self.warned = false;
            return None;
        }
        if self.warned {
            return None;
        }
        self.warned = true;
        let secs = SILENCE_WARNING.as_secs();
        Some(AudioNotice::Warn(match source {
            Some(id) => format!(
                "no sound from audio source {id} for {secs} s, so viewers hear silence; \
                 `dstreamy audio-sources` lists the other devices"
            ),
            None => format!(
                "no sound on the default output for {secs} s, so viewers hear silence; \
                 if your apps play on another device, pick it with --audio-source \
                 (see `dstreamy audio-sources`)"
            ),
        }))
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
    let capture = args.capture_config(&stream, &config);
    let resolved = crate::accounts::resolve(ctx)?;
    let _lock = crate::accounts::session_lock(ctx, &resolved.source)?;
    let restore_token = capture::restore_token(&config);
    let capture::Picked {
        source,
        portal_token,
    } = capture::source_for(&args.target, restore_token.clone())?;
    let audio_source = capture.audio_source.clone();
    if let Some(id) = &audio_source
        && let Some(warning) = capture::verify_audio_source(&args.target, id, &*source).await?
    {
        out.warn(warning);
    }
    let activity = source.audio_activity();
    let mut silence = SilenceWarning::default();
    let mut silence_check = tokio::time::interval(SILENCE_CHECK);

    let client = Arc::new(discord::connect_resolved(ctx, resolved, Persistence::Long).await?);
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
    let mut events = SessionEvents::new(&session);
    if let Err(e) = start(
        &session,
        args.guild,
        args.channel,
        source,
        capture,
        stream.require_e2ee,
    )
    .await
    {
        teardown(session, client).await;
        return Err(e);
    }

    let mut signals = signals();
    let outcome = loop {
        tokio::select! {
            () = signals.recv() => break Outcome::Interrupted,
            _ = silence_check.tick(), if activity.is_some() => {
                let silent_for = activity.as_ref().and_then(|a| a.silent_for());
                let losses = activity.as_ref().map_or(0, |a| a.losses());
                let recoveries = activity.as_ref().map_or(0, |a| a.recoveries());
                match silence.check(silent_for, losses, recoveries, audio_source.as_deref()) {
                    Some(AudioNotice::Warn(warning)) => out.warn(warning),
                    Some(AudioNotice::Note(note)) => out.note(note),
                    None => {}
                }
            }
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

fn health_line(h: &HealthSnapshot) -> String {
    let mut line = format!(
        "{} fps  {} kbps  video {} ({})  audio {} ({})  dropped {}  capture dropped {} video / {} audio  viewer keyframe requests {}",
        h.metrics.fps,
        h.metrics.bitrate_kbps,
        h.health.video_level.label(),
        h.health.video_score,
        h.health.audio_level.label(),
        h.health.audio_score,
        h.stats.video_frames_dropped,
        h.stats.source_video_frames_dropped,
        h.stats.source_audio_frames_dropped,
        h.stats.viewer_keyframe_requests,
    );
    if let Some(n) = h.stats.viewers {
        line.push_str(&format!("  {n} watching"));
    }
    line
}

fn print_event(out: &Out, event: &SessionEvent) {
    if out.json {
        out.json_line(event);
        return;
    }
    match event {
        SessionEvent::State { state } => out.line(format_args!("state: {state}")),
        SessionEvent::Health(h) => out.line(health_line(h)),
        SessionEvent::ChannelMoved { channel_id, .. } => {
            out.line(format_args!("moved to channel {channel_id}"));
        }
        SessionEvent::Resumed => out.line("connection resumed"),
        SessionEvent::E2ee {
            state: dstreamy_discord::E2ee::Encrypted,
            dave_protocol_version,
        } => out.line(format_args!(
            "end-to-end encryption on (DAVE protocol {dave_protocol_version})"
        )),
        SessionEvent::E2ee {
            dave_protocol_version,
            ..
        } => out.warn(format_args!(
            "end-to-end encryption off (DAVE protocol {dave_protocol_version}); Discord can read this stream"
        )),
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
    use dstreamy_media::VideoCodec;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_source_that_fails_after_the_portal_step_leaves_its_token_saved() {
        use dstreamy_core::ConfigPaths;
        use dstreamy_mock_discord::{
            MOCK_GUILD_ID, MOCK_TOKEN, MOCK_VOICE_CHANNEL_ID, MockDiscord,
        };

        let mock = MockDiscord::start().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::in_dir(dir.path());
        paths.save_token(MOCK_TOKEN).unwrap();
        Config {
            target: Some(dstreamy_core::CaptureTarget::Portal {
                restore_token: Some("spent".into()),
            }),
            ..Config::default()
        }
        .save(&paths.config_file)
        .unwrap();
        let ctx = Ctx {
            paths: paths.clone(),
            gateway_url: mock.gateway_url(),
            account: None,
        };
        let args = StreamArgs {
            guild: MOCK_GUILD_ID.into(),
            channel: MOCK_VOICE_CHANNEL_ID.into(),
            target: capture::FAILING_PORTAL_TARGET.into(),
            crop: None,
            codec: None,
            fps: None,
            res: None,
            bitrate: None,
            gain: None,
            audio_source: None,
            require_e2ee: false,
        };
        let (_stop, stop_rx) = tokio::sync::mpsc::unbounded_channel();
        let ran = tokio::time::timeout(
            Duration::from_secs(30),
            run_with(&ctx, &Out::new(true), args, || {
                Signals::from_channel(stop_rx)
            }),
        )
        .await
        .expect("the stream never ended");
        assert!(ran.is_err(), "{ran:?}");
        let config = Config::load(&paths.config_file).unwrap();
        assert_eq!(
            capture::restore_token(&config).as_deref(),
            Some(capture::FRESH_PORTAL_TOKEN)
        );
    }

    #[test]
    fn health_line_distinguishes_capture_drops_from_transport_drops() {
        let snapshot = HealthSnapshot {
            metrics: dstreamy_core::StreamMetrics::default(),
            health: dstreamy_core::StreamHealth::compute(&[], 30),
            stats: dstreamy_discord::StreamStats {
                video_frames_dropped: 7,
                source_video_frames_dropped: 3,
                source_audio_frames_dropped: 5,
                ..Default::default()
            },
        };
        let line = health_line(&snapshot);
        assert!(line.contains("dropped 7"), "{line}");
        assert!(line.contains("capture dropped 3 video / 5 audio"), "{line}");
    }

    #[test]
    fn health_line_says_how_many_watch_once_known() {
        use dstreamy_core::{StreamHealth, StreamMetrics};
        let health = |viewers| HealthSnapshot {
            metrics: StreamMetrics::default(),
            health: StreamHealth::compute(&[StreamMetrics::default()], 30),
            stats: dstreamy_discord::StreamStats {
                viewers,
                ..Default::default()
            },
        };
        let unknown = health_line(&health(None));
        assert!(!unknown.contains("watching"), "{unknown}");
        let known = health_line(&health(Some(3)));
        assert!(
            known.ends_with("viewer keyframe requests 0  3 watching"),
            "{known}"
        );
    }

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
            audio_source: None,
            require_e2ee: false,
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
    fn encryption_can_be_required_by_a_flag_or_the_saved_config() {
        let mut flags = args();
        assert!(
            !flags
                .stream_config(StreamConfig::default())
                .unwrap()
                .require_e2ee
        );
        flags.require_e2ee = true;
        assert!(
            flags
                .stream_config(StreamConfig::default())
                .unwrap()
                .require_e2ee
        );
        flags.require_e2ee = false;
        let base = StreamConfig {
            require_e2ee: true,
            ..StreamConfig::default()
        };
        assert!(flags.stream_config(base).unwrap().require_e2ee);
    }

    #[test]
    fn capture_config_carries_every_setting() {
        let stream = StreamConfig {
            resolution: Resolution::P900,
            fps: Fps::Fps60,
            bitrate_kbps: 8_000,
            codec: Codec::H265,
            audio_gain: 1.5,
            require_e2ee: true,
        };
        let config = Config {
            crop: Some(CropRect::new(1, 2, 30, 40)),
            audio_source: Some("pipeweaver_stream".into()),
            ..Config::default()
        };
        let c = args().capture_config(&stream, &config);
        assert_eq!(c.target_id.as_deref(), Some("test-pattern"));
        assert_eq!((c.width, c.height, c.fps), (1600, 900, 60));
        assert_eq!(c.bitrate_bps, 8_000_000);
        assert_eq!(c.codec, VideoCodec::H265);
        assert_eq!(c.audio_gain, 1.5);
        assert_eq!(c.crop, Some(CropRect::new(1, 2, 30, 40)));
        assert_eq!(c.audio_source.as_deref(), Some("pipeweaver_stream"));
    }

    #[test]
    fn the_audio_source_flag_overrides_the_config_file() {
        let config = Config {
            audio_source: Some("pipeweaver_stream".into()),
            ..Config::default()
        };
        let a = StreamArgs {
            audio_source: Some("pipeweaver_music.monitor".into()),
            ..args()
        };
        let c = a.capture_config(&StreamConfig::default(), &config);
        assert_eq!(c.audio_source.as_deref(), Some("pipeweaver_music.monitor"));
        let c = args().capture_config(&StreamConfig::default(), &Config::default());
        assert_eq!(c.audio_source, None);
    }

    #[test]
    fn silence_warns_once_per_stretch_and_names_the_device() {
        let mut w = SilenceWarning::default();
        let just_under = SILENCE_WARNING - Duration::from_millis(1);
        assert_eq!(w.check(None, 0, 0, None), None);
        assert_eq!(w.check(Some(just_under), 0, 0, None), None);
        let Some(AudioNotice::Warn(first)) = w.check(Some(SILENCE_WARNING), 0, 0, None) else {
            panic!("no warning")
        };
        assert!(first.contains("--audio-source"), "{first}");
        assert_eq!(w.check(Some(SILENCE_WARNING * 3), 0, 0, None), None);
        assert_eq!(w.check(Some(Duration::ZERO), 0, 0, None), None);
        let Some(AudioNotice::Warn(again)) =
            w.check(Some(SILENCE_WARNING), 0, 0, Some("pipeweaver_stream"))
        else {
            panic!("no warning")
        };
        assert!(again.contains("pipeweaver_stream"), "{again}");
    }

    #[test]
    fn a_lost_default_output_promises_no_device_and_says_it_follows_the_default() {
        let mut w = SilenceWarning::default();
        let Some(AudioNotice::Warn(lost)) = w.check(Some(Duration::ZERO), 1, 0, None) else {
            panic!("no warning")
        };
        assert!(lost.contains("default output"), "{lost}");
        assert!(!lost.contains("never"), "{lost}");
        assert!(!lost.contains("same device"), "{lost}");
    }

    #[test]
    fn a_lost_device_is_reported_once_instead_of_as_silence_and_its_return_noted() {
        let mut w = SilenceWarning::default();
        let id = Some("pipeweaver_stream");
        let Some(AudioNotice::Warn(lost)) = w.check(Some(Duration::ZERO), 1, 0, id) else {
            panic!("no warning")
        };
        assert!(lost.contains("pipeweaver_stream disappeared"), "{lost}");
        assert!(lost.contains("never from another"), "{lost}");
        assert_eq!(w.check(Some(SILENCE_WARNING * 2), 1, 0, id), None);
        let Some(AudioNotice::Note(back)) = w.check(Some(SILENCE_WARNING * 2), 1, 1, id) else {
            panic!("no note")
        };
        assert!(back.contains("is back"), "{back}");
        assert_eq!(w.check(Some(SILENCE_WARNING * 2), 1, 1, id), None);
        assert_eq!(w.check(Some(Duration::ZERO), 1, 1, id), None);
        assert!(matches!(
            w.check(Some(SILENCE_WARNING), 1, 1, id),
            Some(AudioNotice::Warn(_))
        ));
    }

    #[test]
    fn a_loss_that_ended_between_two_checks_still_warns_then_notes_the_return() {
        let mut w = SilenceWarning::default();
        let id = Some("pipeweaver_stream");
        let mut check =
            |losses, recoveries| match w.check(Some(Duration::ZERO), losses, recoveries, id) {
                Some(AudioNotice::Warn(text)) if text.contains("times since") => "warn many",
                Some(AudioNotice::Warn(_)) => "warn",
                Some(AudioNotice::Note(_)) => "back",
                None => "-",
            };
        assert_eq!(check(0, 0), "-");
        assert_eq!(check(1, 1), "warn", "a loss that already ended");
        assert_eq!(check(1, 1), "back");
        assert_eq!(check(1, 1), "-");
        assert_eq!(check(3, 2), "warn many", "two losses, the second still on");
        assert_eq!(check(3, 2), "-", "still lost");
        assert_eq!(check(6, 5), "-", "flapping while lost adds nothing");
        assert_eq!(check(6, 6), "back");
        assert_eq!(check(6, 6), "-");
    }

    #[test]
    fn a_loss_ended_by_a_restart_gets_its_back_note_and_later_losses_still_warn() {
        let mut w = SilenceWarning::default();
        let id = Some("pipeweaver_stream");
        let activity = dstreamy_media::AudioActivity::default();
        activity.started();
        let mut check = || {
            w.check(
                Some(Duration::ZERO),
                activity.losses(),
                activity.recoveries(),
                id,
            )
        };
        activity.lost();
        assert!(matches!(check(), Some(AudioNotice::Warn(_))));
        activity.stopped();
        assert_eq!(check(), None, "a stop is no return");
        activity.started();
        assert_eq!(check(), None, "nor is a restart without samples");
        activity.samples([0]);
        assert!(
            matches!(check(), Some(AudioNotice::Note(_))),
            "samples after the restart"
        );
        assert_eq!(check(), None);
        activity.lost();
        let Some(AudioNotice::Warn(again)) = check() else {
            panic!("the next loss was not warned")
        };
        assert!(!again.contains("times since"), "{again}");
    }

    #[test]
    fn a_loss_then_a_stop_gives_no_back_note() {
        let mut w = SilenceWarning::default();
        let activity = dstreamy_media::AudioActivity::default();
        activity.started();
        activity.lost();
        let mut check = || w.check(None, activity.losses(), activity.recoveries(), None);
        assert!(matches!(check(), Some(AudioNotice::Warn(_))));
        activity.stopped();
        assert_eq!(check(), None);
        assert_eq!(check(), None);
    }
}
