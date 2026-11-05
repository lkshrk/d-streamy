//! The status-bar item and its popup panel: everything they show, from the same model as the dock view.

use serde::Serialize;

use crate::control::Named;
use crate::dock::{AUDIO_TRACKS, DockState, track_name};
use crate::dock_config::ConfigState;
use crate::dock_view::{Button, Tone, dock_view};
use crate::live::LiveStats;
use crate::mixer::{AudioSource, AudioView, audio_view};

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
    /// Shown instead of the checkboxes while the track is unmanaged.
    pub line: String,
    pub manage: String,
    pub confirm: Option<Confirm>,
    pub sources: Vec<Source>,
    pub empty: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PanelView {
    pub tone: Tone,
    pub mark: &'static str,
    pub status_bar: String,
    pub status_tooltip: String,
    pub state_line: String,
    /// The fix for a problem, empty otherwise.
    pub state_fix: String,
    pub state_tooltip: String,
    pub guilds: Choices,
    pub channels: Choices,
    pub audio: Audio,
    pub button: Button,
}

pub const LOADING: &str = "Loading…";
pub const NOT_SET_UP: &str = "Not set up";
pub const NOT_SET_UP_FIX: &str = "Pick a server and voice channel below";

fn choices(
    list: &[Named],
    selected: Option<&str>,
    placeholder: &str,
    loading: bool,
    enabled: bool,
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
            name: id.to_owned(),
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

fn audio(state: &ConfigState, sources: &[AudioSource], confirming: bool) -> Audio {
    let m = &state.model;
    let track = m.settings().audio_track;
    let t = track_name(track);
    let tracks = AUDIO_TRACKS
        .map(|n| TrackItem {
            track: n,
            name: track_name(n),
            selected: n == track,
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
    };
    match audio_view(m, track, sources) {
        AudioView::Unmanaged { .. } => {
            audio.line =
                format!("Discord hears {t}. Let D-Streamy manage it to pick sources here.");
            audio.manage = format!("Manage {t}");
            audio.confirm = confirming.then(|| Confirm {
                text: format!(
                    "Turns {t} off on every audio source, also for recordings or VODs that use it."
                ),
                ok: "Manage".to_owned(),
                cancel: "Cancel".to_owned(),
            });
        }
        AudioView::Managed { sources, empty, .. } => {
            audio.managed = true;
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
    let dock = dock_view(state, controls, live);
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
        Tone::Live => (
            format!("Live · {}", dock.detail),
            String::new(),
            format!("{}\n{}", dock.headline, dock.detail_tooltip),
        ),
        Tone::Idle | Tone::Busy => (
            dock.headline.clone(),
            String::new(),
            format!("{}\n{}", dock.headline, dock.detail_tooltip),
        ),
    };
    let idle = m.state() == &DockState::Idle;
    let (loading_guilds, loading_channels) = m.loading();
    PanelView {
        tone: dock.tone,
        mark: dock.mark,
        status_bar: match (dock.tone, live) {
            (Tone::Live, Some(_)) => format!("{} · {} ▴", dock.status_bar, dock.detail),
            _ => format!("{} ▴", dock.status_bar),
        },
        status_tooltip: if not_set_up {
            state_tooltip.clone()
        } else {
            format!("{}\n{}", dock.headline, dock.detail_tooltip)
        },
        state_line,
        state_fix,
        state_tooltip,
        guilds: choices(
            m.guilds(),
            settings.guild_id.as_deref(),
            "Pick a server",
            loading_guilds,
            idle,
        ),
        channels: choices(
            m.channels(),
            settings.channel_id.as_deref(),
            "Pick a voice channel",
            loading_channels,
            idle && settings.guild_id.is_some(),
        ),
        audio: audio(state, sources, confirming),
        button: dock.button,
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
    use crate::control::{ControlError, Started};
    use crate::dock::{Canvas, DockModel, DockSettings};
    use crate::live::Rates;

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
            managed_track: managed.then_some(6),
            ..DockSettings::default()
        });
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
                v.status_tooltip,
                "Not set up\nPick a server and voice channel below"
            );
            assert_eq!(v.state_tooltip, v.status_tooltip);
            assert!(!v.state_fix.contains('⚙'), "the panel has no gear");
            assert!(!v.button.enabled);
        }
        assert_eq!(view(&picked(true)).state_fix, "", "set up");
    }

    #[test]
    fn a_failed_load_ends_loading_and_shows_the_problem() {
        let mut s = ConfigState::default();
        s.model.lists_requested();
        s.model.guilds_loaded(Err(ControlError::NotLoggedIn));
        s.model.lists_finished();
        let v = view(&s);
        assert!(!v.guilds.loading);
        assert_eq!(v.tone, Tone::Attention);
        assert_eq!(v.state_line, "Not logged in");
        assert_eq!(v.state_fix, "Run `dstreamy login`");
        assert_eq!(v.state_tooltip, "Log in first: run `dstreamy login`");
        assert_eq!(v.status_bar, "⚠ D-Streamy ▴");
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
            "Discord hears Track 6. Let D-Streamy manage it to pick sources here."
        );
        assert_eq!(v.audio.manage, "Manage Track 6");
        assert_eq!(v.audio.confirm, None);
        assert!(v.audio.sources.is_empty(), "nothing to tick yet");

        let v = panel_view(&s, true, None, &sources(), true);
        assert_eq!(
            v.audio.confirm,
            Some(Confirm {
                text: "Turns Track 6 off on every audio source, also for recordings or VODs that use it."
                    .into(),
                ok: "Manage".into(),
                cancel: "Cancel".into(),
            })
        );
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
            dropped: 0,
        };
        let v = panel_view(&s, true, Some(&stats), &sources(), false);
        assert_eq!(v.state_line, "Live · 6.0 Mbit/s · 60 fps · 12:34");
        assert_eq!(
            v.status_bar,
            "● D-Streamy live · 6.0 Mbit/s · 60 fps · 12:34 ▴"
        );
        let no_stats = panel_view(&s, true, None, &sources(), false);
        assert_eq!(no_stats.status_bar, "● D-Streamy live ▴");
        assert_eq!(
            v.status_tooltip,
            "Live · Kids Lounge 🧃\n6.0 Mbit/s · 60 fps · 12:34 · 0 dropped\nTrack 6"
        );
        assert!(!v.guilds.enabled && !v.channels.enabled);
        assert_eq!(v.button.label, "Stop D-Streamy");
        assert!(v.audio.managed, "ticks still work while live");
    }

    #[test]
    fn idle_names_the_channel_in_its_line() {
        let v = view(&picked(true));
        assert_eq!(v.tone, Tone::Idle);
        assert_eq!(v.state_line, "Idle · Kids Lounge 🧃");
        assert_eq!(v.state_fix, "");
        assert_eq!(v.status_bar, "D-Streamy idle ▴");
        assert_eq!(v.status_tooltip, "Idle · Kids Lounge 🧃\nTrack 6");
        assert_eq!(v.button.label, "Start D-Streamy");
    }

    #[test]
    fn the_json_carries_every_part_for_the_ui() {
        let v: Value = serde_json::from_str(&view(&picked(true)).to_json()).unwrap();
        for key in [
            "tone",
            "mark",
            "status_bar",
            "status_tooltip",
            "state_line",
            "state_fix",
            "state_tooltip",
            "guilds",
            "channels",
            "audio",
            "button",
        ] {
            assert!(v.get(key).is_some(), "{key}: {v}");
        }
        assert_eq!(v["guilds"]["items"][0]["selected"], true);
        assert_eq!(v["audio"]["sources"][2]["on"], true);
        assert_eq!(v["audio"]["confirm"], Value::Null);
        assert_eq!(v["button"]["action"], "start");
    }
}
