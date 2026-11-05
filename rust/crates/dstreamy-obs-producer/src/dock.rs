//! The dock's state without Qt or libobs: its settings, the guild and channel pickers, and the session it shows.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::control::{ControlError, Named, Ownership, StartFailed, StartRequest, Started};
use crate::settings::{fit_1080p, target_fps};

pub const DEFAULT_VIDEO_ENCODER: &str = "obs_x264";
pub const DEFAULT_BITRATE_KBPS: i64 = 6000;
/// The range the output's bitrate property offers.
pub const BITRATE_KBPS: std::ops::RangeInclusive<i64> = 500..=50_000;
/// OBS's mixer tracks, one-based.
pub const AUDIO_TRACKS: std::ops::RangeInclusive<u8> = 1..=6;
/// The last track, which OBS's default stream and recording settings leave unused.
pub const DEFAULT_AUDIO_TRACK: u8 = 6;
pub const AUDIO_TRACK_TITLE: &str = "Discord audio track";

/// OBS's fixed name for a mixer track; D-Streamy keeps no labels of its own.
pub fn track_name(track: u8) -> String {
    format!("Track {track}")
}

/// serve refuses a longer per-start grace (ADR 0004).
pub const MAX_GRACE_SECS: u64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoMode {
    #[default]
    Own,
    Share,
}

/// What the module config keeps between OBS runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DockSettings {
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
    /// Replaces serve's grace period for sessions this plugin starts.
    pub grace_secs: Option<u64>,
    pub mode: VideoMode,
    pub video_encoder: String,
    pub bitrate_kbps: i64,
    pub share_fallback: bool,
    pub start_with_stream: bool,
    /// The OBS mixer track sent to Discord, one-based.
    pub audio_track: u8,
    /// The track D-Streamy took over; managed only while it equals `audio_track`.
    pub managed_track: Option<u8>,
    pub owned: Option<Ownership>,
}

impl Default for DockSettings {
    fn default() -> Self {
        Self {
            guild_id: None,
            channel_id: None,
            grace_secs: None,
            mode: VideoMode::Own,
            video_encoder: DEFAULT_VIDEO_ENCODER.to_owned(),
            bitrate_kbps: DEFAULT_BITRATE_KBPS,
            share_fallback: true,
            start_with_stream: false,
            audio_track: DEFAULT_AUDIO_TRACK,
            managed_track: None,
            owned: None,
        }
    }
}

impl DockSettings {
    /// Reads the module config field by field, so one unreadable field falls back alone and never takes `owned` along.
    pub fn from_json(text: &str) -> Self {
        let d = Self::default();
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) else {
            return d;
        };
        fn field<T: serde::de::DeserializeOwned>(
            map: &serde_json::Map<String, Value>,
            key: &str,
        ) -> Option<T> {
            map.get(key)
                .and_then(|v| serde_json::from_value(v.clone()).ok())
        }
        let bitrate = map
            .get("bitrate_kbps")
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)));
        Self {
            guild_id: field(&map, "guild_id"),
            channel_id: field(&map, "channel_id"),
            grace_secs: field(&map, "grace_secs"),
            mode: field(&map, "mode").unwrap_or(d.mode),
            video_encoder: field(&map, "video_encoder").unwrap_or(d.video_encoder),
            bitrate_kbps: bitrate.unwrap_or(d.bitrate_kbps),
            share_fallback: field(&map, "share_fallback").unwrap_or(d.share_fallback),
            start_with_stream: field(&map, "start_with_stream").unwrap_or(d.start_with_stream),
            audio_track: map
                .get("audio_track")
                .and_then(|v| {
                    v.as_i64()
                        .or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
                })
                .and_then(|n| u8::try_from(n).ok())
                .unwrap_or(d.audio_track),
            managed_track: field::<u8>(&map, "managed_track").filter(|t| AUDIO_TRACKS.contains(t)),
            owned: field(&map, "owned"),
        }
        .clamped()
    }

    fn clamped(self) -> Self {
        Self {
            bitrate_kbps: self
                .bitrate_kbps
                .clamp(*BITRATE_KBPS.start(), *BITRATE_KBPS.end()),
            grace_secs: self.grace_secs.map(|s| s.min(MAX_GRACE_SECS)),
            audio_track: if AUDIO_TRACKS.contains(&self.audio_track) {
                self.audio_track
            } else {
                DEFAULT_AUDIO_TRACK
            },
            managed_track: self.managed_track.filter(|t| AUDIO_TRACKS.contains(t)),
            ..self
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("settings serialize")
    }

    /// The `dstreamy_output` settings these map to.
    pub fn output_settings(&self) -> Value {
        json!({
            "mode": self.mode,
            "video_encoder": self.video_encoder,
            "bitrate": self.bitrate_kbps,
            "share_fallback": self.share_fallback,
            "audio_track": self.audio_track,
        })
    }
}

/// OBS's video settings, as `obs_get_video_info` reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Canvas {
    pub fps_num: u32,
    pub fps_den: u32,
    pub output_width: u32,
    pub output_height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DockState {
    Idle,
    Starting,
    Live {
        channel: String,
        /// `None` when attached to a session another client started, which rules out share mode.
        session_fps: Option<u32>,
    },
    Stopping,
}

/// The selected track against the one bound to the running session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackStatus {
    Idle { selected: u8 },
    Live { track: u8 },
    Pending { live: u8, next: u8 },
}

/// The settings dialog's picks; `None` is its "Pick a …" entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
    pub audio_track: u8,
}

/// What went wrong: a short headline and fix for the dock, and the full message the dialog shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub headline: &'static str,
    pub fix: &'static str,
    pub message: String,
    /// It came from loading the server or channel list, so the next good load clears it.
    pub lists: bool,
}

impl Problem {
    pub fn new(headline: &'static str, fix: &'static str, message: impl Into<String>) -> Self {
        Self {
            headline,
            fix,
            message: message.into(),
            lists: false,
        }
    }

    pub fn control(error: &ControlError) -> Self {
        Self {
            headline: error.headline(),
            fix: error.fix(),
            message: error.to_string(),
            lists: false,
        }
    }
}

const NO_CHANNEL: &str = "No voice channel picked";
const CANT_START: &str = "Can't start";
const DIDNT_START: &str = "Didn't start";
const STOP_UNFINISHED: &str = "Stop didn't finish";
const STREAM_STOPPED: &str = "Stream stopped";

/// What the dock suggests for a start or stop that couldn't finish; the caller knows the cause.
pub const FIX_LOG: &str = "See the OBS log";
pub const FIX_RESTART_OBS: &str = "Restart OBS";
pub const FIX_AFTER_CLOSING: &str = "Start again after OBS restarts";
pub const FIX_VIDEO: &str = "Check OBS's Settings › Video";
pub const FIX_OUTPUT_SETTING: &str = "Change the OBS setting it names";
pub const FIX_PRESS_START: &str = "Press Start again";
pub const FIX_START_THEN_STOP: &str = "Press Start, then Stop";

pub const PICK_CHANNEL: &str = "Pick a server and a voice channel first";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockModel {
    settings: DockSettings,
    guilds: Vec<Named>,
    channels: Vec<Named>,
    state: DockState,
    error: Option<Problem>,
    /// The track the running output was started with; set once its start is accepted, cleared when the session ends.
    live_track: Option<u8>,
    /// The track written to the output's settings for the start in progress.
    applying: Option<u8>,
    /// List load jobs sent and not yet ended; the lists show loading until the last one ends.
    lists_pending: u32,
}

impl DockModel {
    pub fn new(settings: DockSettings) -> Self {
        Self {
            settings: settings.clamped(),
            guilds: Vec::new(),
            channels: Vec::new(),
            state: DockState::Idle,
            error: None,
            live_track: None,
            applying: None,
            lists_pending: 0,
        }
    }

    /// A start, stop or settings problem outlives a list load; only a list's own problem goes.
    fn clear_list_problem(&mut self) {
        if self.error.as_ref().is_some_and(|p| p.lists) {
            self.error = None;
        }
    }

    /// A list load job was sent.
    pub fn lists_requested(&mut self) {
        self.lists_pending += 1;
    }

    /// A list load job ended, whatever it got.
    pub fn lists_finished(&mut self) {
        self.lists_pending = self.lists_pending.saturating_sub(1);
    }

    /// Whether the server list, and the selected server's channels, are still loading.
    pub fn loading(&self) -> (bool, bool) {
        let loading = self.lists_pending > 0;
        (loading, loading && self.settings.guild_id.is_some())
    }

    pub fn live_track(&self) -> Option<u8> {
        self.live_track
    }

    pub fn track_status(&self) -> TrackStatus {
        let selected = self.settings.audio_track;
        match self.live_track {
            None => TrackStatus::Idle { selected },
            Some(track) if track == selected => TrackStatus::Live { track },
            Some(live) => TrackStatus::Pending {
                live,
                next: selected,
            },
        }
    }

    /// Selects the track the next start binds; never touches a running session. False when out of range.
    pub fn select_audio_track(&mut self, track: u8) -> bool {
        if !AUDIO_TRACKS.contains(&track) {
            return false;
        }
        self.settings.audio_track = track;
        true
    }

    /// Whether D-Streamy manages the selected track's source flags.
    pub fn managed(&self) -> bool {
        self.settings.managed_track == Some(self.settings.audio_track)
    }

    /// Marks the selected track as managed and returns it; the caller clears it on every source first.
    pub fn take_over(&mut self) -> u8 {
        self.settings.managed_track = Some(self.settings.audio_track);
        self.settings.audio_track
    }

    /// One line for the settings dialog; `controls` says whether it can start and stop D-Streamy.
    pub fn status_text(&self, controls: bool) -> String {
        let pending = match self.track_status() {
            TrackStatus::Pending { next, .. } => {
                format!(" — {} applies on next Go Live", track_name(next))
            }
            _ => String::new(),
        };
        let live = self.live_track.map(track_name).unwrap_or_default();
        match &self.state {
            DockState::Idle => match (&self.error, controls) {
                (Some(error), _) => error.message.clone(),
                (None, true) => "Idle".into(),
                (None, false) => "Idle — start controls are unavailable".into(),
            },
            DockState::Starting => format!(
                "Starting on {}…",
                track_name(self.applying.unwrap_or(self.settings.audio_track))
            ),
            DockState::Live { .. } => format!("Live on {live}{pending}"),
            DockState::Stopping => "Stopping…".into(),
        }
    }

    pub fn settings(&self) -> &DockSettings {
        &self.settings
    }

    pub fn state(&self) -> &DockState {
        &self.state
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_ref().map(|p| p.message.as_str())
    }

    pub fn problem(&self) -> Option<&Problem> {
        self.error.as_ref()
    }

    pub fn guilds(&self) -> &[Named] {
        &self.guilds
    }

    pub fn channels(&self) -> &[Named] {
        &self.channels
    }

    /// Replaces every setting but the ownership, and the guild and channel unless idle; returns a guild to load.
    pub fn set_settings(&mut self, settings: DockSettings) -> Option<String> {
        let settings = settings.clamped();
        let (guild_id, channel_id) = if self.state == DockState::Idle {
            (settings.guild_id, settings.channel_id)
        } else {
            (
                self.settings.guild_id.clone(),
                self.settings.channel_id.clone(),
            )
        };
        let guild_changed = guild_id != self.settings.guild_id;
        self.settings = DockSettings {
            guild_id,
            channel_id,
            owned: self.settings.owned.take(),
            ..settings
        };
        if !guild_changed {
            return None;
        }
        self.channels.clear();
        self.settings.guild_id.clone()
    }

    /// Takes settings read back from the module config; its ownership counts only while no session runs.
    pub fn reload(&mut self, settings: DockSettings) -> Option<String> {
        let owned = settings.owned.clone();
        let guild = self.set_settings(settings);
        if self.state == DockState::Idle {
            self.settings.owned = owned;
        }
        guild
    }

    /// Returns the guild whose channels to load next.
    pub fn guilds_loaded(&mut self, result: Result<Vec<Named>, ControlError>) -> Option<String> {
        match result {
            Ok(guilds) => {
                self.guilds = guilds;
                self.clear_list_problem();
                // A saved server the list lacks stays selected and shown until the user picks another.
                let selected = self.settings.guild_id.clone()?;
                if self.guilds.iter().any(|g| g.id == selected) {
                    Some(selected)
                } else {
                    self.channels.clear();
                    None
                }
            }
            Err(e) => {
                self.error = Some(Problem {
                    lists: true,
                    ..Problem::control(&e)
                });
                None
            }
        }
    }

    /// Returns the guild whose channels to load, or `None` when it is already selected.
    pub fn select_guild(&mut self, guild_id: &str) -> Option<String> {
        if self.settings.guild_id.as_deref() == Some(guild_id) {
            return None;
        }
        self.settings.guild_id = Some(guild_id.to_owned());
        self.settings.channel_id = None;
        self.channels.clear();
        self.error = None;
        Some(guild_id.to_owned())
    }

    /// Takes a channel list, unless the guild changed since it was asked for.
    pub fn channels_loaded(&mut self, guild_id: &str, result: Result<Vec<Named>, ControlError>) {
        if self.settings.guild_id.as_deref() != Some(guild_id) {
            return;
        }
        match result {
            Ok(channels) => {
                self.channels = channels;
                self.clear_list_problem();
            }
            Err(e) => {
                self.channels.clear();
                self.error = Some(Problem {
                    lists: true,
                    ..Problem::control(&e)
                });
            }
        }
    }

    /// Whether `channel_id` is a voice channel the service listed for the selected server.
    pub fn lists_channel(&self, channel_id: &str) -> bool {
        self.channels.iter().any(|c| c.id == channel_id)
    }

    /// Whether `guild_id` is a server the service listed.
    pub fn lists_guild(&self, guild_id: &str) -> bool {
        self.guilds.iter().any(|g| g.id == guild_id)
    }

    /// Ends a start or stop that can no longer finish, or refuses one that can't begin, with `message`.
    pub fn abort(&mut self, message: String, fix: &'static str) {
        let headline = match self.state {
            DockState::Starting => DIDNT_START,
            DockState::Stopping => STOP_UNFINISHED,
            DockState::Idle | DockState::Live { .. } => CANT_START,
        };
        self.state = DockState::Idle;
        self.applying = None;
        self.live_track = None;
        self.error = Some(Problem::new(headline, fix, message));
    }

    /// Puts back an ownership a reload replaced, when the file's is older than this one.
    pub fn restore_ownership(&mut self, owned: Option<Ownership>) {
        self.settings.owned = owned;
    }

    pub fn select_channel(&mut self, channel_id: &str) {
        self.settings.channel_id = Some(channel_id.to_owned());
        self.error = None;
    }

    /// The dialog picked a channel; taken only when the service listed it for the selected server.
    pub fn pick_channel(&mut self, channel_id: &str) -> bool {
        let listed = self.lists_channel(channel_id);
        if listed {
            self.select_channel(channel_id);
        }
        listed
    }

    pub fn choice(&self) -> Choice {
        Choice {
            guild_id: self.settings.guild_id.clone(),
            channel_id: self.settings.channel_id.clone(),
            audio_track: self.settings.audio_track,
        }
    }

    /// The dialog's server; "Pick a server" clears the server and its channel. Returns a guild whose channels to load.
    pub fn choose_guild(&mut self, guild_id: Option<&str>) -> Option<String> {
        if let Some(guild_id) = guild_id {
            return self.select_guild(guild_id);
        }
        if self.settings.guild_id.take().is_some() {
            self.settings.channel_id = None;
            self.channels.clear();
            self.error = None;
        }
        None
    }

    /// The dialog's channel: "Pick a voice channel" clears it, another is taken only when listed.
    /// Returns the channel the dialog must show.
    pub fn choose_channel(&mut self, channel_id: Option<&str>) -> Option<String> {
        match channel_id {
            None => self.settings.channel_id = None,
            Some(channel) if self.settings.channel_id.as_deref() != Some(channel) => {
                self.pick_channel(channel);
            }
            Some(_) => {}
        }
        self.settings.channel_id.clone()
    }

    /// The dialog's Cancel: back to the picks it opened with, listed or not. Returns a guild whose channels to load.
    pub fn restore_choice(&mut self, opened: &Choice) -> Option<String> {
        let load = (self.settings.guild_id != opened.guild_id).then(|| {
            self.channels.clear();
            self.settings.guild_id.clone_from(&opened.guild_id);
            opened.guild_id.clone()
        });
        self.settings.channel_id.clone_from(&opened.channel_id);
        self.select_audio_track(opened.audio_track);
        load.flatten()
    }

    /// Moves to `Starting` and returns what to send, or the reason the dock can't start.
    pub fn start(&mut self, canvas: Canvas) -> Result<StartRequest, String> {
        if self.state != DockState::Idle {
            return Err("D-Streamy is already running".into());
        }
        let request = self.start_request(canvas);
        match &request {
            Ok(_) => {
                self.state = DockState::Starting;
                self.error = None;
                self.applying = None;
            }
            Err(e) => self.error = Some(e.clone()),
        }
        request.map_err(|p| p.message)
    }

    fn start_request(&self, canvas: Canvas) -> Result<StartRequest, Problem> {
        let (Some(guild_id), Some(channel_id)) =
            (&self.settings.guild_id, &self.settings.channel_id)
        else {
            return Err(Problem::new(
                NO_CHANNEL,
                "Pick a server and voice channel",
                PICK_CHANNEL,
            ));
        };
        let (fps, _) = target_fps(canvas.fps_num, canvas.fps_den)
            .map_err(|p| Problem::new(CANT_START, FIX_VIDEO, p.message()))?;
        let (width, height) = fit_1080p(canvas.output_width, canvas.output_height);
        Ok(StartRequest {
            guild_id: guild_id.clone(),
            channel_id: channel_id.clone(),
            width,
            height,
            fps,
            grace_secs: self.settings.grace_secs,
        })
    }

    /// The ownership a start should honour, from this or an earlier OBS run.
    pub fn persisted(&self) -> Option<&Ownership> {
        self.settings.owned.as_ref()
    }

    /// The service session is up and `ownership` says whose it is; returns the output settings to start with.
    pub fn claim(&mut self, ownership: Option<Ownership>) -> Option<Value> {
        if self.state != DockState::Starting {
            return None;
        }
        self.settings.owned = ownership;
        self.applying = Some(self.settings.audio_track);
        Some(self.settings.output_settings())
    }

    /// A start's outcome, `Ok` once the output accepted its start: `ownership` replaces what was persisted,
    /// since it says whether the live session is ours.
    /// A start that failed after it was sent keeps its provisional ownership, in case serve still carried it out.
    pub fn started(&mut self, result: Result<Started, StartFailed>) {
        if self.state != DockState::Starting {
            return;
        }
        let applied = self.applying.take();
        match result {
            Ok(started) => {
                let session_fps = started.session_fps();
                let Started { ownership, channel } = started;
                self.settings.owned = ownership;
                let channel = self
                    .channels
                    .iter()
                    .find(|c| c.id == channel)
                    .map_or(channel, |c| c.name.clone());
                self.state = DockState::Live {
                    channel,
                    session_fps,
                };
                self.live_track = Some(applied.unwrap_or(self.settings.audio_track));
            }
            Err(failed) => {
                if let Some(provisional) = failed.ownership {
                    self.settings.owned = Some(*provisional);
                }
                self.state = DockState::Idle;
                self.live_track = None;
                self.error = Some(Problem::control(&failed.error));
            }
        }
    }

    /// The output refused its start after the service session came up; `session_ended` says whether that session is gone.
    pub fn output_failed(&mut self, message: String, fix: &'static str, session_ended: bool) {
        if self.state != DockState::Starting {
            return;
        }
        self.state = DockState::Idle;
        self.applying = None;
        self.live_track = None;
        self.error = Some(Problem::new(DIDNT_START, fix, message));
        if session_ended {
            self.settings.owned = None;
        }
    }

    /// Moves to `Stopping` and returns the ownership the stop should use, or `None` if not live.
    pub fn stop(&mut self) -> Option<Option<Ownership>> {
        if !matches!(self.state, DockState::Live { .. }) {
            return None;
        }
        self.state = DockState::Stopping;
        Some(self.settings.owned.clone())
    }

    /// A stop's outcome. On failure the ownership stays, so a later stop can still end the session.
    pub fn stopped(&mut self, result: Result<(), String>) {
        if self.state != DockState::Stopping {
            return;
        }
        self.state = DockState::Idle;
        self.live_track = None;
        match result {
            Ok(()) => {
                self.settings.owned = None;
                self.error = None;
            }
            Err(e) => self.error = Some(Problem::new(STOP_UNFINISHED, FIX_START_THEN_STOP, e)),
        }
    }

    /// The output ended on its own; while starting or stopping, the pending reply decides instead.
    pub fn ended(&mut self, message: Option<String>) {
        if !matches!(self.state, DockState::Live { .. }) {
            return;
        }
        self.state = DockState::Idle;
        self.live_track = None;
        self.error =
            message.map(|m| Problem::new(STREAM_STOPPED, "Press Start to go live again", m));
    }

    /// The session's fps for share mode's check, while live.
    pub fn session_fps(&self) -> Option<u32> {
        match self.state {
            DockState::Live { session_fps, .. } => session_fps,
            _ => None,
        }
    }

    /// Everything the dock and the `status` vendor request show.
    pub fn view(&self) -> Value {
        json!({
            "state": self.state,
            "error": self.error(),
            "guilds": self.guilds,
            "channels": self.channels,
            "settings": self.settings,
            "live_track": self.live_track,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::Candidate;

    fn named(id: &str) -> Named {
        Named {
            id: id.into(),
            name: id.to_uppercase(),
        }
    }

    fn canvas() -> Canvas {
        Canvas {
            fps_num: 60,
            fps_den: 1,
            output_width: 2560,
            output_height: 1440,
        }
    }

    fn ownership(connection: u64) -> Ownership {
        ownership_at(connection, None)
    }

    fn ownership_at(connection: u64, fps: Option<u32>) -> Ownership {
        Ownership {
            connections: vec![Candidate {
                id: connection,
                fps,
            }],
            guild_id: "g".into(),
            channel_id: "c".into(),
            instance: None,
        }
    }

    fn started(ownership: Option<Ownership>) -> Started {
        Started {
            ownership,
            channel: "General".into(),
        }
    }

    fn picked() -> DockModel {
        let mut m = DockModel::new(DockSettings::default());
        m.guilds_loaded(Ok(vec![named("g"), named("h")]));
        assert_eq!(m.select_guild("g"), Some("g".into()));
        m.channels_loaded("g", Ok(vec![named("c"), named("d")]));
        m.select_channel("c");
        m
    }

    #[test]
    fn settings_default_and_round_trip() {
        let d = DockSettings::default();
        assert_eq!(d.mode, VideoMode::Own);
        assert!(d.share_fallback);
        assert!(!d.start_with_stream);
        assert_eq!(DockSettings::from_json(&d.to_json()), d);
        assert_eq!(DockSettings::from_json("{}"), d);
        assert_eq!(DockSettings::from_json("not json"), d);
        assert_eq!(DockSettings::from_json("[1]"), d);
    }

    const OWNED: &str = r#""owned":{"connection":4,"guild_id":"g","channel_id":"c"}"#;

    fn with_owned(fields: &str) -> DockSettings {
        let s = DockSettings::from_json(&format!("{{{fields},{OWNED}}}"));
        assert_eq!(s.owned, Some(ownership(4)), "{fields}");
        s
    }

    #[test]
    fn a_bad_field_falls_back_alone_and_keeps_the_ownership() {
        let d = DockSettings::default();
        assert_eq!(with_owned(r#""mode":"hybrid""#).mode, d.mode);
        assert_eq!(with_owned(r#""mode":"share""#).mode, VideoMode::Share);
        assert_eq!(with_owned(r#""bitrate_kbps":6000.0"#).bitrate_kbps, 6000);
        assert_eq!(
            with_owned(r#""bitrate_kbps":"fast""#).bitrate_kbps,
            d.bitrate_kbps
        );
        assert_eq!(with_owned(r#""grace_secs":-5"#).grace_secs, None);
        assert_eq!(with_owned(r#""grace_secs":3601"#).grace_secs, Some(3600));
        assert_eq!(with_owned(r#""grace_secs":"long""#).grace_secs, None);
        assert_eq!(with_owned(r#""grace_secs":120"#).grace_secs, Some(120));
        assert_eq!(
            with_owned(r#""share_fallback":"no""#).share_fallback,
            d.share_fallback
        );
        assert_eq!(
            with_owned(r#""video_encoder":7"#).video_encoder,
            d.video_encoder
        );
        assert_eq!(with_owned(r#""guild_id":5"#).guild_id, None);
        let kept = with_owned(r#""guild_id":"g","channel_id":"c","mode":7"#);
        assert_eq!(kept.guild_id.as_deref(), Some("g"));
        assert_eq!(kept.channel_id.as_deref(), Some("c"));
        let bad_owned = DockSettings::from_json(r#"{"guild_id":"g","owned":{"connection":"x"}}"#);
        assert_eq!(bad_owned.owned, None);
        assert_eq!(bad_owned.guild_id.as_deref(), Some("g"));
    }

    #[test]
    fn bitrate_is_clamped_to_the_property_range() {
        assert_eq!(with_owned(r#""bitrate_kbps":100"#).bitrate_kbps, 500);
        assert_eq!(with_owned(r#""bitrate_kbps":90000"#).bitrate_kbps, 50_000);
        let mut m = DockModel::new(DockSettings::default());
        m.set_settings(DockSettings {
            bitrate_kbps: 0,
            ..DockSettings::default()
        });
        assert_eq!(m.settings().bitrate_kbps, 500);
    }

    #[test]
    fn output_settings_use_the_output_keys() {
        let s = DockSettings {
            mode: VideoMode::Share,
            share_fallback: false,
            ..DockSettings::default()
        };
        assert_eq!(
            s.output_settings(),
            json!({"mode": "share", "video_encoder": "obs_x264", "bitrate": 6000, "share_fallback": false, "audio_track": 6})
        );
    }

    #[test]
    fn audio_track_defaults_to_six_and_round_trips() {
        let d = DockSettings::default();
        assert_eq!(d.audio_track, 6);
        assert_eq!(DockSettings::from_json("{}").audio_track, 6);
        for track in 1..=6u8 {
            let s = DockSettings {
                audio_track: track,
                ..DockSettings::default()
            };
            let back = DockSettings::from_json(&s.to_json());
            assert_eq!(back.audio_track, track);
            assert_eq!(s.output_settings()["audio_track"], json!(track));
        }
    }

    #[test]
    fn invalid_audio_track_falls_back_to_six() {
        for bad in ["0", "7", "-1", "256", "2.5", "\"2\"", "null", "true"] {
            let s = with_owned(&format!(r#""audio_track":{bad}"#));
            assert_eq!(s.audio_track, 6, "{bad}");
        }
        assert_eq!(with_owned(r#""audio_track":3.0"#).audio_track, 3);
        let mut m = DockModel::new(DockSettings::default());
        for bad in [0u8, 7, 255] {
            m.set_settings(DockSettings {
                audio_track: bad,
                ..DockSettings::default()
            });
            assert_eq!(m.settings().audio_track, 6, "{bad}");
        }
    }

    fn with_track(m: &mut DockModel, track: u8) {
        let mut s = m.settings().clone();
        s.audio_track = track;
        m.set_settings(s);
    }

    #[test]
    fn a_new_dock_is_idle_on_track_6() {
        let m = DockModel::new(DockSettings::default());
        assert_eq!(m.live_track(), None);
        assert_eq!(m.track_status(), TrackStatus::Idle { selected: 6 });
        assert_eq!(
            m.status_text(false),
            "Idle — start controls are unavailable"
        );
        assert_eq!(m.status_text(true), "Idle");
    }

    #[test]
    fn reloading_the_config_keeps_its_ownership_unless_a_session_runs() {
        let stored = DockSettings {
            audio_track: 4,
            owned: Some(ownership(3)),
            ..picked().settings().clone()
        };
        let mut m = DockModel::new(DockSettings::default());
        m.reload(stored.clone());
        assert_eq!(m.persisted(), Some(&ownership(3)));
        assert_eq!(m.settings().audio_track, 4);

        let mut m = picked();
        m.start(canvas()).unwrap();
        m.started(Ok(started(Some(ownership(7)))));
        m.reload(stored);
        assert_eq!(m.persisted(), Some(&ownership(7)), "the live session's own");
        assert_eq!(m.settings().audio_track, 4);
        assert_eq!(m.live_track(), Some(6));
    }

    #[test]
    fn selecting_a_track_takes_only_tracks_1_to_6() {
        let mut m = DockModel::new(DockSettings::default());
        assert!(m.select_audio_track(6));
        assert_eq!(m.settings().audio_track, 6);
        for bad in [0, 7, 255] {
            assert!(!m.select_audio_track(bad), "{bad}");
            assert_eq!(m.settings().audio_track, 6);
        }
    }

    #[test]
    fn a_track_changed_while_idle_is_the_one_the_next_start_binds() {
        let mut m = picked();
        with_track(&mut m, 3);
        assert_eq!(m.track_status(), TrackStatus::Idle { selected: 3 });
        m.start(canvas()).unwrap();
        assert_eq!(m.live_track(), None, "not bound until the output accepts");
        assert_eq!(m.status_text(true), "Starting on Track 3…");
        m.started(Ok(started(None)));
        assert_eq!(m.track_status(), TrackStatus::Live { track: 3 });
        assert_eq!(m.status_text(true), "Live on Track 3");
    }

    #[test]
    fn the_track_applied_to_the_output_is_the_one_recorded_live() {
        let mut m = picked();
        with_track(&mut m, 3);
        m.start(canvas()).unwrap();
        with_track(&mut m, 4);
        let output = m.claim(Some(ownership(7))).expect("starting");
        assert_eq!(output["audio_track"], json!(4));
        assert_eq!(
            m.persisted(),
            Some(&ownership(7)),
            "claimed before the output starts"
        );
        assert_eq!(m.live_track(), None);
        assert_eq!(m.status_text(true), "Starting on Track 4…");
        with_track(&mut m, 6);
        m.started(Ok(started(Some(ownership(7)))));
        assert_eq!(m.live_track(), Some(4));
        assert_eq!(m.track_status(), TrackStatus::Pending { live: 4, next: 6 });
        assert_eq!(m.claim(None), None, "only while starting");
    }

    #[test]
    fn an_output_that_refuses_its_start_returns_to_idle_with_its_error() {
        let mut m = picked();
        m.start(canvas()).unwrap();
        m.claim(Some(ownership(7)));
        m.output_failed("no encoder".into(), FIX_OUTPUT_SETTING, false);
        assert_eq!(m.state(), &DockState::Idle);
        assert_eq!(m.error(), Some("no encoder"));
        assert_eq!(m.live_track(), None);
        assert_eq!(
            m.persisted(),
            Some(&ownership(7)),
            "the session may still run"
        );

        m.start(canvas()).unwrap();
        m.claim(Some(ownership(8)));
        m.output_failed("no encoder".into(), FIX_OUTPUT_SETTING, true);
        assert_eq!(m.persisted(), None, "its session was stopped");
        m.output_failed("late".into(), FIX_OUTPUT_SETTING, true);
        assert_eq!(m.error(), Some("no encoder"), "ignored once idle");
    }

    #[test]
    fn a_track_changed_while_live_is_kept_for_the_next_start() {
        let mut m = picked();
        with_track(&mut m, 2);
        m.start(canvas()).unwrap();
        m.started(Ok(started(None)));
        with_track(&mut m, 5);
        assert!(matches!(m.state(), DockState::Live { .. }));
        assert_eq!(m.settings().audio_track, 5);
        assert_eq!(m.settings().output_settings()["audio_track"], json!(5));
        assert_eq!(m.live_track(), Some(2));
        assert_eq!(m.track_status(), TrackStatus::Pending { live: 2, next: 5 });
        assert_eq!(
            m.status_text(false),
            "Live on Track 2 — Track 5 applies on next Go Live"
        );
        with_track(&mut m, 2);
        assert_eq!(m.track_status(), TrackStatus::Live { track: 2 });
        assert_eq!(m.view()["live_track"], json!(2));
    }

    #[test]
    fn every_way_out_of_a_session_clears_the_live_track() {
        let live = |track: u8| {
            let mut m = picked();
            with_track(&mut m, track);
            m.start(canvas()).unwrap();
            m.started(Ok(started(None)));
            with_track(&mut m, 6);
            m
        };
        let mut m = live(2);
        m.stop();
        assert_eq!(m.live_track(), Some(2), "still bound while stopping");
        assert_eq!(m.status_text(true), "Stopping…");
        m.stopped(Ok(()));
        assert_eq!(m.live_track(), None);
        assert_eq!(m.track_status(), TrackStatus::Idle { selected: 6 });

        let mut m = live(3);
        m.stop();
        m.stopped(Err("timed out".into()));
        assert_eq!(m.live_track(), None);
        assert_eq!(m.status_text(false), "timed out");

        let mut m = live(4);
        m.ended(Some("the session ended".into()));
        assert_eq!(m.live_track(), None);
        assert_eq!(m.track_status(), TrackStatus::Idle { selected: 6 });

        let mut m = picked();
        m.start(canvas()).unwrap();
        m.claim(None);
        m.started(Err(ControlError::Stopping.into()));
        assert_eq!(m.live_track(), None);
        assert_eq!(m.view()["live_track"], Value::Null);
        assert_eq!(
            m.status_text(true),
            "D-Streamy is still stopping; try again"
        );
    }

    #[test]
    fn tracks_have_fixed_names() {
        let names: Vec<String> = AUDIO_TRACKS.map(track_name).collect();
        assert_eq!(
            names,
            [
                "Track 1", "Track 2", "Track 3", "Track 4", "Track 5", "Track 6"
            ]
        );
        assert_eq!(AUDIO_TRACK_TITLE, "Discord audio track");
    }

    #[test]
    fn a_stored_audio_track_label_is_dropped() {
        let s = with_owned(r#""audio_track":3,"audio_track_label":"Music""#);
        assert_eq!(s.audio_track, 3);
        assert!(!s.to_json().contains("label"), "{}", s.to_json());
        assert!(s.output_settings().get("audio_track_label").is_none());
    }

    #[test]
    fn a_guild_change_clears_the_channel_and_drops_stale_lists() {
        let mut m = picked();
        assert_eq!(m.select_guild("g"), None);
        assert_eq!(m.select_guild("h"), Some("h".into()));
        assert_eq!(m.settings().channel_id, None);
        assert!(m.channels().is_empty());
        m.channels_loaded("g", Ok(vec![named("c")]));
        assert!(
            m.channels().is_empty(),
            "a reply for the old guild is dropped"
        );
        m.channels_loaded("h", Ok(vec![named("e")]));
        assert_eq!(m.channels(), [named("e")]);
    }

    #[test]
    fn reloaded_lists_keep_a_selection_that_still_exists() {
        let mut m = picked();
        assert_eq!(m.guilds_loaded(Ok(vec![named("g")])), Some("g".into()));
        m.channels_loaded("g", Ok(vec![named("c")]));
        assert_eq!(m.settings().channel_id.as_deref(), Some("c"));
        m.channels_loaded("g", Ok(vec![named("d")]));
        assert_eq!(
            m.settings().channel_id.as_deref(),
            Some("c"),
            "kept until the user picks"
        );
        assert!(!m.lists_channel("c"));
        assert_eq!(
            m.guilds_loaded(Ok(vec![named("h")])),
            None,
            "no channels for an unlisted server"
        );
        assert_eq!(m.settings().guild_id.as_deref(), Some("g"));
        assert!(m.channels().is_empty());
    }

    fn choice(guild: Option<&str>, channel: Option<&str>, track: u8) -> Choice {
        Choice {
            guild_id: guild.map(Into::into),
            channel_id: channel.map(Into::into),
            audio_track: track,
        }
    }

    #[test]
    fn pick_a_server_clears_the_server_and_its_channel() {
        let mut m = picked();
        assert_eq!(m.choose_guild(None), None);
        assert_eq!(m.choice(), choice(None, None, 6));
        assert!(
            m.channels().is_empty(),
            "the old server's channels are gone"
        );
        assert_eq!(
            m.start(canvas()).unwrap_err(),
            "Pick a server and a voice channel first"
        );
        assert_eq!(m.choose_guild(None), None, "already cleared");
        assert_eq!(m.choose_guild(Some("h")), Some("h".into()));
        assert_eq!(m.choice(), choice(Some("h"), None, 6));
    }

    #[test]
    fn pick_a_voice_channel_clears_only_the_channel() {
        let mut m = picked();
        assert_eq!(m.choose_channel(None), None);
        assert_eq!(m.choice(), choice(Some("g"), None, 6));
        assert!(m.lists_channel("d"), "the server's channels stay listed");
        assert_eq!(m.choose_channel(Some("d")), Some("d".into()));
        assert_eq!(
            m.choose_channel(Some("x")),
            Some("d".into()),
            "an unlisted channel is refused and the kept one shown"
        );
    }

    #[test]
    fn cancel_puts_back_what_the_dialog_opened_with_even_when_unlisted() {
        let mut m = picked();
        let opened = m.choice();
        assert_eq!(m.choose_guild(Some("h")), Some("h".into()));
        m.channels_loaded("h", Ok(vec![named("e")]));
        assert_eq!(m.choose_channel(Some("e")), Some("e".into()));
        assert!(m.select_audio_track(5));
        assert_eq!(
            m.restore_choice(&opened),
            Some("g".into()),
            "the opened server's channels are loaded again"
        );
        assert_eq!(m.choice(), opened);
        assert!(m.channels().is_empty(), "h's channels are not g's");
        assert_eq!(m.restore_choice(&opened), None, "nothing changes twice");

        m.choose_guild(None);
        assert_eq!(m.restore_choice(&opened), Some("g".into()));
        assert_eq!(
            m.choice(),
            opened,
            "the channel comes back although no list names it yet"
        );
        assert!(m.start(canvas()).is_ok());
    }

    #[test]
    fn cancel_while_live_puts_back_the_track_as_pending_only() {
        let mut m = picked();
        assert!(m.select_audio_track(3));
        m.start(canvas()).unwrap();
        m.claim(None);
        m.started(Ok(started(None)));
        let opened = m.choice();
        assert!(m.select_audio_track(5));
        assert_eq!(m.track_status(), TrackStatus::Pending { live: 3, next: 5 });
        m.restore_choice(&opened);
        assert_eq!(m.track_status(), TrackStatus::Live { track: 3 });
        assert!(matches!(m.state(), DockState::Live { .. }));
    }

    #[test]
    fn only_listed_servers_and_channels_can_be_picked() {
        let mut m = picked();
        m.guilds_loaded(Ok(vec![named("g"), named("h")]));
        m.channels_loaded("g", Ok(vec![named("c"), named("d")]));
        assert!(m.lists_guild("h") && !m.lists_guild("x"));
        assert!(m.lists_channel("d") && !m.lists_channel("e"));
        m.select_guild("h");
        assert_eq!(
            m.settings().channel_id,
            None,
            "a server change clears the channel"
        );
        assert!(!m.lists_channel("c"), "the old server's channels are gone");
        assert!(!m.pick_channel("c"), "the old server's channel is refused");
        assert_eq!(m.settings().channel_id, None);
        assert!(m.start(canvas()).is_err(), "nothing to start in");
        m.channels_loaded("h", Ok(vec![named("e")]));
        assert!(!m.pick_channel("c"), "still not one of h's");
        assert!(m.pick_channel("e"));
        let request = m.start(canvas()).unwrap();
        assert_eq!(
            (request.guild_id.as_str(), request.channel_id.as_str()),
            ("h", "e")
        );
    }

    #[test]
    fn abort_ends_a_start_or_stop_with_its_error() {
        let mut m = picked();
        m.start(canvas()).unwrap();
        m.claim(Some(ownership(7)));
        m.abort("the control step failed".into(), FIX_RESTART_OBS);
        assert_eq!(m.state(), &DockState::Idle);
        assert_eq!(m.status_text(true), "the control step failed");
        assert_eq!(
            m.persisted(),
            Some(&ownership(7)),
            "kept so a later stop can end it"
        );
        m.start(canvas()).unwrap();
        m.started(Ok(started(Some(ownership(7)))));
        m.stop();
        m.abort("gone".into(), FIX_RESTART_OBS);
        assert_eq!(m.live_track(), None);
        assert_eq!(m.state(), &DockState::Idle);
    }

    #[test]
    fn load_errors_are_shown() {
        let mut m = DockModel::new(DockSettings::default());
        m.guilds_loaded(Err(ControlError::NotLoggedIn));
        assert_eq!(m.error(), Some("Log in first: run `dstreamy login`"));
        m.guilds_loaded(Ok(vec![named("g")]));
        assert_eq!(m.error(), None);
    }

    #[test]
    fn a_good_list_load_keeps_a_start_or_stop_problem() {
        let mut m = picked();
        m.start(canvas()).unwrap();
        m.started(Err(ControlError::NotRunning("no socket".into()).into()));
        let problem = m.error().map(str::to_owned);
        assert!(problem.is_some());
        m.guilds_loaded(Ok(vec![named("g")]));
        m.channels_loaded("g", Ok(vec![named("c")]));
        assert_eq!(
            m.error().map(str::to_owned),
            problem,
            "the panel opening must not hide it"
        );

        let mut m = picked();
        m.stop();
        m.start(canvas()).unwrap();
        m.started(Ok(started(None)));
        m.stop();
        m.stopped(Err("timed out".into()));
        m.guilds_loaded(Ok(vec![named("g")]));
        assert_eq!(m.error(), Some("timed out"));

        let mut m = picked();
        m.channels_loaded("g", Err(ControlError::GuildUnavailable));
        assert!(m.problem().is_some_and(|p| p.lists));
        m.channels_loaded("g", Ok(vec![named("c")]));
        assert_eq!(
            m.error(),
            None,
            "a list's own problem goes with the next good load"
        );
    }

    #[test]
    fn a_channel_load_error_clears_the_list() {
        let mut m = picked();
        m.channels_loaded("g", Err(ControlError::GuildUnavailable));
        assert!(m.channels().is_empty());
        assert!(m.error().is_some());
    }

    #[test]
    fn picking_clears_a_stale_error() {
        let mut m = picked();
        m.channels_loaded("g", Err(ControlError::GuildUnavailable));
        m.select_channel("d");
        assert_eq!(m.error(), None);
        m.channels_loaded("g", Err(ControlError::GuildUnavailable));
        m.select_guild("h");
        assert_eq!(m.error(), None);
    }

    #[test]
    fn a_settings_guild_change_clears_the_list_and_asks_for_channels() {
        let mut m = picked();
        let next = DockSettings {
            guild_id: Some("h".into()),
            channel_id: Some("e".into()),
            ..m.settings().clone()
        };
        assert_eq!(m.set_settings(next), Some("h".into()));
        assert!(m.channels().is_empty());
        assert_eq!(m.settings().channel_id.as_deref(), Some("e"));
        m.channels_loaded("h", Ok(vec![named("f")]));
        assert_eq!(
            m.settings().channel_id.as_deref(),
            Some("e"),
            "a saved id is kept"
        );
        assert!(!m.lists_channel("e"));
        assert_eq!(m.set_settings(m.settings().clone()), None);
    }

    #[test]
    fn a_saved_channel_the_list_lacks_is_kept() {
        let mut m = picked();
        let pick = |channel: &str| DockSettings {
            channel_id: Some(channel.into()),
            ..picked().settings().clone()
        };
        assert_eq!(m.set_settings(pick("d")), None);
        assert_eq!(m.settings().channel_id.as_deref(), Some("d"));
        m.set_settings(pick("zz"));
        assert_eq!(m.settings().channel_id.as_deref(), Some("zz"));
        assert!(!m.lists_channel("zz"));
        assert_eq!(m.channels().len(), 2);
    }

    #[test]
    fn guild_and_channel_stay_while_the_dock_is_busy() {
        let mut m = picked();
        m.start(canvas()).unwrap();
        let next = DockSettings {
            guild_id: Some("h".into()),
            channel_id: Some("e".into()),
            bitrate_kbps: 2500,
            ..m.settings().clone()
        };
        assert_eq!(m.set_settings(next.clone()), None);
        assert_eq!(m.settings().guild_id.as_deref(), Some("g"));
        assert_eq!(m.settings().channel_id.as_deref(), Some("c"));
        assert_eq!(m.settings().bitrate_kbps, 2500);
        assert_eq!(m.channels().len(), 2);
        m.started(Ok(started(None)));
        assert_eq!(m.set_settings(next), None);
        assert_eq!(m.settings().guild_id.as_deref(), Some("g"));
    }

    #[test]
    fn start_takes_the_rate_and_size_from_the_canvas() {
        let mut m = picked();
        m.set_settings(DockSettings {
            grace_secs: Some(90),
            ..m.settings().clone()
        });
        let r = m.start(canvas()).unwrap();
        assert_eq!(
            r,
            StartRequest {
                guild_id: "g".into(),
                channel_id: "c".into(),
                width: 1920,
                height: 1080,
                fps: 60,
                grace_secs: Some(90),
            }
        );
        assert_eq!(m.state(), &DockState::Starting);
        assert!(m.start(canvas()).is_err(), "only one start at a time");
    }

    #[test]
    fn start_refuses_without_a_channel_or_with_an_odd_canvas() {
        let mut m = DockModel::new(DockSettings::default());
        assert_eq!(
            m.start(canvas()).unwrap_err(),
            "Pick a server and a voice channel first"
        );
        let mut m = picked();
        let err = m
            .start(Canvas {
                fps_num: 25,
                ..canvas()
            })
            .unwrap_err();
        assert!(err.contains("Common FPS Values"), "{err}");
        assert_eq!(m.state(), &DockState::Idle);
        assert_eq!(m.error(), Some(err.as_str()));
    }

    #[test]
    fn a_start_records_whether_the_session_is_ours() {
        let mut m = picked();
        m.start(canvas()).unwrap();
        m.started(Ok(started(Some(ownership(3)))));
        assert_eq!(
            m.state(),
            &DockState::Live {
                channel: "General".into(),
                session_fps: None,
            }
        );
        assert_eq!(m.persisted(), Some(&ownership(3)));
        assert!(m.start(canvas()).is_err(), "no start while live");
        assert_eq!(m.stop(), Some(Some(ownership(3))));
        m.stopped(Ok(()));
        assert_eq!(m.state(), &DockState::Idle);
        assert_eq!(m.persisted(), None);
    }

    #[test]
    fn attaching_to_a_foreign_session_drops_a_stale_ownership() {
        let mut m = DockModel::new(DockSettings {
            owned: Some(ownership(3)),
            ..picked().settings().clone()
        });
        m.start(canvas()).unwrap();
        m.started(Ok(started(None)));
        assert_eq!(m.persisted(), None);
        assert_eq!(m.stop(), Some(None));
    }

    #[test]
    fn a_failed_start_or_stop_keeps_the_ownership() {
        let mut m = DockModel::new(DockSettings {
            owned: Some(ownership(3)),
            ..picked().settings().clone()
        });
        m.start(canvas()).unwrap();
        m.started(Err(ControlError::Busy {
            channel: "Gaming".into(),
        }
        .into()));
        assert_eq!(m.state(), &DockState::Idle);
        assert_eq!(m.error(), Some("D-Streamy is already live in Gaming"));
        assert_eq!(m.persisted(), Some(&ownership(3)));
        m.start(canvas()).unwrap();
        m.started(Ok(started(Some(ownership(3)))));
        m.stop();
        m.stopped(Err("timed out".into()));
        assert_eq!(m.persisted(), Some(&ownership(3)));
        assert_eq!(m.error(), Some("timed out"));
    }

    #[test]
    fn a_start_lost_after_sending_keeps_its_provisional_ownership() {
        let mut m = DockModel::new(DockSettings {
            owned: Some(ownership(3)),
            ..picked().settings().clone()
        });
        m.start(canvas()).unwrap();
        m.started(Err(StartFailed {
            error: ControlError::Timeout {
                command: "start",
                secs: 10,
            },
            ownership: Some(Box::new(ownership(7))),
        }));
        assert_eq!(m.state(), &DockState::Idle);
        assert_eq!(m.persisted(), Some(&ownership(7)));
        assert_eq!(
            m.error(),
            Some("The D-Streamy service didn't answer start within 10 s")
        );
    }

    #[test]
    fn a_channel_id_is_shown_by_its_name_when_the_list_has_it() {
        let mut m = picked();
        m.start(canvas()).unwrap();
        m.started(Ok(Started {
            ownership: None,
            channel: "c".into(),
        }));
        assert_eq!(
            m.state(),
            &DockState::Live {
                channel: "C".into(),
                session_fps: None,
            }
        );
    }

    #[test]
    fn settings_changes_never_touch_the_ownership() {
        let mut m = DockModel::new(DockSettings {
            owned: Some(ownership(3)),
            ..DockSettings::default()
        });
        m.set_settings(DockSettings {
            bitrate_kbps: 2500,
            ..DockSettings::default()
        });
        assert_eq!(m.settings().bitrate_kbps, 2500);
        assert_eq!(m.persisted(), Some(&ownership(3)));
    }

    #[test]
    fn the_session_fps_is_known_only_for_our_own_session() {
        let mut m = picked();
        m.start(canvas()).unwrap();
        m.started(Ok(started(Some(ownership_at(3, Some(60))))));
        assert_eq!(m.session_fps(), Some(60));
        m.stop();
        assert_eq!(m.session_fps(), None);
        m.stopped(Ok(()));
        m.start(canvas()).unwrap();
        m.started(Ok(started(None)));
        assert_eq!(m.session_fps(), None);
    }

    #[test]
    fn replies_count_only_in_the_state_that_waits_for_them() {
        let mut m = picked();
        m.started(Ok(started(Some(ownership(3)))));
        assert_eq!(m.state(), &DockState::Idle);
        assert_eq!(m.persisted(), None);
        m.stopped(Ok(()));
        assert_eq!(m.state(), &DockState::Idle);
        m.start(canvas()).unwrap();
        m.stopped(Err("late".into()));
        assert_eq!(m.state(), &DockState::Starting);
        m.started(Ok(started(Some(ownership(3)))));
        m.started(Err(ControlError::Stopping.into()));
        assert!(matches!(m.state(), DockState::Live { .. }));
        assert_eq!(m.error(), None);
    }

    #[test]
    fn ended_while_starting_or_stopping_leaves_the_reply_to_decide() {
        let mut m = picked();
        m.start(canvas()).unwrap();
        m.ended(Some("output stopped".into()));
        assert_eq!(m.state(), &DockState::Starting);
        m.started(Ok(started(Some(ownership(3)))));
        assert_eq!(m.persisted(), Some(&ownership(3)));
        m.stop();
        m.ended(Some("output stopped".into()));
        assert_eq!(m.state(), &DockState::Stopping);
        m.stopped(Err("timed out".into()));
        assert_eq!(m.state(), &DockState::Idle);
        assert_eq!(m.error(), Some("timed out"));
        assert_eq!(m.persisted(), Some(&ownership(3)));
    }

    #[test]
    fn stop_needs_a_live_dock_and_ended_returns_to_idle() {
        let mut m = picked();
        assert_eq!(m.stop(), None);
        m.start(canvas()).unwrap();
        assert_eq!(m.stop(), None);
        m.started(Ok(started(None)));
        m.ended(Some("the session ended".into()));
        assert_eq!(m.state(), &DockState::Idle);
        assert_eq!(m.error(), Some("the session ended"));
        assert_eq!(m.view()["state"]["state"], "idle");
    }
}
