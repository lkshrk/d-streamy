use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use dstreamy_core::{Codec, Config, CropRect, Fps, Resolution, StreamConfig};
use dstreamy_discord::{
    DiscordClient, HealthSnapshot, Session, SessionError, SessionEvent, SessionOptions,
    SessionState, StopReason, StreamOptions,
};
use dstreamy_media::{CaptureConfig, CaptureSource, VideoCodec};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::timeout;

use crate::discord::{self, Persistence};
use crate::lock::SessionLock;
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
    /// Audio device to capture, see `dstreamy audio-sources`; by default what plays on the default output
    #[arg(long, value_name = "ID")]
    pub audio_source: Option<String>,
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

pub fn capture_config(
    target: &str,
    stream: &StreamConfig,
    crop: Option<CropRect>,
    audio_source: Option<String>,
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
        audio_source,
        audio_gain: stream.audio_gain,
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
    let _lock = SessionLock::acquire(&ctx.paths)?;
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

/// Returns once the run has left the previous state, so a `Stopped` seen afterwards is this run's.
pub async fn start(
    session: &Session,
    guild: String,
    channel: String,
    source: Box<dyn CaptureSource>,
    capture: CaptureConfig,
) -> Result<(), CliError> {
    let options = SessionOptions::new(StreamOptions::new(capture));
    let mut state = session.state();
    state.borrow_and_update();
    session
        .start(Some(guild), channel, source, options)
        .await
        .map_err(|e| match e.error {
            SessionError::AlreadyRunning => CliError::AlreadyStreaming,
            error => CliError::Session(error.to_string()),
        })?;
    let _ = state.changed().await;
    Ok(())
}

pub fn has_stopped(session: &Session) -> bool {
    matches!(*session.state().borrow(), SessionState::Stopped { .. })
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
        Err(_) => {
            tracing::warn!("the Discord client is still shared; dropping it without a close frame")
        }
    }
}

pub fn stopped_cleanly(reason: &StopReason) -> bool {
    matches!(reason, StopReason::Requested | StopReason::CaptureEnded)
}

fn health_line(h: &HealthSnapshot) -> String {
    let mut line = format!(
        "{} fps  {} kbps  video {} ({})  audio {} ({})  dropped {}  viewer keyframe requests {}",
        h.metrics.fps,
        h.metrics.bitrate_kbps,
        h.health.video_level.label(),
        h.health.video_score,
        h.health.audio_level.label(),
        h.health.audio_score,
        h.stats.video_frames_dropped,
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

    #[test]
    fn audio_sources_are_checked_against_the_list_when_there_is_one() {
        use dstreamy_media::{AudioSource, AudioSourceKind};
        let listed = [AudioSource {
            id: "pipeweaver_stream".into(),
            kind: AudioSourceKind::Input,
            name: "PipeWeaver Stream".into(),
        }];
        let check =
            |id: &str, listed: &[AudioSource]| capture::check_audio_source(id, Ok(listed.to_vec()));
        assert!(matches!(check("pipeweaver_stream", &listed), Ok(None)));
        let err = check("nope", &listed).unwrap_err();
        assert!(err.to_string().contains("dstreamy audio-sources"), "{err}");
        let warning = check("anything", &[]).unwrap().unwrap();
        assert!(warning.contains("\"anything\""), "{warning}");
        assert!(
            warning.contains("fails rather than capture another device"),
            "{warning}"
        );
        let failed = capture::check_audio_source(
            "pipeweaver_stream",
            Err(dstreamy_media::MediaError::Backend("no pulse".into())),
        )
        .unwrap()
        .unwrap();
        assert!(
            failed.contains("listing the audio devices failed: "),
            "{failed}"
        );
        assert!(check("", &[]).is_err());
        assert!(
            capture::check_audio_source(" ", Err(dstreamy_media::MediaError::Backend("x".into())))
                .is_err()
        );
    }
}
