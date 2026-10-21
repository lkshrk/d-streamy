use std::collections::VecDeque;

use dstreamy_core::{Codec, Config, CropRect, Fps, HealthLevel, Resolution, StreamConfig};
use dstreamy_discord::{
    Guild, HealthSnapshot, SessionEvent, SessionState, StopReason, VoiceChannel, VoiceChannelKind,
};
use dstreamy_media::{CaptureTarget, TargetKind};

use crate::capture;
use crate::stream::parse_crop;

const LOG_LINES: usize = 6;
const BITRATE_STEP: i64 = 500;
const RESOLUTIONS: [Resolution; 3] = [Resolution::P720, Resolution::P900, Resolution::P1080];
const FPS: [Fps; 3] = [Fps::Fps15, Fps::Fps30, Fps::Fps60];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Char(char),
    Interrupt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Guilds,
    Channels,
    Targets,
    Settings,
    Live,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Warn,
    Bad,
    Info,
}

impl From<HealthLevel> for Tone {
    fn from(level: HealthLevel) -> Self {
        match level {
            HealthLevel::Good => Self::Good,
            HealthLevel::Warn => Self::Warn,
            HealthLevel::Bad => Self::Bad,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub tone: Tone,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    Connecting,
    Connected { user: String },
    Failed(String),
}

#[derive(Debug, Clone)]
pub enum Msg {
    Key(Key),
    Connected {
        user: String,
    },
    ConnectFailed(String),
    Guilds(Vec<Guild>),
    Channels {
        guild_id: String,
        result: Result<Vec<VoiceChannel>, String>,
    },
    Targets {
        targets: Vec<CaptureTarget>,
        error: Option<String>,
    },
    StartFailed(String),
    Stopping,
    Session(SessionEvent),
    Notice(Notice),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    LoadChannels(String),
    Start(StartRequest),
    Stop,
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StartRequest {
    pub guild_id: String,
    pub channel_id: String,
    pub target_id: String,
    pub stream: StreamConfig,
    pub crop: Option<CropRect>,
}

#[derive(Debug, Clone)]
pub struct Picker<T> {
    pub items: Vec<T>,
    pub selected: usize,
    pub loading: bool,
}

impl<T> Picker<T> {
    fn loading() -> Self {
        Self {
            items: Vec::new(),
            selected: 0,
            loading: true,
        }
    }

    fn set(&mut self, items: Vec<T>) {
        self.items = items;
        self.selected = 0;
        self.loading = false;
    }

    fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn down(&mut self) {
        if self.selected + 1 < self.items.len() {
            self.selected += 1;
        }
    }

    pub fn current(&self) -> Option<&T> {
        self.items.get(self.selected)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Resolution,
    Fps,
    Bitrate,
    Codec,
    Gain,
    Crop,
}

impl Field {
    pub const ALL: [Self; 6] = [
        Self::Resolution,
        Self::Fps,
        Self::Bitrate,
        Self::Codec,
        Self::Gain,
        Self::Crop,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Resolution => "Resolution",
            Self::Fps => "Frame rate",
            Self::Bitrate => "Bitrate",
            Self::Codec => "Codec",
            Self::Gain => "Audio gain",
            Self::Crop => "Crop X,Y,W,H",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|f| *f == self).unwrap_or(0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub stream: StreamConfig,
    pub crop_text: String,
    pub field: Field,
}

impl Settings {
    pub fn from_config(config: &Config) -> Self {
        Self {
            stream: config.stream,
            crop_text: config
                .crop
                .map(|c| format!("{},{},{},{}", c.x, c.y, c.width, c.height))
                .unwrap_or_default(),
            field: Field::Resolution,
        }
    }

    pub fn value(&self, field: Field) -> String {
        let s = &self.stream;
        match field {
            Field::Resolution => s.resolution.to_string(),
            Field::Fps => format!("{} fps", s.fps),
            Field::Bitrate => format!("{} kbps", s.bitrate_kbps),
            Field::Codec => s.codec.to_string(),
            Field::Gain => format!("{:.1}", s.audio_gain),
            Field::Crop if self.crop_text.is_empty() => "none".to_owned(),
            Field::Crop => self.crop_text.clone(),
        }
    }

    fn step(&mut self, by: i32) {
        self.field = Field::ALL[cycle(self.field.index(), Field::ALL.len(), by)];
    }

    fn adjust(&mut self, by: i32) {
        let s = &mut self.stream;
        match self.field {
            Field::Resolution => {
                s.resolution = match RESOLUTIONS.iter().position(|r| *r == s.resolution) {
                    Some(i) => RESOLUTIONS[cycle(i, RESOLUTIONS.len(), by)],
                    None => Resolution::default(),
                };
            }
            Field::Fps => {
                let i = FPS.iter().position(|f| *f == s.fps).unwrap_or(1);
                s.fps = FPS[cycle(i, FPS.len(), by)];
            }
            Field::Bitrate => {
                let range = StreamConfig::BITRATE_KBPS;
                let next = i64::from(s.bitrate_kbps) + BITRATE_STEP * i64::from(by);
                let clamped = next.clamp(i64::from(*range.start()), i64::from(*range.end()));
                s.bitrate_kbps = u32::try_from(clamped).unwrap_or(*range.start());
            }
            Field::Codec => {
                s.codec = match s.codec {
                    Codec::H264 => Codec::H265,
                    Codec::H265 => Codec::H264,
                };
            }
            Field::Gain => {
                let tenths = ((s.audio_gain * 10.0).round() as i32 + by).clamp(0, 30);
                s.audio_gain = tenths as f32 / 10.0;
            }
            Field::Crop => {}
        }
    }

    pub fn crop(&self) -> Result<Option<CropRect>, String> {
        if self.crop_text.trim().is_empty() {
            return Ok(None);
        }
        parse_crop(&self.crop_text).map(Some)
    }
}

fn cycle(i: usize, len: usize, by: i32) -> usize {
    let len = len as i64;
    (i as i64 + i64::from(by)).rem_euclid(len) as usize
}

#[derive(Debug, Clone)]
pub struct Live {
    pub guild: String,
    pub channel: String,
    pub target: String,
    pub target_fps: u32,
    pub state: SessionState,
    pub health: Option<HealthSnapshot>,
    pub log: VecDeque<Notice>,
    pub stopping: bool,
}

impl Live {
    pub fn active(&self) -> bool {
        !matches!(self.state, SessionState::Stopped { .. })
    }

    fn log(&mut self, tone: Tone, text: impl Into<String>) {
        if self.log.len() == LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(Notice {
            tone,
            text: text.into(),
        });
    }
}

pub fn state_tone(state: &SessionState) -> Tone {
    match state {
        SessionState::Streaming => Tone::Good,
        SessionState::Idle | SessionState::Connecting | SessionState::Reconnecting { .. } => {
            Tone::Warn
        }
        SessionState::Stopped {
            reason: StopReason::Requested | StopReason::CaptureEnded,
        } => Tone::Info,
        SessionState::Stopped { .. } => Tone::Bad,
    }
}

pub struct Score {
    pub label: &'static str,
    pub percent: u32,
    pub level: &'static str,
    pub tone: Tone,
}

pub struct HealthView {
    pub video: Score,
    pub audio: Score,
    pub rows: Vec<(&'static str, String)>,
    pub fps_series: Vec<u64>,
    pub fps_max: u64,
    pub audio_series: Vec<u64>,
    pub audio_max: u64,
}

fn level_name(level: HealthLevel) -> &'static str {
    match level {
        HealthLevel::Good => "good",
        HealthLevel::Warn => "warn",
        HealthLevel::Bad => "bad",
    }
}

impl HealthView {
    pub fn new(snapshot: &HealthSnapshot, target_fps: u32) -> Self {
        let HealthSnapshot {
            metrics: m,
            health: h,
            stats,
        } = snapshot;
        let audio_per_sec = dstreamy_core::health::AUDIO_FRAMES_PER_SEC as u64;
        let truncated = if m.gap_trunc { "+" } else { "" };
        let series = |xs: &[f64]| xs.iter().map(|x| x.max(0.0).round() as u64).collect();
        Self {
            video: Score {
                label: "Video",
                percent: h.video_score,
                level: level_name(h.video_level),
                tone: h.video_level.into(),
            },
            audio: Score {
                label: "Audio",
                percent: h.audio_score,
                level: level_name(h.audio_level),
                tone: h.audio_level.into(),
            },
            rows: vec![
                ("FPS", format!("{} / {target_fps}", m.fps)),
                (
                    "Jitter",
                    format!(
                        "p95 {} ms, max {}{truncated} ms",
                        m.gap_p95_ms, m.gap_max_ms
                    ),
                ),
                (
                    "Drops",
                    format!(
                        "{} this second, {} total",
                        m.v_drop, stats.video_frames_dropped
                    ),
                ),
                (
                    "Audio",
                    format!(
                        "{} / {audio_per_sec} frames/s, {} not ready, {} errors",
                        m.a_sent, m.a_drop_not_ready, m.a_enc_err
                    ),
                ),
                ("Bitrate", format!("{} kbps", m.bitrate_kbps)),
                (
                    "Keyframes",
                    format!(
                        "{} viewer requests, {} resumes",
                        stats.viewer_keyframe_requests, stats.resumes
                    ),
                ),
            ],
            fps_series: series(&h.fps_series),
            fps_max: u64::from(target_fps),
            audio_series: series(&h.audio_series),
            audio_max: audio_per_sec,
        }
    }
}

pub fn target_label(t: &CaptureTarget) -> String {
    let kind = match t.kind {
        TargetKind::Display => "display",
        TargetKind::Window => "window",
    };
    let mut label = format!("{kind:<7}  {}", t.title);
    if let Some(app) = &t.app {
        label.push_str(&format!(" ({app})"));
    }
    if t.width > 0 {
        label.push_str(&format!("  {}x{}", t.width, t.height));
    }
    label
}

pub fn channel_label(c: &VoiceChannel) -> String {
    match c.kind {
        VoiceChannelKind::Voice => c.name.clone(),
        VoiceChannelKind::Stage => format!("{} (stage)", c.name),
    }
}

pub struct App {
    pub screen: Screen,
    pub connection: Connection,
    pub guilds: Picker<Guild>,
    pub channels: Picker<VoiceChannel>,
    pub targets: Picker<CaptureTarget>,
    pub settings: Settings,
    pub live: Option<Live>,
    pub notice: Option<Notice>,
    preferred_target: Option<String>,
}

impl App {
    pub fn new(config: &Config) -> Self {
        Self {
            screen: Screen::Guilds,
            connection: Connection::Connecting,
            guilds: Picker::loading(),
            channels: Picker::loading(),
            targets: Picker::loading(),
            settings: Settings::from_config(config),
            live: None,
            notice: None,
            preferred_target: config.target.as_ref().map(capture::target_id),
        }
    }

    pub fn streaming(&self) -> bool {
        self.live.as_ref().is_some_and(Live::active)
    }

    pub fn update(&mut self, msg: Msg) -> Option<Effect> {
        match msg {
            Msg::Key(key) => return self.key(key),
            Msg::Connected { user } => self.connection = Connection::Connected { user },
            Msg::ConnectFailed(e) => {
                self.guilds.loading = false;
                self.notify(Tone::Bad, format!("connecting to Discord failed: {e}"));
                self.connection = Connection::Failed(e);
            }
            Msg::Guilds(guilds) => {
                if guilds.is_empty() {
                    self.notify(Tone::Warn, "this account is in no guilds");
                }
                self.guilds.set(guilds);
            }
            Msg::Channels { guild_id, result } => {
                if self.guilds.current().is_some_and(|g| g.id == guild_id) {
                    match result {
                        Ok(channels) => {
                            if channels.is_empty() {
                                self.notify(Tone::Warn, "this guild has no voice channels");
                            }
                            self.channels.set(channels);
                        }
                        Err(e) => {
                            self.channels.set(Vec::new());
                            self.notify(Tone::Bad, e);
                        }
                    }
                }
            }
            Msg::Targets { targets, error } => {
                self.targets.set(targets);
                if let Some(preferred) = &self.preferred_target
                    && let Some(i) = self.targets.items.iter().position(|t| &t.id == preferred)
                {
                    self.targets.selected = i;
                }
                if let Some(e) = error {
                    self.notify(
                        Tone::Warn,
                        format!("listing screen capture targets failed: {e}"),
                    );
                }
            }
            Msg::StartFailed(e) => {
                self.live = None;
                self.screen = Screen::Settings;
                self.notify(Tone::Bad, format!("starting the stream failed: {e}"));
            }
            Msg::Stopping => {
                if let Some(live) = &mut self.live {
                    live.stopping = true;
                }
            }
            Msg::Session(event) => self.session_event(event),
            Msg::Notice(notice) => self.notice = Some(notice),
        }
        None
    }

    fn notify(&mut self, tone: Tone, text: impl Into<String>) {
        self.notice = Some(Notice {
            tone,
            text: text.into(),
        });
    }

    fn session_event(&mut self, event: SessionEvent) {
        let Some(live) = &mut self.live else { return };
        match event {
            SessionEvent::State { state } => {
                live.log(state_tone(&state), state.to_string());
                live.state = state;
                if let SessionState::Stopped { reason } = &live.state {
                    live.stopping = false;
                    if *reason == StopReason::Requested {
                        self.screen = Screen::Settings;
                        self.notify(Tone::Info, "stream stopped");
                    } else {
                        let tone = state_tone(&live.state);
                        let text = format!("stream stopped: {reason}");
                        self.notify(tone, text);
                    }
                }
            }
            SessionEvent::Health(health) => live.health = Some(health),
            SessionEvent::ChannelMoved { channel_id, .. } => {
                live.channel = self
                    .channels
                    .items
                    .iter()
                    .find(|c| c.id == channel_id)
                    .map_or_else(|| channel_id.clone(), |c| c.name.clone());
                live.log(Tone::Info, format!("moved to {}", live.channel));
            }
            SessionEvent::Resumed => live.log(Tone::Info, "connection resumed"),
            SessionEvent::Error { message, fatal } => {
                live.log(if fatal { Tone::Bad } else { Tone::Warn }, message);
            }
        }
    }

    fn key(&mut self, key: Key) -> Option<Effect> {
        if matches!(key, Key::Interrupt | Key::Char('q')) {
            return Some(Effect::Quit);
        }
        match self.screen {
            Screen::Guilds => self.guilds_key(key),
            Screen::Channels => self.channels_key(key),
            Screen::Targets => self.targets_key(key),
            Screen::Settings => self.settings_key(key),
            Screen::Live => self.live_key(key),
        }
    }

    fn guilds_key(&mut self, key: Key) -> Option<Effect> {
        match key {
            Key::Up | Key::Char('k') => self.guilds.up(),
            Key::Down | Key::Char('j') => self.guilds.down(),
            Key::Enter => {
                let guild = self.guilds.current()?;
                if !guild.available {
                    let text = format!("{} is unavailable right now", guild.name);
                    self.notify(Tone::Warn, text);
                    return None;
                }
                let id = guild.id.clone();
                self.notice = None;
                self.channels = Picker::loading();
                self.screen = Screen::Channels;
                return Some(Effect::LoadChannels(id));
            }
            _ => {}
        }
        None
    }

    fn channels_key(&mut self, key: Key) -> Option<Effect> {
        match key {
            Key::Up | Key::Char('k') => self.channels.up(),
            Key::Down | Key::Char('j') => self.channels.down(),
            Key::Enter if self.channels.current().is_some() => {
                self.notice = None;
                self.screen = Screen::Targets;
            }
            Key::Esc => {
                self.notice = None;
                self.screen = Screen::Guilds;
            }
            _ => {}
        }
        None
    }

    fn targets_key(&mut self, key: Key) -> Option<Effect> {
        match key {
            Key::Up | Key::Char('k') => self.targets.up(),
            Key::Down | Key::Char('j') => self.targets.down(),
            Key::Enter if self.targets.current().is_some() => {
                self.notice = None;
                self.screen = Screen::Settings;
            }
            Key::Esc => {
                self.notice = None;
                self.screen = Screen::Channels;
            }
            _ => {}
        }
        None
    }

    fn settings_key(&mut self, key: Key) -> Option<Effect> {
        let editing_crop = self.settings.field == Field::Crop;
        match key {
            Key::Up | Key::BackTab | Key::Char('k') => self.settings.step(-1),
            Key::Down | Key::Tab | Key::Char('j') => self.settings.step(1),
            Key::Left | Key::Char('-') => self.settings.adjust(-1),
            Key::Right | Key::Char('+') => self.settings.adjust(1),
            Key::Char(c) if editing_crop && (c.is_ascii_digit() || c == ',') => {
                self.settings.crop_text.push(c);
            }
            Key::Backspace if editing_crop => {
                self.settings.crop_text.pop();
            }
            Key::Esc => {
                self.notice = None;
                self.screen = Screen::Targets;
            }
            Key::Enter => return self.start(),
            _ => {}
        }
        None
    }

    fn start(&mut self) -> Option<Effect> {
        let crop = match self.settings.crop() {
            Ok(crop) => crop,
            Err(e) => {
                self.notify(Tone::Bad, format!("crop: {e}"));
                return None;
            }
        };
        let guild = self.guilds.current()?;
        let channel = self.channels.current()?;
        let target = self.targets.current()?;
        let request = StartRequest {
            guild_id: guild.id.clone(),
            channel_id: channel.id.clone(),
            target_id: target.id.clone(),
            stream: self.settings.stream,
            crop,
        };
        self.live = Some(Live {
            guild: guild.name.clone(),
            channel: channel.name.clone(),
            target: target.title.clone(),
            target_fps: self.settings.stream.fps.get(),
            state: SessionState::Connecting,
            health: None,
            log: VecDeque::new(),
            stopping: false,
        });
        self.notice = None;
        self.screen = Screen::Live;
        Some(Effect::Start(request))
    }

    fn live_key(&mut self, key: Key) -> Option<Effect> {
        let live = self.live.as_ref()?;
        match key {
            Key::Char('s') | Key::Esc if live.active() && !live.stopping => Some(Effect::Stop),
            Key::Esc | Key::Enter if !live.active() => {
                self.screen = Screen::Settings;
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use dstreamy_core::{CaptureTarget as ConfigTarget, StreamHealth, StreamMetrics};
    use dstreamy_discord::StreamStats;

    use super::*;
    use crate::tui::fixtures::{channel, guild, target};

    fn keys(app: &mut App, keys: &[Key]) -> Vec<Effect> {
        keys.iter()
            .filter_map(|k| app.update(Msg::Key(*k)))
            .collect()
    }

    fn ready_app(config: &Config) -> App {
        let mut app = App::new(config);
        app.update(Msg::Connected {
            user: "mockuser".into(),
        });
        app.update(Msg::Guilds(vec![
            guild("g0", "Down", false),
            guild("g1", "Mock Guild", true),
        ]));
        app.update(Msg::Targets {
            targets: vec![
                target("display:1", "Display 1"),
                target("display:2", "Display 2"),
                target("test-pattern", "Test pattern"),
            ],
            error: None,
        });
        app
    }

    #[test]
    fn walks_from_guild_to_start_request() {
        let mut app = ready_app(&Config::default());
        assert!(keys(&mut app, &[Key::Enter]).is_empty());
        assert_eq!(app.screen, Screen::Guilds);
        assert_eq!(app.notice.as_ref().unwrap().tone, Tone::Warn);

        assert_eq!(
            keys(&mut app, &[Key::Down, Key::Down, Key::Enter]),
            [Effect::LoadChannels("g1".into())]
        );
        assert_eq!(app.screen, Screen::Channels);
        assert!(app.channels.loading);
        app.update(Msg::Channels {
            guild_id: "g1".into(),
            result: Ok(vec![channel("c1", "General"), channel("c2", "Gaming")]),
        });
        keys(&mut app, &[Key::Char('j'), Key::Enter]);
        assert_eq!(app.screen, Screen::Targets);
        keys(&mut app, &[Key::Down, Key::Down, Key::Enter]);
        assert_eq!(app.screen, Screen::Settings);

        let effects = keys(&mut app, &[Key::Enter]);
        let [Effect::Start(request)] = &effects[..] else {
            panic!("{effects:?}");
        };
        assert_eq!(
            *request,
            StartRequest {
                guild_id: "g1".into(),
                channel_id: "c2".into(),
                target_id: "test-pattern".into(),
                stream: StreamConfig::default(),
                crop: None,
            }
        );
        assert_eq!(app.screen, Screen::Live);
        assert!(app.streaming());
    }

    #[test]
    fn channels_for_another_guild_are_ignored() {
        let mut app = ready_app(&Config::default());
        keys(&mut app, &[Key::Down, Key::Enter]);
        app.update(Msg::Channels {
            guild_id: "g9".into(),
            result: Ok(vec![channel("c1", "General")]),
        });
        assert!(app.channels.loading);
        app.update(Msg::Channels {
            guild_id: "g1".into(),
            result: Err("boom".into()),
        });
        assert!(!app.channels.loading);
        assert_eq!(app.notice.as_ref().unwrap().text, "boom");
        keys(&mut app, &[Key::Enter]);
        assert_eq!(app.screen, Screen::Channels);
        keys(&mut app, &[Key::Esc]);
        assert_eq!(app.screen, Screen::Guilds);
    }

    #[test]
    fn target_picker_defaults_to_the_config_target() {
        let config = Config {
            target: Some(ConfigTarget::Display { id: 2 }),
            ..Config::default()
        };
        let app = ready_app(&config);
        assert_eq!(app.targets.current().unwrap().id, "display:2");
        let app = ready_app(&Config::default());
        assert_eq!(app.targets.selected, 0);
    }

    #[test]
    fn settings_adjust_within_bounds() {
        let mut s = Settings::from_config(&Config::default());
        s.adjust(1);
        assert_eq!(s.stream.resolution, Resolution::P720);
        s.adjust(-1);
        assert_eq!(s.stream.resolution, Resolution::P1080);
        s.stream.resolution = Resolution::new(320, 180).unwrap();
        s.adjust(1);
        assert_eq!(s.stream.resolution, Resolution::P1080);

        s.step(1);
        s.adjust(1);
        assert_eq!(s.stream.fps, Fps::Fps60);
        s.adjust(1);
        assert_eq!(s.stream.fps, Fps::Fps15);

        s.step(1);
        s.stream.bitrate_kbps = 14_800;
        s.adjust(1);
        assert_eq!(s.stream.bitrate_kbps, 15_000);
        s.stream.bitrate_kbps = 1_200;
        s.adjust(-1);
        assert_eq!(s.stream.bitrate_kbps, 1_000);

        s.step(1);
        s.adjust(1);
        assert_eq!(s.stream.codec, Codec::H265);
        s.adjust(-1);
        assert_eq!(s.stream.codec, Codec::H264);

        s.step(1);
        s.stream.audio_gain = 2.96;
        s.adjust(1);
        assert_eq!(s.stream.audio_gain, 3.0);
        s.stream.audio_gain = 0.04;
        s.adjust(-1);
        assert_eq!(s.stream.audio_gain, 0.0);
        s.adjust(1);
        assert_eq!(s.value(Field::Gain), "0.1");
        assert!(s.stream.validate().is_ok());

        s.step(1);
        assert_eq!(s.field, Field::Crop);
        s.step(1);
        assert_eq!(s.field, Field::Resolution);
        s.step(-1);
        assert_eq!(s.field, Field::Crop);
    }

    #[test]
    fn crop_is_typed_and_validated_before_start() {
        let config = Config {
            crop: Some(CropRect::new(1, 2, 3, 4)),
            ..Config::default()
        };
        let mut app = ready_app(&config);
        assert_eq!(app.settings.value(Field::Crop), "1,2,3,4");
        keys(&mut app, &[Key::Down, Key::Enter]);
        app.update(Msg::Channels {
            guild_id: "g1".into(),
            result: Ok(vec![channel("c1", "General")]),
        });
        keys(&mut app, &[Key::Enter, Key::Enter, Key::BackTab]);
        assert_eq!(app.settings.field, Field::Crop);
        keys(&mut app, &[Key::Backspace, Key::Char('x'), Key::Char('0')]);
        assert_eq!(app.settings.crop_text, "1,2,3,0");
        assert!(keys(&mut app, &[Key::Enter]).is_empty());
        assert_eq!(app.notice.as_ref().unwrap().tone, Tone::Bad);
        assert_eq!(app.screen, Screen::Settings);

        for _ in 0..7 {
            app.update(Msg::Key(Key::Backspace));
        }
        assert_eq!(app.settings.value(Field::Crop), "none");
        let typed: Vec<Key> = "9,8,7,6".chars().map(Key::Char).collect();
        keys(&mut app, &typed);
        let effects = keys(&mut app, &[Key::Enter]);
        let [Effect::Start(request)] = &effects[..] else {
            panic!("{effects:?}");
        };
        assert_eq!(request.crop, Some(CropRect::new(9, 8, 7, 6)));
    }

    fn live_app() -> App {
        let mut app = ready_app(&Config::default());
        keys(&mut app, &[Key::Down, Key::Enter]);
        app.update(Msg::Channels {
            guild_id: "g1".into(),
            result: Ok(vec![channel("c1", "General"), channel("c2", "Gaming")]),
        });
        keys(&mut app, &[Key::Enter, Key::Enter, Key::Enter]);
        app
    }

    fn state(state: SessionState) -> Msg {
        Msg::Session(SessionEvent::State { state })
    }

    #[test]
    fn stop_returns_to_settings() {
        let mut app = live_app();
        app.update(state(SessionState::Streaming));
        assert_eq!(keys(&mut app, &[Key::Char('s')]), [Effect::Stop]);
        app.update(Msg::Stopping);
        assert!(keys(&mut app, &[Key::Char('s')]).is_empty());
        app.update(state(SessionState::Stopped {
            reason: StopReason::Requested,
        }));
        assert_eq!(app.screen, Screen::Settings);
        assert!(!app.streaming());
        assert_eq!(app.notice.as_ref().unwrap().text, "stream stopped");
    }

    #[test]
    fn a_failed_stream_stays_on_the_live_screen() {
        let mut app = live_app();
        app.update(state(SessionState::Stopped {
            reason: StopReason::GaveUp { attempts: 3 },
        }));
        assert_eq!(app.screen, Screen::Live);
        assert_eq!(app.notice.as_ref().unwrap().tone, Tone::Bad);
        assert!(keys(&mut app, &[Key::Char('s')]).is_empty());
        keys(&mut app, &[Key::Esc]);
        assert_eq!(app.screen, Screen::Settings);
    }

    #[test]
    fn start_failure_goes_back_to_settings() {
        let mut app = live_app();
        app.update(Msg::StartFailed("nope".into()));
        assert_eq!(app.screen, Screen::Settings);
        assert!(app.live.is_none());
        assert!(app.notice.as_ref().unwrap().text.contains("nope"));
    }

    #[test]
    fn quit_from_any_screen() {
        for key in [Key::Char('q'), Key::Interrupt] {
            let mut app = live_app();
            assert_eq!(keys(&mut app, &[key]), [Effect::Quit]);
            let mut app = ready_app(&Config::default());
            assert_eq!(keys(&mut app, &[key]), [Effect::Quit]);
        }
    }

    #[test]
    fn session_events_update_the_live_panel() {
        let mut app = live_app();
        app.update(Msg::Session(SessionEvent::ChannelMoved {
            guild_id: Some("g1".into()),
            channel_id: "c1".into(),
        }));
        app.update(Msg::Session(SessionEvent::Resumed));
        app.update(Msg::Session(SessionEvent::Error {
            message: "hiccup".into(),
            fatal: false,
        }));
        for _ in 0..LOG_LINES {
            app.update(state(SessionState::Reconnecting { attempt: 1 }));
        }
        let live = app.live.as_ref().unwrap();
        assert_eq!(live.channel, "General");
        assert_eq!(live.log.len(), LOG_LINES);
        assert_eq!(live.state, SessionState::Reconnecting { attempt: 1 });
    }

    #[test]
    fn health_view_formats_the_snapshot() {
        let metrics = StreamMetrics {
            fps: 29,
            gap_p95_ms: 40,
            gap_max_ms: 90,
            gap_trunc: true,
            bitrate_kbps: 5_900,
            v_drop: 1,
            a_enc: 50,
            a_sent: 49,
            a_drop_not_ready: 0,
            a_enc_err: 0,
        };
        let snapshot = HealthSnapshot {
            metrics,
            health: StreamHealth::compute(&[metrics, StreamMetrics::default()], 30),
            stats: StreamStats {
                video_frames_dropped: 4,
                viewer_keyframe_requests: 2,
                ..StreamStats::default()
            },
        };
        let view = HealthView::new(&snapshot, 30);
        assert_eq!(view.video.percent, 25);
        assert_eq!((view.video.level, view.video.tone), ("bad", Tone::Bad));
        assert_eq!(view.fps_series, [29, 0]);
        assert_eq!(view.audio_series, [49, 0]);
        assert_eq!((view.fps_max, view.audio_max), (30, 50));
        let row = |name| view.rows.iter().find(|r| r.0 == name).unwrap().1.clone();
        assert_eq!(row("FPS"), "29 / 30");
        assert_eq!(row("Jitter"), "p95 40 ms, max 90+ ms");
        assert_eq!(row("Drops"), "1 this second, 4 total");
        assert_eq!(row("Audio"), "49 / 50 frames/s, 0 not ready, 0 errors");
    }

    #[test]
    fn tones_follow_levels_and_states() {
        assert_eq!(Tone::from(HealthLevel::Good), Tone::Good);
        assert_eq!(Tone::from(HealthLevel::Warn), Tone::Warn);
        assert_eq!(Tone::from(HealthLevel::Bad), Tone::Bad);
        assert_eq!(state_tone(&SessionState::Streaming), Tone::Good);
        assert_eq!(state_tone(&SessionState::Connecting), Tone::Warn);
        let stopped = |reason| SessionState::Stopped { reason };
        assert_eq!(state_tone(&stopped(StopReason::Requested)), Tone::Info);
        assert_eq!(state_tone(&stopped(StopReason::GatewayClosed)), Tone::Bad);
    }

    #[test]
    fn connect_failure_is_reported() {
        let mut app = App::new(&Config::default());
        app.update(Msg::ConnectFailed("bad token".into()));
        assert!(!app.guilds.loading);
        assert_eq!(app.connection, Connection::Failed("bad token".into()));
        assert_eq!(app.notice.as_ref().unwrap().tone, Tone::Bad);
    }
}
