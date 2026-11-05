//! The Discord track's audio sources: which OBS sources feed it, and what the settings dialog shows about them.

use crate::dock::{AUDIO_TRACKS, DockModel, track_name};

/// An OBS source with audio, as libobs reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSource {
    pub uuid: String,
    pub name: String,
    /// OBS's mixer flags: bit 0 is Track 1.
    pub mixers: u32,
    /// A microphone or other input device, or a source in a Mic/Aux channel.
    pub mic: bool,
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
        /// What the takeover does, shown before it is confirmed.
        effect: String,
    },
    Managed {
        track: u8,
        help: String,
        /// Sorted by name; empty when OBS has no audio source yet.
        sources: Vec<SourceRow>,
        empty: Option<String>,
        /// A warning about the managed track itself.
        note: Option<String>,
    },
}

/// OBS's global Mic/Aux channels, where Settings › Audio puts microphones (`OBSBasic::LoadData`).
pub const MIC_AUX_CHANNELS: std::ops::RangeInclusive<u32> = 3..=6;

/// Whether an unversioned source id captures an input device; video capture devices (mostly capture cards) don't.
pub fn is_input_device(id: &str) -> bool {
    id.ends_with("_input_capture") || matches!(id, "jack_output_capture" | "sndio_output_capture")
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

pub fn take_over_effect(track: u8) -> String {
    format!(
        "Turns {} off on every audio source, also for recordings and streams that use it.",
        track_name(track)
    )
}

pub const CONFIRM: &str = "Manage";
pub const NOT_NOW: &str = "Not now";

/// Shown while Track 1, OBS's own stream and recording track, is managed.
pub const TRACK_1_NOTE: &str = "Track 1 is OBS's streaming and recording track, and D-Streamy turned it off \
     on every source. To fix it, pick Track 6 for Discord, then turn Track 1 back on for each source in \
     Edit › Advanced Audio Properties.";

/// Names in one line: "Mic", "Mic and Headset", "Mic, Headset and Webcam".
fn names(list: &[String]) -> String {
    match list {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// What Discord hears from a track D-Streamy doesn't manage.
pub fn unmanaged_line(track: u8, sources: &[AudioSource]) -> String {
    let t = track_name(track);
    let mics = mics_on(track, sources);
    let heard = if mics.is_empty() {
        format!("Discord hears every source on {t}.")
    } else {
        format!(
            "Discord hears every source on {t}, including {}.",
            names(&mics)
        )
    };
    format!("{heard} Let D-Streamy manage {t} to pick sources here.")
}

fn managed_help(track: u8) -> String {
    let t = track_name(track);
    format!(
        "Discord hears the ticked sources. Ticking turns {t} on for that source in Edit › Advanced Audio \
         Properties, at once. Another track applies on the next Go Live."
    )
}

pub const NO_SOURCES: &str = "OBS has no audio source yet. Add one, and it shows up here.";

/// The names of microphone-type sources with `track` on, sorted.
fn mics_on(track: u8, sources: &[AudioSource]) -> Vec<String> {
    let mut mics: Vec<&AudioSource> = sources
        .iter()
        .filter(|s| s.mic && s.mixers & track_bit(track) != 0)
        .collect();
    mics.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.uuid.cmp(&b.uuid))
    });
    mics.dedup_by(|a, b| a.uuid == b.uuid);
    mics.into_iter().map(|s| s.name.clone()).collect()
}

/// A microphone feeds the unmanaged track Discord hears now, or would hear on Go Live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicGuard {
    pub track: u8,
    pub mics: Vec<String>,
    /// Discord hears it now; Go Live is otherwise held back.
    pub live: bool,
}

impl MicGuard {
    pub fn headline(&self) -> String {
        let who = names(&self.mics);
        if self.live {
            format!("Discord hears {who}")
        } else {
            format!("Discord would hear {who}")
        }
    }

    pub fn fix(&self) -> String {
        take_over_label(self.track)
    }

    pub fn message(&self) -> String {
        let t = track_name(self.track);
        let who = names(&self.mics);
        let (verb, it) = if self.mics.len() == 1 {
            ("is", "it")
        } else {
            ("are", "them")
        };
        let hears = if self.live {
            format!("Discord hears {it} now")
        } else {
            format!("Discord would hear {it}")
        };
        format!(
            "{who} {verb} on {t}, so {hears}. Let D-Streamy manage {t}, \
             or turn {t} off for {who} in Edit › Advanced Audio Properties."
        )
    }
}

/// The mic Discord hears, or would hear on Go Live, from the unmanaged track it gets.
pub fn mic_guard(model: &DockModel, sources: &[AudioSource]) -> Option<MicGuard> {
    let track = model.heard_track();
    if model.manages(track) {
        return None;
    }
    let mics = mics_on(track, sources);
    (!mics.is_empty()).then_some(MicGuard {
        track,
        mics,
        live: model.live_track().is_some(),
    })
}

/// Cancel's mixer flags for one source: `touched` tracks as at open, or OBS's default for a source added since.
pub fn restored_mixers(current: u32, at_open: Option<u32>, touched: u32, managed: u32) -> u32 {
    let wanted = at_open.map_or(touched & !managed, |old| old & touched);
    (current & !touched) | wanted
}

/// Cancel's managed tracks: only the `touched` ones go back; a takeover from elsewhere stays.
pub fn restored_managed(current: u32, at_open: u32, touched: u32) -> u32 {
    (current & !touched) | (at_open & touched)
}

/// The audio part for `track`, which may differ from the saved one while it is being picked or while live.
pub fn audio_view(model: &DockModel, track: u8, sources: &[AudioSource]) -> AudioView {
    if !model.manages(track) {
        return AudioView::Unmanaged {
            track,
            help: unmanaged_line(track, sources),
            take_over: take_over_label(track),
            effect: take_over_effect(track),
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
        note: (track == 1).then(|| TRACK_1_NOTE.to_owned()),
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
            mic: false,
        }
    }

    fn mic(uuid: &str, name: &str, mixers: u32) -> AudioSource {
        AudioSource {
            mic: true,
            ..source(uuid, name, mixers)
        }
    }

    fn model(track: u8, managed: Option<u8>) -> DockModel {
        let mut m = DockModel::new(DockSettings {
            audio_track: track,
            ..DockSettings::default()
        });
        if let Some(t) = managed {
            m.take_over(t);
        }
        m
    }

    #[test]
    fn input_devices_are_known_by_their_capture_id() {
        for id in [
            "pulse_input_capture",
            "alsa_input_capture",
            "wasapi_input_capture",
            "coreaudio_input_capture",
            "oss_input_capture",
            "pipewire_audio_input_capture",
            "jack_output_capture",
            "sndio_output_capture",
        ] {
            assert!(is_input_device(id), "{id}");
        }
        for id in [
            "pulse_output_capture",
            "wasapi_output_capture",
            "wasapi_process_output_capture",
            "coreaudio_output_capture",
            "pipewire_audio_output_capture",
            "sck_audio_capture",
            "dshow_input",
            "macos-avcapture",
            "av_capture_input",
            "decklink-input",
            "v4l2_input",
            "ffmpeg_source",
            "browser_source",
            "",
        ] {
            assert!(!is_input_device(id), "{id}");
        }
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
        let v = audio_view(&model(6, None), 6, &[source("a", "Music", 0b11_1111)]);
        let AudioView::Unmanaged {
            track,
            help,
            take_over,
            effect,
        } = v
        else {
            panic!("{v:?}")
        };
        assert_eq!(track, 6);
        assert_eq!(take_over, "Let D-Streamy manage Track 6");
        assert_eq!(
            help,
            "Discord hears every source on Track 6. Let D-Streamy manage Track 6 to pick sources here."
        );
        assert_eq!(
            effect,
            "Turns Track 6 off on every audio source, also for recordings and streams that use it."
        );
    }

    #[test]
    fn an_unmanaged_track_names_the_mics_discord_hears() {
        let sources = [
            mic("m", "Mic/Aux", 0b11_1111),
            mic("h", "Headset", 0b10_0000),
            mic("off", "Webcam", 0b01_1111),
            source("a", "Music", 0b11_1111),
        ];
        let AudioView::Unmanaged { help, .. } = audio_view(&model(6, None), 6, &sources) else {
            panic!()
        };
        assert_eq!(
            help,
            "Discord hears every source on Track 6, including Headset and Mic/Aux. \
             Let D-Streamy manage Track 6 to pick sources here."
        );
    }

    #[test]
    fn go_live_waits_while_an_unmanaged_track_carries_a_mic() {
        let sources = [
            mic("m", "Mic/Aux", 0b11_1111),
            source("a", "Music", 0b11_1111),
        ];
        let guard = mic_guard(&model(6, None), &sources).expect("held back");
        assert_eq!(guard.track, 6);
        assert!(!guard.live);
        assert_eq!(guard.headline(), "Discord would hear Mic/Aux");
        assert_eq!(guard.fix(), "Let D-Streamy manage Track 6");
        assert_eq!(
            guard.message(),
            "Mic/Aux is on Track 6, so Discord would hear it. Let D-Streamy manage Track 6, \
             or turn Track 6 off for Mic/Aux in Edit › Advanced Audio Properties."
        );
        let two = [mic("m", "Mic", 0b10_0000), mic("w", "Webcam", 0b10_0000)];
        let both = mic_guard(&model(6, None), &two).unwrap();
        assert_eq!(both.headline(), "Discord would hear Mic and Webcam");
        assert_eq!(
            both.message(),
            "Mic and Webcam are on Track 6, so Discord would hear them. Let D-Streamy manage Track 6, \
             or turn Track 6 off for Mic and Webcam in Edit › Advanced Audio Properties."
        );

        assert_eq!(mic_guard(&model(6, Some(6)), &sources), None, "managed");
        assert_eq!(
            mic_guard(&model(6, None), &[mic("m", "Mic", 0b01_1111)]),
            None,
            "the mic is off Track 6"
        );
        assert_eq!(
            mic_guard(&model(6, None), &[source("a", "Music", 0b11_1111)]),
            None,
            "no mic"
        );
        assert_eq!(
            mic_guard(&model(3, Some(6)), &sources).map(|g| g.track),
            Some(3),
            "the next start's track counts"
        );
    }

    fn live_on(mut m: DockModel) -> DockModel {
        m.start(crate::dock::Canvas {
            fps_num: 30,
            fps_den: 1,
            output_width: 1280,
            output_height: 720,
        })
        .ok();
        m.claim(None);
        m.started(Ok(crate::control::Started {
            ownership: None,
            channel: "c".into(),
        }));
        m
    }

    fn picked(track: u8, managed: Option<u8>) -> DockModel {
        let mut m = DockModel::new(DockSettings {
            guild_id: Some("g".into()),
            channel_id: Some("c".into()),
            audio_track: track,
            ..DockSettings::default()
        });
        if let Some(t) = managed {
            m.take_over(t);
        }
        m
    }

    #[test]
    fn while_live_the_guard_follows_the_track_discord_hears() {
        let sources = [mic("m", "Mic/Aux", 0b11_1111)];
        let mut m = live_on(picked(6, None));
        let guard = mic_guard(&m, &sources).expect("the mic leaks");
        assert!(guard.live);
        assert_eq!(guard.headline(), "Discord hears Mic/Aux");
        assert_eq!(
            guard.message(),
            "Mic/Aux is on Track 6, so Discord hears it now. Let D-Streamy manage Track 6, \
             or turn Track 6 off for Mic/Aux in Edit › Advanced Audio Properties."
        );
        m.select_audio_track(3);
        m.take_over(3);
        assert_eq!(
            mic_guard(&m, &sources).map(|g| g.track),
            Some(6),
            "the live track counts, not the next pick"
        );
        m.take_over(6);
        assert_eq!(mic_guard(&m, &sources), None, "resolved once managed");
    }

    #[test]
    fn cancel_puts_back_only_the_touched_tracks() {
        let six = track_bit(6);
        assert_eq!(
            restored_mixers(0b01_1111, Some(0b11_1111), six, 0),
            0b11_1111
        );
        assert_eq!(
            restored_mixers(0b11_1111, Some(0b01_1111), six, 0),
            0b01_1111
        );
        assert_eq!(
            restored_mixers(0b01_1110, Some(0b11_1111), six, 0),
            0b11_1110,
            "Track 1 changed elsewhere stays"
        );
        assert_eq!(
            restored_mixers(0b1101_1111, None, six, 0),
            0b1111_1111,
            "a source added since gets OBS's default"
        );
        assert_eq!(
            restored_mixers(0b1101_1111, None, six, six),
            0b1101_1111,
            "still managed: a new source starts without it"
        );
        assert_eq!(
            restored_managed(six | track_bit(3), 0, six),
            track_bit(3),
            "a takeover from the panel stays"
        );
        assert_eq!(restored_managed(0, six, six), six, "managed at open stays");
    }

    #[test]
    fn a_managed_track_1_warns_about_streams_and_recordings() {
        let AudioView::Managed { note, .. } = audio_view(&model(1, Some(1)), 1, &[]) else {
            panic!()
        };
        assert_eq!(note.as_deref(), Some(TRACK_1_NOTE));
        let AudioView::Managed { note, .. } = audio_view(&model(6, Some(6)), 6, &[]) else {
            panic!()
        };
        assert_eq!(note, None);
    }

    #[test]
    fn taking_over_manages_only_that_track_and_remembers_the_others() {
        let mut m = model(3, None);
        assert!(!m.managed());
        assert!(m.take_over(3));
        assert!(m.managed());
        assert!(matches!(audio_view(&m, 3, &[]), AudioView::Managed { .. }));
        m.select_audio_track(4);
        assert!(!m.managed(), "a new track needs its own takeover");
        assert!(matches!(
            audio_view(&m, 4, &[]),
            AudioView::Unmanaged { track: 4, .. }
        ));
        m.take_over(4);
        m.select_audio_track(3);
        assert!(
            m.managed(),
            "Track 3 stays managed after taking over Track 4"
        );
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
            ..
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
                note: None,
            }
        );
    }
}
