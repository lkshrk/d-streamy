//! The Discord track's audio sources: which OBS sources feed it, and what the settings dialog shows about them.

use crate::dock::{AUDIO_TRACKS, DockModel, track_name};

/// An OBS source with audio, as libobs reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSource {
    pub uuid: String,
    pub name: String,
    /// OBS's mixer flags: bit 0 is Track 1.
    pub mixers: u32,
}

/// One checkbox: whether `uuid` feeds the managed track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRow {
    pub uuid: String,
    pub name: String,
    pub on: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioView {
    /// Nothing is changed on any source until the user takes the track over.
    Unmanaged {
        track: u8,
        help: String,
        take_over: String,
    },
    Managed {
        track: u8,
        help: String,
        /// Sorted by name; empty when OBS has no audio source yet.
        sources: Vec<SourceRow>,
        empty: Option<String>,
    },
}

pub fn track_bit(track: u8) -> u32 {
    debug_assert!(AUDIO_TRACKS.contains(&track));
    1 << (u32::from(track) - 1)
}

/// `mixers` with `track` turned on or off, every other track untouched.
pub fn with_track(mixers: u32, track: u8, on: bool) -> u32 {
    if on {
        mixers | track_bit(track)
    } else {
        mixers & !track_bit(track)
    }
}

pub fn take_over_label(track: u8) -> String {
    format!("Let D-Streamy manage {}", track_name(track))
}

fn take_over_help(track: u8) -> String {
    let t = track_name(track);
    format!(
        "D-Streamy can manage {t} for you: it turns {t} off on every audio source, then you tick what \
         Discord hears. This changes {t} in Edit › Advanced Audio Properties, so recordings or streams \
         that use {t} change too."
    )
}

fn managed_help(track: u8) -> String {
    let t = track_name(track);
    format!(
        "Discord hears the ticked sources. Ticking turns {t} on for that source in Edit › Advanced Audio \
         Properties, at once. Another track applies on the next Go Live and has to be taken over first."
    )
}

pub const NO_SOURCES: &str = "OBS has no audio source yet. Add one, and it shows up here.";

/// The dialog's audio part for the track it shows, which may differ from the saved one while it is being picked.
pub fn audio_view(model: &DockModel, track: u8, sources: &[AudioSource]) -> AudioView {
    if model.settings().managed_track != Some(track) {
        return AudioView::Unmanaged {
            track,
            help: take_over_help(track),
            take_over: take_over_label(track),
        };
    }
    let mut rows: Vec<SourceRow> = sources
        .iter()
        .map(|s| SourceRow {
            uuid: s.uuid.clone(),
            name: s.name.clone(),
            on: s.mixers & track_bit(track) != 0,
        })
        .collect();
    rows.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.uuid.cmp(&b.uuid))
    });
    rows.dedup_by(|a, b| a.uuid == b.uuid);
    AudioView::Managed {
        track,
        help: managed_help(track),
        empty: rows.is_empty().then(|| NO_SOURCES.to_owned()),
        sources: rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dock::DockSettings;

    fn source(uuid: &str, name: &str, mixers: u32) -> AudioSource {
        AudioSource {
            uuid: uuid.into(),
            name: name.into(),
            mixers,
        }
    }

    fn model(track: u8, managed: Option<u8>) -> DockModel {
        DockModel::new(DockSettings {
            audio_track: track,
            managed_track: managed,
            ..DockSettings::default()
        })
    }

    #[test]
    fn bits_follow_obs_track_numbers() {
        assert_eq!(track_bit(1), 0b1);
        assert_eq!(track_bit(6), 0b10_0000);
        assert_eq!(with_track(0b11_1111, 6, false), 0b01_1111);
        assert_eq!(with_track(0b00_0001, 6, true), 0b10_0001);
        assert_eq!(with_track(0b10_0001, 6, true), 0b10_0001);
        assert_eq!(with_track(0, 3, false), 0);
    }

    #[test]
    fn an_unmanaged_track_only_offers_the_takeover() {
        let v = audio_view(&model(6, None), 6, &[source("a", "Mic", 0b11_1111)]);
        let AudioView::Unmanaged {
            track,
            help,
            take_over,
        } = v
        else {
            panic!("{v:?}")
        };
        assert_eq!(track, 6);
        assert_eq!(take_over, "Let D-Streamy manage Track 6");
        assert!(
            help.contains("turns Track 6 off on every audio source"),
            "{help}"
        );
        assert!(help.contains("recordings"), "{help}");
    }

    #[test]
    fn taking_over_manages_only_the_selected_track() {
        let mut m = model(3, None);
        assert!(!m.managed());
        assert_eq!(m.take_over(), 3);
        assert!(m.managed());
        assert!(matches!(audio_view(&m, 3, &[]), AudioView::Managed { .. }));
        m.select_audio_track(4);
        assert!(!m.managed(), "a new track needs its own takeover");
        assert!(matches!(
            audio_view(&m, 4, &[]),
            AudioView::Unmanaged { track: 4, .. }
        ));
        m.select_audio_track(3);
        assert!(m.managed(), "the managed track is remembered");
    }

    #[test]
    fn the_dialog_previews_the_track_it_shows() {
        let m = model(3, Some(3));
        assert!(matches!(
            audio_view(&m, 5, &[]),
            AudioView::Unmanaged { track: 5, .. }
        ));
    }

    #[test]
    fn a_managed_track_lists_sources_sorted_with_their_bit() {
        let m = model(6, Some(6));
        let v = audio_view(
            &m,
            6,
            &[
                source("u3", "mic/Aux", 0b10_0000),
                source("u1", "Desktop Audio", 0b01_1111),
                source("u2", "Browser", 0b11_1111),
                source("u2", "Browser", 0b11_1111),
            ],
        );
        let AudioView::Managed {
            sources,
            empty,
            help,
            track,
        } = v
        else {
            panic!("{v:?}")
        };
        assert_eq!(track, 6);
        assert_eq!(empty, None);
        assert!(
            help.starts_with("Discord hears the ticked sources."),
            "{help}"
        );
        let shown: Vec<(&str, bool)> = sources.iter().map(|s| (s.name.as_str(), s.on)).collect();
        assert_eq!(
            shown,
            [
                ("Browser", true),
                ("Desktop Audio", false),
                ("mic/Aux", true)
            ]
        );
    }

    #[test]
    fn a_managed_track_without_sources_says_so() {
        let v = audio_view(&model(2, Some(2)), 2, &[]);
        assert_eq!(
            v,
            AudioView::Managed {
                track: 2,
                help: managed_help(2),
                sources: vec![],
                empty: Some(NO_SOURCES.to_owned()),
            }
        );
    }

    #[test]
    fn the_managed_track_is_saved_and_old_configs_load_unchanged() {
        let old = DockSettings::from_json(r#"{"audio_track": 3, "guild_id": "g"}"#);
        assert_eq!((old.audio_track, old.managed_track), (3, None));
        assert_eq!(DockSettings::from_json("{}").audio_track, 6, "new default");
        let s = DockSettings {
            managed_track: Some(3),
            audio_track: 3,
            ..DockSettings::default()
        };
        assert_eq!(DockSettings::from_json(&s.to_json()), s);
        for bad in ["0", "7", "\"3\"", "-1"] {
            let s = DockSettings::from_json(&format!(
                r#"{{"audio_track": 3, "managed_track": {bad}}}"#
            ));
            assert_eq!((s.audio_track, s.managed_track), (3, None), "{bad}");
        }
    }
}
