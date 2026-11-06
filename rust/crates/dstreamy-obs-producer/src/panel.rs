//! The status-bar item and its popup panel: everything they show, from the same model as the dock view.

use serde::Serialize;

use crate::control::{ADD_ACCOUNT, Named};
use crate::dock::{
    AUDIO_TRACKS, DockModel, DockState, LIVE_NO_LOGOUT, LIVE_NO_SWITCH, LOGIN_WAITS,
    SWITCHING_ACCOUNT, track_name,
};
use crate::dock_config::ConfigState;
use crate::dock_view::{Button, Tone, dock_view};
use crate::live::LiveStats;
use crate::mixer::{
    AudioSource, AudioView, CONFIRM, NOT_NOW, audio_view, instead_label, mic_guard, notices,
    release_label,
};
use crate::status_bar::{StatusBar, status_bar};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    pub id: String,
    pub name: String,
    pub selected: bool,
}

/// A combo box: its first entry (id "") is the placeholder, and picking it clears the choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Choices {
    pub placeholder: String,
    pub items: Vec<Item>,
    pub loading: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrackItem {
    pub track: u8,
    pub name: String,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Confirm {
    pub text: String,
    pub ok: String,
    pub cancel: String,
    /// A track to pick instead of this takeover.
    pub instead: Option<TrackAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrackAction {
    pub track: u8,
    pub label: String,
}

/// A warning shown above the audio part whatever the Discord track is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Notice {
    pub text: String,
    pub release: Option<TrackAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Source {
    pub uuid: String,
    pub name: String,
    pub on: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Audio {
    pub title: String,
    pub track: u8,
    pub tracks: Vec<TrackItem>,
    pub managed: bool,
    /// What Discord hears from the track shown: above the takeover, or above the checkboxes.
    pub line: String,
    pub manage: String,
    pub confirm: Option<Confirm>,
    pub sources: Vec<Source>,
    pub empty: Option<String>,
    pub notices: Vec<Notice>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginAction {
    Login,
    Cancel,
}

/// Opens serve's login window, or closes the one it waits for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Login {
    pub label: &'static str,
    pub action: LoginAction,
}

pub const CANCEL_LOGIN: &str = "Cancel login";
pub const ACCOUNT_LABEL: &str = "Account";
pub const LOG_OUT: &str = "Log out";
pub const NOT_AVAILABLE: &str = "Not available";

/// Log out's confirm question for `name`.
pub fn logout_question(name: &str) -> String {
    format!("Log out {name}?")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccountItem {
    pub name: String,
    /// The Discord user's display name, empty while unknown.
    pub user: String,
    pub selected: bool,
}

/// The account row: a picker of serve's saved accounts ending with "Add account…", and Log out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccountRow {
    pub label: &'static str,
    pub items: Vec<AccountItem>,
    pub add: &'static str,
    pub logout: &'static str,
    pub enabled: bool,
    /// Why the picker is disabled, empty while enabled.
    pub tooltip: &'static str,
    pub logout_tooltip: &'static str,
    /// Log out's confirm row while it waits for an answer.
    pub confirm: Option<Confirm>,
}

/// The row, shown once serve lists at least one account; with none the Log in button takes its place.
pub fn account_row(m: &DockModel) -> Option<AccountRow> {
    let accounts = m.accounts().filter(|a| !a.accounts.is_empty())?;
    let enabled = m.accounts_editable();
    let live = m.state() != &DockState::Idle;
    let (tooltip, logout_tooltip) = match () {
        _ if enabled => ("", ""),
        _ if live => (LIVE_NO_SWITCH, LIVE_NO_LOGOUT),
        _ if m.account_changing() => (SWITCHING_ACCOUNT, SWITCHING_ACCOUNT),
        _ if m.logging_in() => (LOGIN_WAITS, LOGIN_WAITS),
        _ => ("", ""),
    };
    let shown = m.pending_account().or(accounts.active.as_deref());
    Some(AccountRow {
        label: ACCOUNT_LABEL,
        items: accounts
            .accounts
            .iter()
            .map(|a| AccountItem {
                name: a.name.clone(),
                user: a
                    .global_name
                    .clone()
                    .or_else(|| a.username.clone())
                    .unwrap_or_default(),
                selected: shown == Some(a.name.as_str()),
            })
            .collect(),
        add: ADD_ACCOUNT,
        logout: LOG_OUT,
        enabled,
        tooltip,
        logout_tooltip,
        confirm: m.confirming_logout().map(|name| Confirm {
            text: logout_question(name),
            ok: LOG_OUT.to_owned(),
            cancel: NOT_NOW.to_owned(),
            instead: None,
        }),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PanelView {
    pub tone: Tone,
    pub mark: &'static str,
    pub status: StatusBar,
    pub state_line: String,
    /// The fix for a problem, empty otherwise.
    pub state_fix: String,
    pub state_tooltip: String,
    pub guilds: Choices,
    pub channels: Choices,
    pub audio: Audio,
    pub button: Button,
    pub login: Option<Login>,
    pub account: Option<AccountRow>,
}

pub const LOADING: &str = "Loading…";
pub const NOT_SET_UP: &str = "Not set up";
pub const NOT_SET_UP_FIX: &str = "Pick a server and voice channel below";
pub const NO_AUDIO_CONTROLS: &str = "D-Streamy can't change Discord audio on Windows yet.";

fn choices(
    list: &[Named],
    selected: Option<&str>,
    placeholder: &str,
    loading: bool,
    enabled: bool,
    unavailable: bool,
) -> Choices {
    let mut items: Vec<Item> = list
        .iter()
        .map(|n| Item {
            id: n.id.clone(),
            name: n.name.clone(),
            selected: Some(n.id.as_str()) == selected,
        })
        .collect();
    if let Some(id) = selected.filter(|id| list.iter().all(|n| n.id != *id)) {
        items.push(Item {
            id: id.to_owned(),
            name: if unavailable {
                NOT_AVAILABLE.to_owned()
            } else {
                id.to_owned()
            },
            selected: true,
        });
    }
    Choices {
        placeholder: if loading && list.is_empty() {
            LOADING.to_owned()
        } else {
            placeholder.to_owned()
        },
        items,
        loading,
        enabled,
    }
}

/// The audio part shows the track Discord hears now while live, else the one the next Go Live sends.
fn audio(state: &ConfigState, controls: bool, sources: &[AudioSource], confirming: bool) -> Audio {
    let m = &state.model;
    let selected = m.settings().audio_track;
    let track = m.heard_track();
    let tracks = AUDIO_TRACKS
        .map(|n| TrackItem {
            track: n,
            name: track_name(n),
            selected: n == selected,
        })
        .collect();
    let mut audio = Audio {
        title: "Discord audio".to_owned(),
        track,
        tracks,
        managed: false,
        line: String::new(),
        manage: String::new(),
        confirm: None,
        sources: Vec::new(),
        empty: None,
        notices: Vec::new(),
    };
    if !controls {
        audio.line = NO_AUDIO_CONTROLS.to_owned();
        return audio;
    }
    audio.notices = notices(m, sources)
        .into_iter()
        .map(|n| Notice {
            text: n.text,
            release: n.release.map(|track| TrackAction {
                track,
                label: release_label(track),
            }),
        })
        .collect();
    match audio_view(m, track, sources) {
        AudioView::Unmanaged {
            help,
            take_over,
            effect,
            instead,
            ..
        } => {
            audio.line = help;
            audio.manage = take_over;
            audio.confirm = confirming.then(|| Confirm {
                text: effect,
                ok: CONFIRM.to_owned(),
                cancel: NOT_NOW.to_owned(),
                instead: instead.map(|track| TrackAction {
                    track,
                    label: instead_label(track),
                }),
            });
        }
        AudioView::Managed {
            help,
            sources,
            empty,
            ..
        } => {
            audio.managed = true;
            audio.line = help;
            audio.sources = sources
                .into_iter()
                .map(|s| Source {
                    uuid: s.uuid,
                    name: s.name,
                    on: s.on,
                })
                .collect();
            audio.empty = empty;
        }
    }
    audio
}

/// `stats` with what the session adds: its frame rate, and whether a source feeds the Discord track.
fn with_session(
    m: &DockModel,
    controls: bool,
    sources: &[AudioSource],
    stats: &LiveStats,
) -> LiveStats {
    let target_fps = match m.state() {
        DockState::Live { session_fps, .. } => *session_fps,
        _ => None,
    };
    let bit = 1u32 << (m.heard_track() - 1);
    LiveStats {
        target_fps,
        audio_fed: controls.then(|| sources.iter().any(|s| s.mixers & bit != 0)),
        ..*stats
    }
}

/// The service's frame gaps and audio rate under the live line, empty without a service report.
fn gaps_line(stats: &LiveStats) -> String {
    let Some(s) = stats.service else {
        return String::new();
    };
    let mut parts = Vec::new();
    if let (Some(p95), Some(max)) = (s.jitter_p95_ms, s.jitter_max_ms) {
        parts.push(format!("Frame gaps {p95} ms p95, {max} ms max"));
    }
    if let Some(n) = s.audio_per_sec {
        parts.push(format!("audio {n}/s"));
    }
    parts.join(" · ")
}

/// The status-bar item and panel; `confirming` says the takeover's confirm row is open.
pub fn panel_view(
    state: &ConfigState,
    controls: bool,
    live: Option<&LiveStats>,
    sources: &[AudioSource],
    confirming: bool,
) -> PanelView {
    let m = &state.model;
    let settings = m.settings();
    let idle = m.state() == &DockState::Idle;
    let mic = controls.then(|| mic_guard(m, sources)).flatten();
    let live = live.map(|stats| with_session(m, controls, sources, stats));
    let live = live.as_ref();
    let dock = dock_view(state, controls, live, mic.as_ref());
    let not_set_up =
        dock.tone == Tone::Idle && (settings.guild_id.is_none() || settings.channel_id.is_none());
    let (state_line, state_fix, state_tooltip) = match dock.tone {
        _ if not_set_up => (
            NOT_SET_UP.to_owned(),
            NOT_SET_UP_FIX.to_owned(),
            format!("{NOT_SET_UP}\n{NOT_SET_UP_FIX}"),
        ),
        Tone::Attention => (
            dock.headline.clone(),
            dock.detail.clone(),
            dock.detail_tooltip.clone(),
        ),
        Tone::Busy if m.logging_in() => (
            dock.headline.clone(),
            dock.detail.clone(),
            format!("{}\n{}", dock.headline, dock.detail_tooltip),
        ),
        Tone::Live => (
            format!("Live · {}", dock.detail),
            live.map(gaps_line).unwrap_or_default(),
            format!("{}\n{}", dock.headline, dock.detail_tooltip),
        ),
        Tone::Idle | Tone::Busy => (
            dock.headline.clone(),
            String::new(),
            format!("{}\n{}", dock.headline, dock.detail_tooltip),
        ),
    };
    let (loading_guilds, loading_channels) = m.loading();
    let offer = controls.then(|| m.login_offer()).flatten();
    let login = match offer {
        Some(offer) => Some(Login {
            label: offer.label(),
            action: LoginAction::Login,
        }),
        None if controls && m.logging_in() => Some(Login {
            label: CANCEL_LOGIN,
            action: LoginAction::Cancel,
        }),
        None => None,
    };
    let state_fix = if offer.is_some() && m.problem().is_some_and(|p| p.login_fix) {
        String::new()
    } else {
        state_fix
    };
    PanelView {
        tone: dock.tone,
        mark: dock.mark,
        status: status_bar(
            dock.tone,
            live,
            if not_set_up {
                state_tooltip.clone()
            } else {
                format!("{}\n{}", dock.headline, dock.detail_tooltip)
            },
        ),
        state_line,
        state_fix,
        state_tooltip,
        guilds: choices(
            m.guilds(),
            settings.guild_id.as_deref(),
            "Pick a server",
            loading_guilds,
            idle,
            m.guild_unavailable(),
        ),
        channels: choices(
            m.channels(),
            settings.channel_id.as_deref(),
            "Pick a voice channel",
            loading_channels,
            idle && settings.guild_id.is_some(),
            m.channel_unavailable(),
        ),
        audio: audio(state, controls, sources, confirming),
        button: dock.button,
        login,
        account: controls.then(|| account_row(m)).flatten(),
    }
}

impl PanelView {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("the panel view serializes")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::control::{Account, Accounts, ControlError, Started};
    use crate::dock::{Canvas, DockModel, DockSettings};
    use crate::live::{Rates, ServiceHealth};
    use crate::status_bar::State;

    fn named(id: &str, name: &str) -> Named {
        Named {
            id: id.into(),
            name: name.into(),
        }
    }

    fn state(settings: DockSettings) -> ConfigState {
        let mut s = ConfigState::default();
        s.model = DockModel::new(settings);
        s
    }

    fn picked(managed: bool) -> ConfigState {
        let mut s = state(DockSettings {
            guild_id: Some("g".into()),
            channel_id: Some("c".into()),
            audio_track: 6,
            ..DockSettings::default()
        });
        if managed {
            s.model.take_over(6);
        }
        s.model.guilds_loaded(Ok(vec![
            named("g", "Beste Chatgruppe"),
            named("h", "Other"),
        ]));
        s.model
            .channels_loaded("g", Ok(vec![named("c", "Kids Lounge 🧃")]));
        s
    }

    fn sources() -> Vec<AudioSource> {
        ["Alerts", "Mic", "Music", "Music-Out", "Stream"]
            .iter()
            .enumerate()
            .map(|(i, name)| AudioSource {
                uuid: format!("u{i}"),
                name: (*name).into(),
                mixers: if *name == "Music" { 0b10_0000 } else { 0b1 },
                mic: false,
            })
            .collect()
    }

    fn view(s: &ConfigState) -> PanelView {
        panel_view(s, true, None, &sources(), false)
    }

    #[test]
    fn lists_mark_the_saved_choice_and_keep_an_unlisted_one() {
        let v = view(&picked(true));
        assert_eq!(v.guilds.placeholder, "Pick a server");
        assert_eq!(
            v.guilds.items,
            [
                Item {
                    id: "g".into(),
                    name: "Beste Chatgruppe".into(),
                    selected: true
                },
                Item {
                    id: "h".into(),
                    name: "Other".into(),
                    selected: false
                },
            ]
        );
        assert_eq!(v.channels.items[0].name, "Kids Lounge 🧃");
        assert!(v.guilds.enabled && v.channels.enabled);

        let s = state(DockSettings {
            guild_id: Some("saved".into()),
            ..DockSettings::default()
        });
        let v = view(&s);
        assert_eq!(
            v.guilds.items,
            [Item {
                id: "saved".into(),
                name: "saved".into(),
                selected: true
            }]
        );
        assert!(v.channels.items.is_empty());
        assert!(v.channels.enabled, "a server is picked");
        assert!(!view(&ConfigState::default()).channels.enabled);
    }

    #[test]
    fn loading_shows_in_the_placeholder_until_the_list_arrives() {
        let mut s = state(DockSettings {
            guild_id: Some("g".into()),
            ..DockSettings::default()
        });
        s.model.lists_requested();
        let v = view(&s);
        assert!(v.guilds.loading && v.channels.loading);
        assert_eq!(v.guilds.placeholder, LOADING);
        assert_eq!(v.channels.placeholder, LOADING);
        s.model.guilds_loaded(Ok(vec![named("g", "G")]));
        let v = view(&s);
        assert!(v.guilds.loading, "until the job ends");
        assert_eq!(
            v.guilds.placeholder, "Pick a server",
            "a list that arrived is shown"
        );
        s.model.lists_finished();
        let v = view(&s);
        assert!(!v.guilds.loading && !v.channels.loading);

        let mut s = ConfigState::default();
        s.model.lists_requested();
        assert!(view(&s).guilds.loading);
        assert!(!view(&s).channels.loading, "no server, no channels to load");
    }

    #[test]
    fn loading_lasts_until_the_last_queued_job_ends() {
        let mut s = picked(true);
        s.model.lists_requested();
        s.model.lists_requested();
        s.model.lists_finished();
        let v = view(&s);
        assert!(
            v.guilds.loading && v.channels.loading,
            "one job is still queued"
        );
        assert_eq!(
            v.channels.placeholder, "Pick a voice channel",
            "the old list stays while loading"
        );
        s.model.lists_finished();
        assert!(!view(&s).guilds.loading);
        s.model.lists_finished();
        assert!(
            !view(&s).guilds.loading,
            "an extra end never goes below zero"
        );
        s.model.lists_requested();
        assert!(view(&s).guilds.loading);
    }

    #[test]
    fn not_set_up_points_to_the_lists_below() {
        for s in [
            ConfigState::default(),
            state(DockSettings {
                guild_id: Some("g".into()),
                ..DockSettings::default()
            }),
        ] {
            let v = view(&s);
            assert_eq!(v.tone, Tone::Idle);
            assert_eq!(v.state_line, "Not set up");
            assert_eq!(v.state_fix, "Pick a server and voice channel below");
            assert_eq!(
                v.status.tooltip,
                "Not set up\nPick a server and voice channel below"
            );
            assert_eq!(v.state_tooltip, v.status.tooltip);
            assert_eq!(v.status.state, State::Idle);
            assert!(!v.state_fix.contains('⚙'), "the panel has no gear");
            assert!(!v.button.enabled);
        }
        assert_eq!(view(&picked(true)).state_fix, "", "set up");
    }

    #[test]
    fn a_failed_load_ends_loading_and_shows_the_problem() {
        let mut s = ConfigState::default();
        s.model.lists_requested();
        s.model
            .guilds_loaded(Err(ControlError::NotLoggedIn { window: false }));
        s.model.lists_finished();
        let v = view(&s);
        assert!(!v.guilds.loading);
        assert_eq!(v.tone, Tone::Attention);
        assert_eq!(v.state_line, "Not logged in");
        assert_eq!(v.state_fix, "Run `dstreamy login`");
        assert_eq!(v.state_tooltip, "Log in first: run `dstreamy login`");
        assert_eq!(v.status.state, State::Error);
        assert_eq!(v.login, None, "this serve can't open a login window");
    }

    #[test]
    fn a_login_button_replaces_the_command_while_serve_can_open_the_window() {
        let mut s = ConfigState::default();
        s.model
            .guilds_loaded(Err(ControlError::NotLoggedIn { window: true }));
        let v = view(&s);
        assert_eq!(v.state_line, "Not logged in");
        assert_eq!(v.state_fix, "", "the button takes the fix line's place");
        assert_eq!(v.state_tooltip, "Log in first: press Log in");
        assert_eq!(v.login, Some(log_in("Log in")));
        assert_eq!(panel_view(&s, false, None, &[], false).login, None);

        s.model
            .guilds_loaded(Err(ControlError::InvalidToken { window: true }));
        let v = view(&s);
        assert_eq!(v.state_line, "Discord login expired");
        assert_eq!(v.login, Some(log_in("Log in again")));

        assert!(s.model.begin_login());
        let v = view(&s);
        assert_eq!(v.tone, Tone::Busy);
        assert_eq!(v.state_line, "Logging in…");
        assert_eq!(
            v.login,
            Some(Login {
                label: CANCEL_LOGIN,
                action: LoginAction::Cancel
            }),
            "the wait can be ended from OBS"
        );
        assert_eq!(v.status.state, State::Busy);
        assert!(!v.button.enabled);
        assert_eq!(v.state_fix, "Waiting for login in the browser window…");

        s.model.login_finished(Err(ControlError::LoginCancelled));
        let v = view(&s);
        assert_eq!(v.state_line, "Login cancelled");
        assert_eq!(v.login, Some(log_in("Log in again")));

        s.model.begin_login();
        s.model.login_finished(Ok(Some("Mock".into())));
        let v = view(&s);
        assert_eq!(v.tone, Tone::Idle);
        assert_eq!(v.state_line, "Not set up");
        assert_eq!(v.login, None);
    }

    fn log_in(label: &'static str) -> Login {
        Login {
            label,
            action: LoginAction::Login,
        }
    }

    #[test]
    fn a_missing_login_comes_before_a_mic_on_the_unmanaged_track() {
        let mut s = picked(false);
        let mut with_mic = sources();
        with_mic[1].mic = true;
        with_mic[1].mixers = 0b10_0000;
        let shown = |s: &ConfigState| panel_view(s, true, None, &with_mic, false);
        assert_eq!(shown(&s).state_line, "Discord would hear Mic");
        s.model
            .guilds_loaded(Err(ControlError::NotLoggedIn { window: true }));
        let v = shown(&s);
        assert_eq!(v.state_line, "Not logged in");
        assert_eq!(v.state_fix, "");
        assert_eq!(v.login, Some(log_in("Log in")));
        assert!(!v.button.enabled, "the mic still holds Go Live back");
        s.model.begin_login();
        assert_eq!(
            shown(&s).state_fix,
            "Waiting for login in the browser window…"
        );
        s.model.login_finished(Ok(None));
        let v = shown(&s);
        assert_eq!(
            v.state_line, "Discord would hear Mic",
            "the mic once logged in"
        );
        assert_eq!(v.state_fix, "Manage Track 6");
        assert_eq!(v.login, None);
    }

    #[test]
    fn a_fix_the_button_doesn_t_cover_stays_next_to_it() {
        let mut s = ConfigState::default();
        s.model
            .guilds_loaded(Err(ControlError::InvalidToken { window: true }));
        s.model.begin_login();
        s.model.login_finished(Err(ControlError::Timeout {
            command: "login",
            secs: 630,
        }));
        let v = view(&s);
        assert_eq!(v.state_fix, "Check the service, then retry");
        assert_eq!(v.login, Some(log_in("Log in again")));
        s.model.begin_login();
        s.model.login_finished(Err(ControlError::LoginRunning));
        let v = view(&s);
        assert_eq!(v.state_fix, "Finish the login in its window");
        assert_eq!(v.login, Some(log_in("Log in again")), "never stuck");
        s.model.begin_login();
        s.model.login_finished(Err(ControlError::NotRunning(
            "it closed the connection before answering login".into(),
        )));
        let v = view(&s);
        assert_eq!(v.state_line, "Service not running");
        assert!(!v.state_fix.is_empty());
        assert_eq!(v.login, None);
    }

    #[test]
    fn an_unmanaged_track_offers_the_takeover_and_its_confirm_row() {
        let s = picked(false);
        let v = view(&s);
        assert!(!v.audio.managed);
        assert_eq!(v.audio.title, "Discord audio");
        assert_eq!(v.audio.track, 6);
        assert_eq!(
            v.audio.line,
            "Discord would hear every source on Track 6. Manage Track 6 to pick sources here."
        );
        assert_eq!(v.audio.manage, "Manage Track 6");
        assert_eq!(v.audio.confirm, None);
        assert!(v.audio.sources.is_empty(), "nothing to tick yet");

        let v = panel_view(&s, true, None, &sources(), true);
        assert_eq!(
            v.audio.confirm,
            Some(Confirm {
                text: "Turns Track 6 off on every audio source, also for recordings and streams that use it."
                    .into(),
                ok: "Manage".into(),
                cancel: "Not now".into(),
                instead: None,
            })
        );
        let mut on_1 = picked(false);
        on_1.model.select_audio_track(1);
        let v = panel_view(&on_1, true, None, &sources(), true);
        assert_eq!(
            v.audio.confirm.and_then(|c| c.instead),
            Some(TrackAction {
                track: 6,
                label: "Use Track 6 instead".into()
            })
        );
    }

    #[test]
    fn other_managed_tracks_show_as_warnings_with_stop_managing() {
        let mut s = picked(true);
        s.model.take_over(1);
        let v = view(&s);
        assert_eq!(v.audio.notices.len(), 1);
        assert_eq!(
            v.audio.notices[0].release,
            Some(TrackAction {
                track: 1,
                label: "Stop managing Track 1".into()
            })
        );
        assert!(
            v.audio
                .line
                .starts_with("Discord would hear the ticked sources on Track 6.")
        );
        assert!(view(&picked(true)).audio.notices.is_empty());
    }

    #[test]
    fn a_managed_track_lists_every_source_with_its_tick() {
        let v = panel_view(&picked(true), true, None, &sources(), true);
        assert!(v.audio.managed);
        assert_eq!(v.audio.confirm, None, "no confirm once managed");
        let shown: Vec<(&str, bool)> = v
            .audio
            .sources
            .iter()
            .map(|s| (s.name.as_str(), s.on))
            .collect();
        assert_eq!(
            shown,
            [
                ("Alerts", false),
                ("Mic", false),
                ("Music", true),
                ("Music-Out", false),
                ("Stream", false)
            ]
        );
        let tracks: Vec<(u8, bool)> = v
            .audio
            .tracks
            .iter()
            .map(|t| (t.track, t.selected))
            .collect();
        assert_eq!(
            tracks,
            [
                (1, false),
                (2, false),
                (3, false),
                (4, false),
                (5, false),
                (6, true)
            ]
        );
        let empty = panel_view(&picked(true), true, None, &[], false);
        assert!(empty.audio.empty.is_some());
    }

    fn mic_on_track_6() -> Vec<AudioSource> {
        let mut list = sources();
        list.push(AudioSource {
            uuid: "mic".into(),
            name: "Mic/Aux".into(),
            mixers: 0b11_1111,
            mic: true,
        });
        list
    }

    fn live(mut s: ConfigState) -> ConfigState {
        s.model
            .start(Canvas {
                fps_num: 30,
                fps_den: 1,
                output_width: 1280,
                output_height: 720,
            })
            .unwrap();
        s.model.claim(None);
        s.model.started(Ok(Started {
            ownership: None,
            channel: "c".into(),
        }));
        s
    }

    #[test]
    fn a_mic_on_the_unmanaged_track_blocks_go_live_and_says_how_to_fix_it() {
        let v = panel_view(&picked(false), true, None, &mic_on_track_6(), false);
        assert_eq!(v.tone, Tone::Attention);
        assert_eq!(v.state_line, "Discord would hear Mic/Aux");
        assert_eq!(v.state_fix, "Manage Track 6");
        assert!(
            v.state_tooltip
                .starts_with("Mic/Aux is on Track 6, so Discord would hear it."),
            "{}",
            v.state_tooltip
        );
        assert!(!v.button.enabled, "Start is held back");
        assert_eq!(v.audio.manage, "Manage Track 6", "the fix is right there");
        assert!(
            v.audio.line.contains("including Mic/Aux"),
            "{}",
            v.audio.line
        );

        let managed = panel_view(&picked(true), true, None, &mic_on_track_6(), false);
        assert!(managed.button.enabled, "a managed track can go live");
        let no_mic = view(&picked(false));
        assert!(no_mic.button.enabled, "nothing to hold back without a mic");
        let leak = panel_view(&live(picked(false)), true, None, &mic_on_track_6(), false);
        assert_eq!(leak.tone, Tone::Attention, "Discord hears the mic now");
        assert_eq!(leak.state_line, "Discord hears Mic/Aux");
        assert_eq!(leak.state_fix, "Manage Track 6");
        assert_eq!(leak.status.state, State::Error);
        assert!(leak.button.enabled, "Stop stays");
        assert_eq!(
            leak.audio.manage, "Manage Track 6",
            "the fix for the heard track"
        );
        let mut s = live(picked(false));
        s.model.take_over(6);
        let resolved = panel_view(&s, true, None, &mic_on_track_6(), false);
        assert_eq!(resolved.tone, Tone::Live, "clears once managed");
    }

    #[test]
    fn a_managed_track_1_says_streams_and_recordings_lost_it() {
        let mut s = picked(false);
        s.model.select_audio_track(1);
        s.model.take_over(1);
        let v = view(&s);
        assert!(v.audio.managed);
        assert_eq!(v.audio.notices.len(), 1);
        assert!(
            v.audio.notices[0]
                .text
                .starts_with("Track 1 is OBS's streaming and recording track"),
            "{:?}",
            v.audio.notices
        );
        assert_eq!(v.audio.notices[0].release, None, "Discord gets Track 1");
    }

    #[test]
    fn while_live_the_audio_part_shows_the_track_discord_hears() {
        let mut s = live(picked(true));
        assert!(s.model.select_audio_track(3));
        let v = panel_view(&s, true, None, &sources(), false);
        assert_eq!(v.audio.track, 6, "Discord still hears Track 6");
        assert!(v.audio.managed);
        assert_eq!(v.audio.sources.iter().filter(|s| s.on).count(), 1);
        let picked: Vec<u8> = v
            .audio
            .tracks
            .iter()
            .filter(|t| t.selected)
            .map(|t| t.track)
            .collect();
        assert_eq!(picked, [3], "the next Go Live's pick stays selected");
    }

    #[test]
    fn without_controls_the_panel_never_offers_to_change_mixers() {
        let v = panel_view(&picked(false), false, None, &mic_on_track_6(), true);
        assert_eq!(v.audio.line, NO_AUDIO_CONTROLS);
        assert_eq!(v.audio.manage, "");
        assert_eq!(v.audio.confirm, None);
        assert!(!v.audio.managed);
        let v = panel_view(&picked(true), false, None, &sources(), false);
        assert!(v.audio.sources.is_empty() && !v.audio.managed);
    }

    #[test]
    fn live_shows_stats_and_locks_the_lists() {
        let mut s = picked(true);
        s.model
            .start(Canvas {
                fps_num: 30,
                fps_den: 1,
                output_width: 1280,
                output_height: 720,
            })
            .unwrap();
        s.model.claim(None);
        s.model.started(Ok(Started {
            ownership: None,
            channel: "c".into(),
        }));
        let stats = LiveStats {
            uptime_ms: 754_000,
            rates: Some(Rates {
                bits_per_sec: 6_000_000.0,
                fps: 60.0,
            }),
            ..LiveStats::default()
        };
        let v = panel_view(&s, true, Some(&stats), &sources(), false);
        assert_eq!(v.state_line, "Live · 6.0 Mbit/s · 60 fps · 12:34");
        assert_eq!(v.state_fix, "", "no service report");
        assert_eq!(v.status.state, State::Live);
        let texts: Vec<&str> = v.status.counters.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["0", "–", "–", "–"]);
        assert_eq!(v.status.duration, "12:34");
        let no_stats = panel_view(&s, true, None, &sources(), false);
        assert_eq!(no_stats.status.duration, "");
        assert_eq!(
            v.status.tooltip,
            "Live · Kids Lounge 🧃\nMissed frames: 0\nAudio: unknown\nReconnects: unknown\n\
             Viewers: unknown\nDuration: 12:34\nVideo: 6.0 Mbit/s, 60 fps\nTrack 6"
        );
        assert!(!v.guilds.enabled && !v.channels.enabled);
        assert_eq!(v.button.label, "Stop D-Streamy");
        assert!(v.audio.managed, "ticks still work while live");
        let watched = LiveStats {
            viewers: Some(3),
            service: Some(ServiceHealth {
                audio_per_sec: Some(50),
                jitter_p95_ms: Some(18),
                jitter_max_ms: Some(41),
                ..ServiceHealth::default()
            }),
            ..stats
        };
        let v = panel_view(&s, true, Some(&watched), &sources(), false);
        assert_eq!(v.state_line, "Live · 6.0 Mbit/s · 60 fps · 12:34");
        assert_eq!(v.state_fix, "Frame gaps 18 ms p95, 41 ms max · audio 50/s");
        let texts: Vec<&str> = v.status.counters.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["0", "ok", "0", "3"]);
        assert!(
            v.status.tooltip.contains("\nViewers: 3\n"),
            "{}",
            v.status.tooltip
        );
        let unfed = panel_view(&s, true, Some(&watched), &[], false);
        assert_eq!(
            unfed.status.counters[1].text, "silent",
            "nothing feeds Track 6"
        );
        assert_eq!(unfed.status.state, State::Degraded);
        let windows = panel_view(&s, false, Some(&watched), &[], false);
        assert_ne!(
            windows.status.counters[1].text, "silent",
            "no mixer data there"
        );
    }

    #[test]
    fn idle_names_the_channel_in_its_line() {
        let v = view(&picked(true));
        assert_eq!(v.tone, Tone::Idle);
        assert_eq!(v.state_line, "Idle · Kids Lounge 🧃");
        assert_eq!(v.state_fix, "");
        assert_eq!(v.status.state, State::Idle);
        assert_eq!(v.status.tooltip, "Idle · Kids Lounge 🧃\nTrack 6");
        assert_eq!(v.button.label, "Start D-Streamy");
    }

    #[test]
    fn the_json_carries_every_part_for_the_ui() {
        let v: Value = serde_json::from_str(&view(&picked(true)).to_json()).unwrap();
        for key in [
            "tone",
            "mark",
            "status",
            "state_line",
            "state_fix",
            "state_tooltip",
            "guilds",
            "channels",
            "audio",
            "button",
            "login",
            "account",
        ] {
            assert!(v.get(key).is_some(), "{key}: {v}");
        }
        assert_eq!(v["guilds"]["items"][0]["selected"], true);
        assert_eq!(v["audio"]["sources"][2]["on"], true);
        assert_eq!(v["audio"]["confirm"], Value::Null);
        assert_eq!(v["button"]["action"], "start");
        assert_eq!(v["login"], Value::Null);
        assert_eq!(v["account"], Value::Null);
    }

    fn accounts(active: Option<&str>, names: &[&str]) -> Accounts {
        Accounts {
            active: active.map(Into::into),
            accounts: names
                .iter()
                .map(|n| Account {
                    name: (*n).into(),
                    username: Some(format!("{n}_user")),
                    global_name: (*n == "a").then(|| "Alpha".into()),
                    has_token: true,
                    active: Some(*n) == active,
                })
                .collect(),
        }
    }

    #[test]
    fn the_account_row_lists_accounts_and_hides_without_serve_support() {
        let mut s = picked(true);
        assert_eq!(view(&s).account, None, "an older serve has no accounts");
        s.model.accounts_loaded(Ok(Some(accounts(None, &[]))));
        assert_eq!(
            view(&s).account,
            None,
            "no account: the Log in button instead"
        );
        s.model
            .accounts_loaded(Ok(Some(accounts(Some("a"), &["a", "b"]))));
        let row = view(&s).account.unwrap();
        assert_eq!(row.label, "Account");
        assert_eq!(row.add, "Add account…");
        assert_eq!(row.logout, "Log out");
        assert!(row.enabled);
        assert_eq!(row.tooltip, "");
        assert_eq!(
            row.items,
            [
                AccountItem {
                    name: "a".into(),
                    user: "Alpha".into(),
                    selected: true
                },
                AccountItem {
                    name: "b".into(),
                    user: "b_user".into(),
                    selected: false
                },
            ]
        );
        assert_eq!(panel_view(&s, false, None, &[], false).account, None);
        let json: Value = serde_json::from_str(&view(&s).to_json()).unwrap();
        assert_eq!(json["account"]["items"][1]["name"], "b");
        assert_eq!(json["account"]["enabled"], true);
    }

    #[test]
    fn live_disables_the_account_row_and_says_why() {
        let mut s = picked(true);
        s.model
            .accounts_loaded(Ok(Some(accounts(Some("a"), &["a", "b"]))));
        let s = live(s);
        let row = view(&s).account.unwrap();
        assert!(!row.enabled);
        assert_eq!(row.tooltip, "Stop the stream to switch accounts");
        assert_eq!(row.logout_tooltip, "Stop the stream to log out");
    }

    #[test]
    fn a_switch_shows_the_pending_pick_and_brings_back_each_account_s_server() {
        let mut s = picked(true);
        s.model
            .accounts_loaded(Ok(Some(accounts(Some("a"), &["a", "b"]))));
        assert_eq!(
            s.model.settings().account.as_deref(),
            Some("a"),
            "the pick made before accounts belongs to the first one seen"
        );
        assert!(!s.model.begin_select_account("a"), "already active");
        assert!(!s.model.begin_select_account("zz"), "not listed");
        assert!(s.model.begin_select_account("b"));
        assert_eq!(s.model.pending_account(), Some("b"));
        let row = view(&s).account.unwrap();
        assert!(!row.enabled);
        assert_eq!(row.tooltip, "Switching account…");
        assert!(
            row.items[1].selected && !row.items[0].selected,
            "the pick, not the old account"
        );
        assert!(
            s.model
                .account_changed(Ok(accounts(Some("b"), &["a", "b"])))
        );
        assert_eq!(s.model.pending_account(), None);
        assert!(s.model.guilds().is_empty(), "the old account's lists go");
        let v = view(&s);
        assert!(v.account.unwrap().items[1].selected);
        assert_eq!(s.model.settings().guild_id, None, "b never picked a server");
        assert!(v.guilds.items.is_empty());

        s.model.guilds_loaded(Ok(vec![named("x", "B's server")]));
        s.model.select_guild("x");
        s.model
            .channels_loaded("x", Ok(vec![named("y", "B's room")]));
        s.model.select_channel("y");
        assert!(s.model.begin_select_account("a"));
        assert!(
            s.model
                .account_changed(Ok(accounts(Some("a"), &["a", "b"])))
        );
        let settings = s.model.settings();
        assert_eq!(
            (settings.guild_id.as_deref(), settings.channel_id.as_deref()),
            (Some("g"), Some("c")),
            "a's pick comes back"
        );
        assert_eq!(settings.picks["b"].guild_id.as_deref(), Some("x"));
        assert_eq!(settings.picks["b"].channel_id.as_deref(), Some("y"));

        s.model
            .accounts_loaded(Ok(Some(accounts(Some("a"), &["a"]))));
        assert!(
            s.model.settings().picks.is_empty(),
            "a logged out account's pick goes"
        );
    }

    #[test]
    fn a_saved_server_the_account_isn_t_in_is_flagged_and_blocks_start() {
        let mut s = picked(true);
        s.model.guilds_loaded(Ok(vec![named("h", "Other")]));
        let v = view(&s);
        assert_eq!(
            v.guilds.items.last().unwrap(),
            &Item {
                id: "g".into(),
                name: "Not available".into(),
                selected: true
            }
        );
        assert_eq!(v.state_line, "Server not available");
        assert_eq!(v.state_fix, "Pick the server again");
        assert!(!v.button.enabled);
        assert!(s.model.start(canvas()).is_err());
        s.model.select_guild("h");
        s.model.channels_loaded("h", Ok(vec![named("e", "Room")]));
        let v = view(&s);
        assert_eq!(v.state_line, "Not set up", "{v:?}");

        let mut s = picked(true);
        s.model
            .channels_loaded("g", Ok(vec![named("d", "Elsewhere")]));
        let v = view(&s);
        assert_eq!(v.channels.items.last().unwrap().name, "Not available");
        assert_eq!(v.state_line, "Voice channel not available");
        assert_eq!(v.state_fix, "Pick the voice channel again");
        assert!(!v.button.enabled);
    }

    fn canvas() -> Canvas {
        Canvas {
            fps_num: 30,
            fps_den: 1,
            output_width: 1280,
            output_height: 720,
        }
    }

    #[test]
    fn log_out_asks_first_and_a_retry_that_works_clears_the_failure() {
        let mut s = picked(true);
        s.model
            .accounts_loaded(Ok(Some(accounts(Some("a"), &["a", "b"]))));
        assert!(!s.model.request_logout("zz"));
        assert!(s.model.request_logout("a"));
        let row = view(&s).account.unwrap();
        assert_eq!(
            row.confirm,
            Some(Confirm {
                text: "Log out a?".into(),
                ok: "Log out".into(),
                cancel: "Not now".into(),
                instead: None
            })
        );
        assert_eq!(s.model.confirm_logout(false), None);
        assert_eq!(view(&s).account.unwrap().confirm, None);
        assert!(s.model.request_logout("a"));
        assert_eq!(s.model.confirm_logout(true).as_deref(), Some("a"));
        assert!(s.model.begin_logout_account("a"));
        assert!(
            !s.model
                .account_changed(Err(ControlError::Locked("pid 7".into())))
        );
        assert_eq!(view(&s).state_line, "Account in use");
        assert!(s.model.begin_logout_account("a"));
        assert!(s.model.account_changed(Ok(accounts(Some("b"), &["b"]))));
        assert_ne!(view(&s).state_line, "Account in use", "the retry worked");
        assert_eq!(s.model.problem(), None);
    }

    #[test]
    fn the_row_says_why_it_waits_for_a_login() {
        let mut s = state(DockSettings::default());
        s.model
            .accounts_loaded(Ok(Some(accounts(Some("a"), &["a"]))));
        s.model
            .guilds_loaded(Err(ControlError::InvalidToken { window: true }));
        assert!(s.model.begin_login());
        assert_eq!(
            s.model.login_account().as_deref(),
            Some("a"),
            "Log in again repairs a"
        );
        let row = view(&s).account.unwrap();
        assert!(!row.enabled);
        assert_eq!(row.tooltip, "Finish or cancel the login first");
        assert_eq!(row.logout_tooltip, "Finish or cancel the login first");
        s.model.login_finished(Err(ControlError::LoginFailed(
            "signed in as x, but account a belongs to y: use Add account… for x".into(),
        )));
        assert!(
            view(&s).state_tooltip.contains("use Add account… for x"),
            "{}",
            view(&s).state_tooltip
        );
    }

    #[test]
    fn a_failed_add_account_keeps_the_log_in_again_offer() {
        let mut s = state(DockSettings::default());
        s.model
            .accounts_loaded(Ok(Some(accounts(Some("a"), &["a"]))));
        s.model
            .guilds_loaded(Err(ControlError::InvalidToken { window: true }));
        assert!(s.model.begin_add_account());
        assert_eq!(s.model.login_account(), None, "a new account");
        s.model.login_finished(Err(ControlError::LoginCancelled));
        assert_eq!(view(&s).login, Some(log_in("Log in again")));

        let mut s = state(DockSettings::default());
        s.model
            .accounts_loaded(Ok(Some(accounts(Some("a"), &["a"]))));
        assert!(s.model.begin_add_account());
        s.model.login_finished(Err(ControlError::LoginCancelled));
        let v = view(&s);
        assert_eq!(v.login, None);
        assert_eq!(v.state_fix, "Use Add account… again");
    }

    #[test]
    fn a_refused_switch_shows_why_and_keeps_the_account() {
        let mut s = picked(true);
        s.model
            .accounts_loaded(Ok(Some(accounts(Some("a"), &["a", "b"]))));
        assert!(s.model.begin_logout_account("a"));
        assert!(
            !s.model
                .account_changed(Err(ControlError::Streaming("stop the stream first".into())))
        );
        let v = view(&s);
        assert_eq!(v.state_line, "Stream running");
        assert!(v.account.unwrap().enabled);
        assert!(s.model.begin_select_account("b"));
        assert!(
            s.model
                .account_changed(Err(ControlError::UnknownAccount("saved: a".into()))),
            "an unknown account reloads the list"
        );
    }

    #[test]
    fn add_account_waits_for_the_login_and_a_failure_offers_no_stale_button() {
        let mut s = picked(true);
        s.model
            .accounts_loaded(Ok(Some(accounts(Some("a"), &["a"]))));
        assert!(s.model.begin_add_account());
        let v = view(&s);
        assert!(!v.account.unwrap().enabled);
        assert_eq!(
            v.login,
            Some(Login {
                label: CANCEL_LOGIN,
                action: LoginAction::Cancel
            })
        );
        s.model.login_finished(Err(ControlError::LoginCancelled));
        let v = view(&s);
        assert_eq!(v.state_line, "Login cancelled");
        assert_eq!(v.login, None, "Add account… stays in the picker");
        assert!(v.account.unwrap().enabled);
        assert!(s.model.begin_add_account());
        assert!(s.model.login_finished(Ok(Some("New".into()))));
    }
}
