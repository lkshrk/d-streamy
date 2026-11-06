//! What the OBS dock and the status-bar indicator show, from the same model and texts as the settings dialog.

use serde::Serialize;

use crate::dock::{
    DockModel, DockState, LOGGING_IN, SWITCHING_ACCOUNT, TrackStatus, WAITING_FOR_LOGIN,
    next_go_live, track_name,
};
use crate::dock_config::ConfigState;
use crate::live::{LiveStats, stats_line, stats_tooltip};
use crate::mixer::MicGuard;

/// Shown instead of the session status where the plugin has no service client yet.
pub const UNSUPPORTED: &str =
    "D-Streamy for OBS is not supported on Windows yet, so Start and Stop are disabled";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    Idle,
    Busy,
    Live,
    Attention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Start,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Button {
    pub label: &'static str,
    pub action: Action,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DockView {
    pub tone: Tone,
    /// The glyph before the headline and in the status bar.
    pub mark: &'static str,
    pub headline: String,
    pub detail: String,
    /// The detail in full, plus the track while live stats take the detail's place.
    pub detail_tooltip: String,
    pub button: Button,
    pub status_bar: String,
}

const START: &str = "Start D-Streamy";
const STOP: &str = "Stop D-Streamy";

fn button(label: &'static str, action: Action, enabled: bool) -> Button {
    Button {
        label,
        action,
        enabled,
    }
}

fn joined(first: String, second: Option<&str>) -> String {
    match second {
        Some(second) if first.is_empty() => second.to_owned(),
        Some(second) => format!("{first}. {second}"),
        None => first,
    }
}

fn with_dot(first: String, second: Option<&str>) -> String {
    match second {
        Some(second) => format!("{first} · {second}"),
        None => first,
    }
}

/// The track a session uses, and the one the next Go Live will use when they differ; `short` fits the panel.
fn track_line(m: &DockModel, short: bool) -> String {
    match m.track_status() {
        TrackStatus::Idle { selected: track } | TrackStatus::Live { track } => track_name(track),
        TrackStatus::Pending { live, next } if short => {
            format!("{} · next: {}", track_name(live), track_name(next))
        }
        TrackStatus::Pending { live, next } => {
            format!("{} · {}", track_name(live), next_go_live(next))
        }
    }
}

fn chosen_channel(m: &DockModel) -> Option<Option<&str>> {
    let settings = m.settings();
    settings.guild_id.as_ref()?;
    let channel = settings.channel_id.as_deref()?;
    Some(
        m.channels()
            .iter()
            .find(|c| c.id == channel)
            .map(|c| c.name.as_str()),
    )
}

/// The dock and status bar; `live`: the output's stats; `mic`: a mic Discord hears, or would on Go Live.
pub fn dock_view(
    state: &ConfigState,
    controls: bool,
    live: Option<&LiveStats>,
    mic: Option<&MicGuard>,
) -> DockView {
    let m = &state.model;
    let config_error = state.error();
    let config = config_error.map(|e| e.message(m.settings().audio_track));
    let mut detail_tooltip = None;
    let (tone, headline, detail, button) = if !controls {
        detail_tooltip = Some(UNSUPPORTED.to_owned());
        (
            Tone::Attention,
            "Not supported here yet".to_owned(),
            "Start and Stop are disabled".to_owned(),
            button(START, Action::Start, false),
        )
    } else {
        match m.state() {
            DockState::Idle if m.logging_in() => (
                Tone::Busy,
                LOGGING_IN.to_owned(),
                WAITING_FOR_LOGIN.to_owned(),
                button(START, Action::Start, false),
            ),
            DockState::Idle if m.account_changing() => (
                Tone::Busy,
                SWITCHING_ACCOUNT.to_owned(),
                track_line(m, true),
                button(START, Action::Start, false),
            ),
            DockState::Idle => {
                let chosen = chosen_channel(m);
                let blocked = mic.filter(|g| !g.live);
                let unavailable = m.guild_unavailable() || m.channel_unavailable();
                let start = button(
                    START,
                    Action::Start,
                    chosen.is_some() && blocked.is_none() && !unavailable,
                );
                // Without a login nothing can stream, so the login problem comes before the mic.
                if let Some(guard) = blocked.filter(|_| m.login_offer().is_none()) {
                    detail_tooltip = Some(guard.tooltip());
                    (Tone::Attention, guard.headline(), guard.fix(), start)
                } else if let Some(problem) = m.problem() {
                    detail_tooltip = Some(joined(problem.message.clone(), config.as_deref()));
                    (
                        Tone::Attention,
                        problem.headline.to_owned(),
                        problem.fix.to_owned(),
                        start,
                    )
                } else if let (Some(e), Some(config)) = (config_error, config) {
                    detail_tooltip = Some(config);
                    (
                        Tone::Attention,
                        e.headline().to_owned(),
                        e.fix().to_owned(),
                        start,
                    )
                } else {
                    let (headline, detail) = match chosen {
                        None => (
                            "Not set up".to_owned(),
                            "Pick a server and voice channel".to_owned(),
                        ),
                        Some(Some(name)) => (format!("Idle · {name}"), track_line(m, true)),
                        Some(None) => ("Idle".to_owned(), track_line(m, true)),
                    };
                    (Tone::Idle, headline, detail, start)
                }
            }
            busy_or_live => {
                let short = with_dot(track_line(m, true), config_error.map(|e| e.headline()));
                let full = with_dot(track_line(m, false), config.as_deref());
                let (detail, tooltip) = match (busy_or_live, live) {
                    (DockState::Live { .. }, Some(stats)) => (
                        stats_line(stats),
                        format!("{}\n{full}", stats_tooltip(stats)),
                    ),
                    _ => (short, full),
                };
                let leak = mic.filter(|g| g.live);
                detail_tooltip = Some(match leak {
                    Some(guard) => format!("{}\n{tooltip}", guard.tooltip()),
                    None => tooltip,
                });
                match busy_or_live {
                    DockState::Live { .. } if let Some(guard) = leak => (
                        Tone::Attention,
                        guard.headline(),
                        guard.fix(),
                        button(STOP, Action::Stop, true),
                    ),
                    DockState::Live { channel, .. } => (
                        Tone::Live,
                        format!("Live · {channel}"),
                        detail,
                        button(STOP, Action::Stop, true),
                    ),
                    DockState::Starting => (
                        Tone::Busy,
                        "Starting…".to_owned(),
                        detail,
                        button(START, Action::Start, false),
                    ),
                    _ => (
                        Tone::Busy,
                        "Stopping…".to_owned(),
                        detail,
                        button(STOP, Action::Stop, false),
                    ),
                }
            }
        }
    };
    let (mark, status_bar) = match (tone, m.state()) {
        (Tone::Attention, DockState::Live { .. }) => ("⚠", "⚠ D-Streamy live".to_owned()),
        (Tone::Attention, _) => ("⚠", "⚠ D-Streamy".to_owned()),
        (Tone::Live, _) => ("●", "● D-Streamy live".to_owned()),
        (_, DockState::Starting) => ("●", "D-Streamy starting…".to_owned()),
        (_, DockState::Stopping) => ("●", "D-Streamy stopping…".to_owned()),
        (Tone::Busy, DockState::Idle) if m.account_changing() => {
            ("●", "D-Streamy switching account…".to_owned())
        }
        (Tone::Busy, DockState::Idle) => ("●", "D-Streamy logging in…".to_owned()),
        _ => ("●", "D-Streamy idle".to_owned()),
    };
    DockView {
        tone,
        mark,
        headline,
        detail_tooltip: detail_tooltip.unwrap_or_else(|| detail.clone()),
        detail,
        button,
        status_bar,
    }
}

impl DockView {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("the dock view serializes")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::control::{ControlError, Named, StartFailed, Started};
    use crate::dock::{Canvas, DockSettings, FIX_OUTPUT_SETTING, FIX_RESTART_OBS, FIX_VIDEO};
    use crate::dock_config::ConfigError;
    use crate::live::Rates;

    fn named(id: &str, name: &str) -> Named {
        Named {
            id: id.into(),
            name: name.into(),
        }
    }

    fn canvas() -> Canvas {
        Canvas {
            fps_num: 30,
            fps_den: 1,
            output_width: 1280,
            output_height: 720,
        }
    }

    fn state(settings: DockSettings) -> ConfigState {
        let mut s = ConfigState::default();
        s.model = DockModel::new(settings);
        s
    }

    fn picked() -> ConfigState {
        let mut s = state(DockSettings {
            guild_id: Some("g".into()),
            channel_id: Some("c".into()),
            ..DockSettings::default()
        });
        s.model.guilds_loaded(Ok(vec![named("g", "Friends")]));
        s.model
            .channels_loaded("g", Ok(vec![named("c", "Kids Lounge 🧃")]));
        s
    }

    fn live() -> ConfigState {
        let mut s = picked();
        s.model.start(canvas()).unwrap();
        s.model.claim(None);
        s.model.started(Ok(Started {
            ownership: None,
            channel: "c".into(),
        }));
        s
    }

    fn view(s: &ConfigState) -> DockView {
        dock_view(s, true, None, None)
    }

    #[test]
    fn a_dock_without_a_channel_asks_for_the_settings() {
        let v = view(&ConfigState::default());
        assert_eq!(v.tone, Tone::Idle);
        assert_eq!(v.headline, "Not set up");
        assert_eq!(v.detail, "Pick a server and voice channel");
        assert_eq!(v.button, button(START, Action::Start, false));
        assert_eq!(v.status_bar, "D-Streamy idle");
        assert_eq!(v.mark, "●");
    }

    #[test]
    fn a_mic_on_the_unmanaged_track_holds_start_back_with_its_fix() {
        let guard = MicGuard {
            track: 6,
            mics: vec!["Mic/Aux".into()],
            live: false,
        };
        let v = dock_view(&picked(), true, None, Some(&guard));
        assert_eq!(v.tone, Tone::Attention);
        assert_eq!(v.headline, "Discord would hear Mic/Aux");
        assert_eq!(v.detail, "Manage Track 6");
        assert_eq!(v.detail_tooltip, guard.tooltip());
        assert_eq!(v.button, button(START, Action::Start, false));
        assert_eq!(v.status_bar, "⚠ D-Streamy");
        let live = dock_view(&live(), true, None, Some(&guard));
        assert_eq!(
            live.tone,
            Tone::Live,
            "a running session is never held back"
        );
        assert!(live.button.enabled);
    }

    #[test]
    fn a_mic_discord_hears_while_live_warns_and_keeps_streaming() {
        let leak = MicGuard {
            track: 6,
            mics: vec!["Mic/Aux".into()],
            live: true,
        };
        let v = dock_view(&live(), true, Some(&stats(true)), Some(&leak));
        assert_eq!(v.tone, Tone::Attention);
        assert_eq!(v.headline, "Discord hears Mic/Aux");
        assert_eq!(v.detail, "Manage Track 6");
        assert!(
            v.detail_tooltip.starts_with(&leak.message()),
            "{}",
            v.detail_tooltip
        );
        assert_eq!(
            v.button,
            button(STOP, Action::Stop, true),
            "still streaming"
        );
        assert_eq!((v.mark, v.status_bar.as_str()), ("⚠", "⚠ D-Streamy live"));
        let fine = dock_view(&live(), true, Some(&stats(true)), None);
        assert_eq!(fine.tone, Tone::Live, "clears once resolved");
    }

    #[test]
    fn an_idle_dock_with_a_channel_can_start() {
        let v = view(&picked());
        assert_eq!(v.tone, Tone::Idle);
        assert_eq!(v.headline, "Idle · Kids Lounge 🧃");
        assert_eq!(v.detail, "Track 6");
        assert_eq!(v.button, button(START, Action::Start, true));

        let mut s = state(DockSettings {
            guild_id: Some("g".into()),
            channel_id: Some("c".into()),
            audio_track: 3,
            ..DockSettings::default()
        });
        assert_eq!(
            view(&s).headline,
            "Idle",
            "the channel's name isn't loaded yet"
        );
        assert_eq!(view(&s).detail, "Track 3");
        assert!(view(&s).button.enabled, "a saved channel starts unlisted");
        s.model.select_audio_track(5);
        assert_eq!(view(&s).detail, "Track 5");
    }

    #[test]
    fn starting_and_stopping_are_busy_with_the_button_disabled() {
        let mut s = picked();
        s.model.select_audio_track(3);
        s.model.start(canvas()).unwrap();
        let v = view(&s);
        assert_eq!(v.tone, Tone::Busy);
        assert_eq!(v.headline, "Starting…");
        assert_eq!(v.detail, "Track 3");
        assert_eq!(v.button, button(START, Action::Start, false));
        assert_eq!(v.status_bar, "D-Streamy starting…");

        let mut s = live();
        s.model.stop();
        let v = view(&s);
        assert_eq!(v.tone, Tone::Busy);
        assert_eq!(v.headline, "Stopping…");
        assert_eq!(v.detail, "Track 6");
        assert_eq!(v.button, button(STOP, Action::Stop, false));
        assert_eq!(v.status_bar, "D-Streamy stopping…");
    }

    #[test]
    fn live_names_the_channel_and_offers_stop() {
        let mut s = live();
        let v = view(&s);
        assert_eq!(v.tone, Tone::Live);
        assert_eq!(v.headline, "Live · Kids Lounge 🧃");
        assert_eq!(v.detail, "Track 6");
        assert_eq!(v.button, button(STOP, Action::Stop, true));
        assert_eq!(v.status_bar, "● D-Streamy live");

        s.model.select_audio_track(4);
        assert_eq!(view(&s).detail, "Track 6 · next: Track 4");
        assert_eq!(
            view(&s).detail_tooltip,
            "Track 6 · Discord hears Track 4 from the next Go Live"
        );
    }

    #[test]
    fn a_service_error_needs_attention_with_its_fix() {
        let mut s = picked();
        s.model.start(canvas()).unwrap();
        let error = ControlError::NotRunning("no service is listening at /run/d.sock".into());
        s.model.started(Err(StartFailed::from(error.clone())));
        let v = view(&s);
        assert_eq!(v.tone, Tone::Attention);
        assert_eq!(v.headline, "Service not running");
        #[cfg(target_os = "linux")]
        assert_eq!(v.detail, "Run `dstreamy service install`");
        #[cfg(not(target_os = "linux"))]
        assert_eq!(v.detail, "Run `dstreamy serve`");
        assert_eq!(v.detail_tooltip, error.to_string());
        assert_eq!(
            v.detail_tooltip,
            s.model.status_text(true),
            "the same text as the dialog"
        );
        assert_eq!(v.button, button(START, Action::Start, true));
        assert_eq!(v.status_bar, "⚠ D-Streamy");
        assert_eq!(v.mark, "⚠");
    }

    #[test]
    fn every_control_error_has_a_short_headline() {
        let errors = [
            ControlError::NotRunning("x".into()),
            ControlError::Connect("x".into()),
            ControlError::NotLoggedIn { window: false },
            ControlError::NotLoggedIn { window: true },
            ControlError::InvalidToken { window: false },
            ControlError::InvalidToken { window: true },
            ControlError::LoginCancelled,
            ControlError::LoginFailed("x".into()),
            ControlError::LoginRunning,
            ControlError::LoginUnavailable,
            ControlError::UnknownAccount("saved: a, b".into()),
            ControlError::Streaming("stop first".into()),
            ControlError::UnknownGuild,
            ControlError::UnknownChannel,
            ControlError::GuildUnavailable,
            ControlError::Locked("pid 1".into()),
            ControlError::Busy {
                channel: "x".into(),
            },
            ControlError::OtherSource {
                channel: "x".into(),
            },
            ControlError::Outdated { feature: "x" },
            ControlError::Stopping,
            ControlError::StillStarting,
            ControlError::Restarted,
            ControlError::NoPlace,
            ControlError::Refused {
                code: "x".into(),
                message: "y".into(),
            },
            ControlError::Timeout {
                command: "start",
                secs: 10,
            },
            ControlError::Abandoned,
            ControlError::Protocol("x".into()),
        ];
        for error in errors {
            let mut s = ConfigState::default();
            s.model.guilds_loaded(Err(error.clone()));
            let v = view(&s);
            assert_eq!(v.tone, Tone::Attention, "{error:?}");
            assert_eq!(v.headline, error.headline());
            assert!(
                !v.headline.is_empty() && v.headline.len() <= 28,
                "{error:?}"
            );
            assert_eq!(v.detail, error.fix());
            assert_eq!(v.detail_tooltip, error.to_string());
            assert_short_fix(&v);
        }
        assert_eq!(
            ControlError::NotLoggedIn { window: false }.fix(),
            "Run `dstreamy login`"
        );
        assert_eq!(
            ControlError::InvalidToken { window: true }.fix(),
            "Press Log in again"
        );
        assert_eq!(
            ControlError::UnknownChannel.fix(),
            "Pick the voice channel again"
        );
    }

    /// An attention detail is a short fix that never repeats its headline; the tooltip has the whole message.
    fn assert_short_fix(v: &DockView) {
        assert_eq!(v.tone, Tone::Attention, "{v:?}");
        let (detail, headline) = (v.detail.to_lowercase(), v.headline.to_lowercase());
        assert!(
            !detail.starts_with(&headline) && !detail.contains(&headline),
            "{v:?}"
        );
        assert!(v.detail.chars().count() <= 35, "{v:?}");
        assert!(!detail.contains("fix it"), "a concrete action: {v:?}");
        assert!(!v.detail.is_empty() && !v.detail.contains('\n'), "{v:?}");
        assert!(!v.detail_tooltip.is_empty(), "{v:?}");
    }

    #[test]
    fn every_attention_state_shows_a_short_fix() {
        let mut views = Vec::new();
        let mut s = picked();
        s.model.start(canvas()).unwrap();
        s.model
            .output_failed("no encoder".into(), FIX_OUTPUT_SETTING, true);
        views.push(view(&s));
        let mut s = live();
        s.model.stop();
        s.model.stopped(Err("timed out".into()));
        views.push(view(&s));
        let mut s = live();
        s.model.ended(Some("the session ended".into()));
        views.push(view(&s));
        let mut s = picked();
        s.model.abort("OBS has no video running".into(), FIX_VIDEO);
        views.push(view(&s));
        let mut s = state(DockSettings {
            guild_id: Some("g".into()),
            ..DockSettings::default()
        });
        let _ = s.model.start(canvas());
        views.push(view(&s));
        for error in [
            ConfigError::NoFolder,
            ConfigError::Failed("disk full".into()),
            ConfigError::Corrupt {
                path: "/c.json".into(),
                detail: "x".into(),
            },
            ConfigError::Unreadable {
                path: "/c.json".into(),
                detail: "x".into(),
            },
            ConfigError::Symlink {
                path: "/c.json".into(),
            },
        ] {
            let mut s = picked();
            s.open(|| Err(error.clone()), |_| Ok(()));
            views.push(view(&s));
        }
        views.push(dock_view(&picked(), false, None, None));
        for v in &views {
            assert_short_fix(v);
        }
        assert_eq!(views[4].headline, "No voice channel picked");
        assert_eq!(views[4].detail, "Pick a server and voice channel");
    }

    #[test]
    fn plugin_side_failures_get_their_own_headlines() {
        let mut s = picked();
        s.model.start(canvas()).unwrap();
        s.model
            .output_failed("no encoder".into(), FIX_OUTPUT_SETTING, true);
        assert_eq!(
            (view(&s).headline.as_str(), view(&s).detail_tooltip.as_str()),
            ("Didn't start", "no encoder")
        );
        assert_eq!(view(&s).detail, "Change the OBS setting it names");

        let mut s = live();
        s.model.stop();
        s.model.stopped(Err("timed out".into()));
        assert_eq!(view(&s).headline, "Stop didn't finish");
        assert_eq!(view(&s).detail, "Press Start, then Stop");
        assert_eq!(view(&s).detail_tooltip, "timed out");

        let mut s = live();
        s.model.ended(Some("the session ended".into()));
        assert_eq!(view(&s).headline, "Stream stopped");
        let mut s = live();
        s.model.ended(None);
        assert_eq!(view(&s).tone, Tone::Idle, "a clean end needs nothing");

        let mut s = picked();
        s.model.abort("OBS has no video running".into(), FIX_VIDEO);
        assert_eq!(view(&s).headline, "Can't start");
        let mut s = picked();
        s.model.start(canvas()).unwrap();
        s.model.abort("gone".into(), FIX_RESTART_OBS);
        assert_eq!(view(&s).headline, "Didn't start");
        assert_eq!(view(&s).detail, "Restart OBS");
        assert_eq!(view(&s).detail_tooltip, "gone");
        let mut s = live();
        s.model.stop();
        s.model.abort("gone".into(), FIX_RESTART_OBS);
        assert_eq!(view(&s).headline, "Stop didn't finish");

        let mut s = picked();
        let err = s
            .model
            .start(Canvas {
                fps_num: 24,
                ..canvas()
            })
            .unwrap_err();
        assert_eq!(view(&s).headline, "Can't start");
        assert_eq!(view(&s).detail, "Check OBS's Settings › Video");
        assert_eq!(view(&s).detail_tooltip, err);
    }

    #[test]
    fn a_settings_file_problem_shows_like_the_dialog() {
        let mut s = picked();
        s.open(
            || {
                Err(ConfigError::Symlink {
                    path: "/c.json".into(),
                })
            },
            |_| Ok(()),
        );
        let config = s.error().unwrap().message(6);
        let v = view(&s);
        assert_eq!(v.tone, Tone::Attention);
        assert_eq!(v.headline, "Settings file is a link");
        assert_eq!(v.detail, "Replace the link with a file");
        assert_eq!(v.detail_tooltip, config);
        assert!(v.button.enabled);

        s.model.start(canvas()).unwrap();
        s.model
            .started(Err(ControlError::NotLoggedIn { window: false }.into()));
        let v = view(&s);
        assert_eq!(v.headline, "Not logged in");
        assert_eq!(v.detail, "Run `dstreamy login`");
        assert_eq!(
            v.detail_tooltip,
            format!("Log in first: run `dstreamy login`. {config}")
        );

        let mut s = live();
        s.open(|| Err(ConfigError::NoFolder), |_| Ok(()));
        let v = view(&s);
        assert_eq!(v.tone, Tone::Live, "still live");
        assert_eq!(v.detail, "Track 6 · Settings not saved");
        assert_eq!(
            v.detail_tooltip,
            format!("Track 6 · {}", s.error().unwrap().message(6))
        );
    }

    #[test]
    fn without_controls_nothing_can_start() {
        let v = dock_view(&picked(), false, None, None);
        assert_eq!(v.tone, Tone::Attention);
        assert_eq!(v.detail, "Start and Stop are disabled");
        assert_eq!(v.detail_tooltip, UNSUPPORTED);
        assert!(!v.button.enabled);
    }

    #[test]
    fn the_json_names_every_field_for_the_ui() {
        let v: Value = serde_json::from_str(&view(&live()).to_json()).unwrap();
        assert_eq!(
            v,
            json!({
                "tone": "live",
                "mark": "●",
                "headline": "Live · Kids Lounge 🧃",
                "detail": "Track 6",
                "detail_tooltip": "Track 6",
                "button": {"label": "Stop D-Streamy", "action": "stop", "enabled": true},
                "status_bar": "● D-Streamy live",
            })
        );
        let v: Value = serde_json::from_str(&view(&picked()).to_json()).unwrap();
        assert_eq!(v["tone"], "idle");
        assert_eq!(v["button"]["action"], "start");
    }

    fn stats(rates: bool) -> LiveStats {
        LiveStats {
            uptime_ms: 754_000,
            rates: rates.then_some(Rates {
                bits_per_sec: 6_000_000.0,
                fps: 60.0,
            }),
            ..LiveStats::default()
        }
    }

    #[test]
    fn live_shows_stats_with_the_track_in_the_tooltip() {
        let mut s = live();
        s.model.select_audio_track(3);
        let v = dock_view(&s, true, Some(&stats(true)), None);
        assert_eq!(v.tone, Tone::Live);
        assert_eq!(v.detail, "6.0 Mbit/s · 60 fps · 12:34");
        assert_eq!(
            v.detail_tooltip,
            "Missed frames: 0\nAudio dropouts: unknown\nReconnects: unknown\nViewers: unknown\n\
             Duration: 12:34\nVideo: 6.0 Mbit/s, 60 fps\n\
             Track 6 · Discord hears Track 3 from the next Go Live"
        );
        let early = dock_view(&s, true, Some(&stats(false)), None);
        assert_eq!(
            early.detail, "12:34",
            "only the uptime before a delta exists"
        );
        assert_eq!(
            dock_view(&s, true, None, None).detail,
            "Track 6 · next: Track 3"
        );
    }

    #[test]
    fn stats_only_replace_the_live_line() {
        for s in [picked(), ConfigState::default()] {
            let v = dock_view(&s, true, Some(&stats(true)), None);
            assert_eq!(v, view(&s), "idle ignores stats");
            assert_eq!(v.detail, v.detail_tooltip);
        }
        let mut s = live();
        s.model.stop();
        assert_eq!(
            dock_view(&s, true, Some(&stats(true)), None).detail,
            "Track 6"
        );
    }
}
