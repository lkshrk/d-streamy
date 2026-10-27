//! The dock's state without Qt or libobs: its settings, the guild and channel pickers, and the session it shows.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::control::{ControlError, Named, Ownership, StartFailed, StartRequest, Started};
use crate::settings::{fit_1080p, target_fps};

pub const DEFAULT_VIDEO_ENCODER: &str = "obs_x264";
pub const DEFAULT_BITRATE_KBPS: i64 = 6000;

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
            owned: None,
        }
    }
}

impl DockSettings {
    /// Reads the module config; anything unreadable falls back to the defaults.
    pub fn from_json(text: &str) -> Self {
        serde_json::from_str(text).unwrap_or_default()
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
    Live { channel: String },
    Stopping,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockModel {
    settings: DockSettings,
    guilds: Vec<Named>,
    channels: Vec<Named>,
    state: DockState,
    error: Option<String>,
}

impl DockModel {
    pub fn new(settings: DockSettings) -> Self {
        Self {
            settings,
            guilds: Vec::new(),
            channels: Vec::new(),
            state: DockState::Idle,
            error: None,
        }
    }

    pub fn settings(&self) -> &DockSettings {
        &self.settings
    }

    pub fn state(&self) -> &DockState {
        &self.state
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn guilds(&self) -> &[Named] {
        &self.guilds
    }

    pub fn channels(&self) -> &[Named] {
        &self.channels
    }

    /// Replaces every setting but the session ownership, which only starts and stops change.
    pub fn set_settings(&mut self, settings: DockSettings) {
        let owned = self.settings.owned.take();
        let guild_changed = settings.guild_id != self.settings.guild_id;
        self.settings = DockSettings { owned, ..settings };
        if guild_changed {
            self.channels.clear();
        }
    }

    /// Returns the guild whose channels to load next.
    pub fn guilds_loaded(&mut self, result: Result<Vec<Named>, ControlError>) -> Option<String> {
        match result {
            Ok(guilds) => {
                self.guilds = guilds;
                self.error = None;
                let selected = self.settings.guild_id.clone()?;
                if self.guilds.iter().any(|g| g.id == selected) {
                    Some(selected)
                } else {
                    self.settings.guild_id = None;
                    self.settings.channel_id = None;
                    self.channels.clear();
                    None
                }
            }
            Err(e) => {
                self.error = Some(e.to_string());
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
                self.error = None;
                let known = |id: &String| self.channels.iter().any(|c| &c.id == id);
                if !self.settings.channel_id.as_ref().is_some_and(known) {
                    self.settings.channel_id = None;
                }
            }
            Err(e) => {
                self.channels.clear();
                self.error = Some(e.to_string());
            }
        }
    }

    pub fn select_channel(&mut self, channel_id: &str) {
        self.settings.channel_id = Some(channel_id.to_owned());
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
            }
            Err(e) => self.error = Some(e.clone()),
        }
        request
    }

    fn start_request(&self, canvas: Canvas) -> Result<StartRequest, String> {
        let (Some(guild_id), Some(channel_id)) =
            (&self.settings.guild_id, &self.settings.channel_id)
        else {
            return Err("Pick a server and a voice channel first".into());
        };
        let (fps, _) = target_fps(canvas.fps_num, canvas.fps_den).map_err(|p| p.message())?;
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

    /// A start's outcome: `ownership` replaces what was persisted, since it says whether the live session is ours.
    /// A start that failed after it was sent keeps its provisional ownership, in case serve still carried it out.
    pub fn started(&mut self, result: Result<Started, StartFailed>) {
        if self.state != DockState::Starting {
            return;
        }
        match result {
            Ok(Started { ownership, channel }) => {
                self.settings.owned = ownership;
                let channel = self
                    .channels
                    .iter()
                    .find(|c| c.id == channel)
                    .map_or(channel, |c| c.name.clone());
                self.state = DockState::Live { channel };
            }
            Err(failed) => {
                if let Some(provisional) = failed.ownership {
                    self.settings.owned = Some(*provisional);
                }
                self.state = DockState::Idle;
                self.error = Some(failed.error.to_string());
            }
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
        match result {
            Ok(()) => {
                self.settings.owned = None;
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// The output ended without a stop from the dock, for example because the session ended.
    pub fn ended(&mut self, message: Option<String>) {
        if matches!(self.state, DockState::Idle) {
            return;
        }
        self.state = DockState::Idle;
        self.error = message;
    }

    /// Everything the dock and the `status` vendor request show.
    pub fn view(&self) -> Value {
        json!({
            "state": self.state,
            "error": self.error,
            "guilds": self.guilds,
            "channels": self.channels,
            "settings": self.settings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        Ownership {
            connection,
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
        let partial = DockSettings::from_json(
            r#"{"mode":"share","owned":{"connection":4,"guild_id":"g","channel_id":"c"}}"#,
        );
        assert_eq!(partial.mode, VideoMode::Share);
        assert_eq!(partial.owned, Some(ownership(4)));
        assert_eq!(partial.bitrate_kbps, DEFAULT_BITRATE_KBPS);
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
            json!({"mode": "share", "video_encoder": "obs_x264", "bitrate": 6000, "share_fallback": false})
        );
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
        assert_eq!(m.settings().channel_id, None);
        assert_eq!(m.guilds_loaded(Ok(vec![named("h")])), None);
        assert_eq!(m.settings().guild_id, None);
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
                channel: "General".into()
            }
        );
        assert_eq!(m.persisted(), Some(&ownership(3)));
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
                channel: "C".into()
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
