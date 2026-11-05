//! The status-bar item and its popup panel: everything they show, from the same model as the dock view.

use serde::Serialize;

use crate::control::Named;
use crate::dock::{AUDIO_TRACKS, DockState, track_name};
use crate::dock_config::ConfigState;
use crate::dock_view::{Button, Tone, dock_view};
use crate::live::LiveStats;
use crate::mixer::{
    AudioSource, AudioView, CONFIRM, NOT_NOW, audio_view, instead_label, mic_guard, notices,
    release_label,
};

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
pub const NO_AUDIO_CONTROLS: &str = "D-Streamy can't change Discord audio on Windows yet.";

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
        audio: audio(state, controls, sources, confirming),
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
        assert_eq!(leak.status_bar, "⚠ D-Streamy live ▴");
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
