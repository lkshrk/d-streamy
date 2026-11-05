//! The Discord track's audio sources: which OBS sources feed it, and what the settings dialog shows about them.

use crate::dock::{AUDIO_TRACKS, DEFAULT_AUDIO_TRACK, DockModel, next_go_live, track_name};

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
        /// A better track to offer instead of taking this one over.
        instead: Option<u8>,
    },
    Managed {
        track: u8,
        help: String,
        /// Sorted by name; empty when OBS has no audio source yet.
        sources: Vec<SourceRow>,
        empty: Option<String>,
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
    format!("Manage {}", track_name(track))
}

pub fn release_label(track: u8) -> String {
    format!("Stop managing {}", track_name(track))
}

pub fn instead_label(track: u8) -> String {
    format!("Use {} instead", track_name(track))
}

/// What a takeover does; `live`: Discord hears `track` now.
pub fn take_over_effect(track: u8, live: bool) -> String {
    let t = track_name(track);
    let silent = "Discord goes silent until you tick what it hears.";
    if track != 1 {
        let effect = format!(
            "Turns {t} off on every audio source, also for recordings and streams that use it."
        );
        return if live {
            format!("{effect} {silent}")
        } else {
            effect
        };
    }
    let cost = format!(
        "{t} is OBS's streaming and recording track: managing it turns it off on every audio source, \
         so your stream and recordings lose their audio"
    );
    if live {
        format!("{cost}, and {silent}")
    } else {
        format!(
            "{cost}. Use {} for Discord instead.",
            track_name(DEFAULT_AUDIO_TRACK)
        )
    }
}

pub const CONFIRM: &str = "Manage";
pub const NOT_NOW: &str = "Not now";
/// Where the mic check is explained: what it looks at, and what it can't see.
pub const MIC_CHECK: &str = "D-Streamy checks microphones and Mic/Aux sources, not video capture devices \
     such as webcams and capture cards.";

/// Names in one line: "Mic", "Mic and Headset", "Mic, Headset and Line In".
fn names(list: &[String]) -> String {
    match list {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Like [`names`], but at most three, then how many more.
fn some_names(list: &[String]) -> String {
    match list {
        [first, second, third, rest @ ..] if !rest.is_empty() => {
            format!("{first}, {second}, {third} and {} more", rest.len())
        }
        _ => names(list),
    }
}

/// "Discord hears … on Track 6 (live)" while live, else "Discord would hear … on Track 6".
fn hears(track: u8, live: bool, what: &str) -> String {
    let t = track_name(track);
    if live {
        format!("Discord hears {what} on {t} (live)")
    } else {
        format!("Discord would hear {what} on {t}")
    }
}

/// What Discord hears from a track D-Streamy doesn't manage.
pub fn unmanaged_line(track: u8, live: bool, sources: &[AudioSource]) -> String {
    let mics = mics_on(track, sources);
    let heard = hears(track, live, "every source");
    let heard = if mics.is_empty() {
        format!("{heard}.")
    } else {
        format!("{heard}, including {}.", names(&mics))
    };
    format!("{heard} {} to pick sources here.", take_over_label(track))
}

/// The managed list's line; `next`: the pick that applies on the next Go Live, when it differs.
pub fn managed_line(track: u8, live: bool, next: Option<u8>) -> String {
    let t = track_name(track);
    let mut line = format!(
        "{}. Ticking changes {t} in Edit › Advanced Audio Properties at once.",
        hears(track, live, "the ticked sources")
    );
    if let Some(next) = next {
        line.push_str(&format!(" {}.", next_go_live(next)));
    }
    line
}

/// A managed track that isn't the one Discord gets, and how to let it go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    /// The track "Stop managing" lets go; `None` while Discord gets it.
    pub release: Option<u8>,
}

/// Warnings about managed tracks Discord doesn't get and about Track 1, then the follow-ups of earlier actions.
pub fn notices(model: &DockModel, sources: &[AudioSource]) -> Vec<Notice> {
    let six = track_name(DEFAULT_AUDIO_TRACK);
    let back = "then turn it back on in Edit › Advanced Audio Properties where needed.";
    let mut shown: Vec<Notice> = AUDIO_TRACKS
        .filter(|&t| model.manages(t) && (t == 1 || !model.gets(t)))
        .map(|track| {
            let t = track_name(track);
            let next = model.settings().audio_track;
            let text = match track {
                1 if model.gets(1) => {
                    let then = match (model.live_track() == Some(1), next) {
                        (false, _) => format!("Pick {six} for Discord, manage it, then stop managing {t}."),
                        (true, 1) => format!(
                            "Pick {six} for the next Go Live now; after this stream, manage it and stop managing {t}."
                        ),
                        (true, n) if model.manages(n) => format!("After this stream, stop managing {t} here."),
                        (true, n) => format!(
                            "After this stream, manage {} and stop managing {t}.",
                            track_name(n)
                        ),
                    };
                    format!(
                        "{t} is OBS's streaming and recording track, and D-Streamy manages it for Discord, so new \
                         sources start without it. {then}"
                    )
                }
                1 => format!(
                    "{t} is OBS's streaming and recording track, and D-Streamy still manages it, so new sources \
                     start without it. Stop managing {t}, {back}"
                ),
                _ => format!(
                    "D-Streamy also manages {t}, which Discord doesn't get, so new sources start without it. Stop \
                     managing it, {back}"
                ),
            };
            Notice {
                text,
                release: (!model.gets(track)).then_some(track),
            }
        })
        .collect();
    for track in
        AUDIO_TRACKS.filter(|&t| model.released_mixers() & track_bit(t) != 0 && !model.manages(t))
    {
        let off = sources_off(track, sources);
        if off.is_empty() {
            continue;
        }
        let (t, who) = (track_name(track), some_names(&off));
        let text = if track == 1 {
            format!(
                "D-Streamy no longer manages {t}, but it is still off on {who}, so your stream and recordings miss \
                 them. Turn {t} back on for them in Edit › Advanced Audio Properties."
            )
        } else {
            format!(
                "D-Streamy no longer manages {t}. It is still off on {who}: turn it back on in Edit › Advanced \
                 Audio Properties for those that need it."
            )
        };
        shown.push(Notice {
            text,
            release: None,
        });
    }
    for track in AUDIO_TRACKS.filter(|&t| model.unrestored_mixers() & track_bit(t) != 0) {
        let t = track_name(track);
        shown.push(Notice {
            text: format!(
                "D-Streamy couldn't undo the settings dialog's changes to {t}. Check {t} in Edit › Advanced Audio \
                 Properties."
            ),
            release: None,
        });
    }
    shown
}

pub const NO_SOURCES: &str = "OBS has no audio source yet. Add one, and it shows up here.";

/// The names of microphone-type sources with `track` on, sorted.
fn mics_on(track: u8, sources: &[AudioSource]) -> Vec<String> {
    sorted_names(
        sources
            .iter()
            .filter(|s| s.mic && s.mixers & track_bit(track) != 0),
    )
}

/// The names of sources with `track` off, sorted.
fn sources_off(track: u8, sources: &[AudioSource]) -> Vec<String> {
    sorted_names(sources.iter().filter(|s| s.mixers & track_bit(track) == 0))
}

fn sorted_names<'a>(found: impl Iterator<Item = &'a AudioSource>) -> Vec<String> {
    let mut mics: Vec<&AudioSource> = found.collect();
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

    /// The fix, then what the check can't see.
    pub fn tooltip(&self) -> String {
        format!("{} {MIC_CHECK}", self.message())
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
            "{who} {verb} on {t}, so {hears}. Manage {t}, \
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
    let live = model.live_track() == Some(track);
    if !model.manages(track) {
        return AudioView::Unmanaged {
            track,
            help: unmanaged_line(track, live, sources),
            take_over: take_over_label(track),
            effect: take_over_effect(track, live),
            instead: (track == 1 && !live).then_some(DEFAULT_AUDIO_TRACK),
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
    let next = Some(model.settings().audio_track).filter(|&n| live && n != track);
    AudioView::Managed {
        track,
        help: managed_line(track, live, next),
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
            instead,
        } = v
        else {
            panic!("{v:?}")
        };
        assert_eq!(track, 6);
        assert_eq!(take_over, "Manage Track 6");
        assert_eq!(instead, None);
        assert_eq!(
            help,
            "Discord would hear every source on Track 6. Manage Track 6 to pick sources here."
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
            mic("off", "Line In", 0b01_1111),
            source("a", "Music", 0b11_1111),
        ];
        let AudioView::Unmanaged { help, .. } = audio_view(&model(6, None), 6, &sources) else {
            panic!()
        };
        assert_eq!(
            help,
            "Discord would hear every source on Track 6, including Headset and Mic/Aux. \
             Manage Track 6 to pick sources here."
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
        assert_eq!(guard.fix(), "Manage Track 6");
        assert_eq!(
            guard.message(),
            "Mic/Aux is on Track 6, so Discord would hear it. Manage Track 6, \
             or turn Track 6 off for Mic/Aux in Edit › Advanced Audio Properties."
        );
        assert_eq!(
            guard.tooltip(),
            format!("{} {MIC_CHECK}", guard.message()),
            "says what the check can't see"
        );
        let two = [mic("m", "Mic", 0b10_0000), mic("h", "Headset", 0b10_0000)];
        let both = mic_guard(&model(6, None), &two).unwrap();
        assert_eq!(both.headline(), "Discord would hear Headset and Mic");
        assert_eq!(
            both.message(),
            "Headset and Mic are on Track 6, so Discord would hear them. Manage Track 6, \
             or turn Track 6 off for Headset and Mic in Edit › Advanced Audio Properties."
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
            "Mic/Aux is on Track 6, so Discord hears it now. Manage Track 6, \
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
    fn track_1_stays_a_warning_with_a_way_out_whatever_discord_gets() {
        let mut m = model(6, Some(1));
        m.take_over(6);
        let shown = notices(&m, &[]);
        assert_eq!(shown.len(), 1, "{shown:?}");
        assert_eq!(shown[0].release, Some(1));
        assert!(
            shown[0].text.starts_with(
                "Track 1 is OBS's streaming and recording track, and D-Streamy still manages it"
            ),
            "{}",
            shown[0].text
        );
        assert!(shown[0].text.contains("Edit › Advanced Audio Properties"));
        assert!(
            !shown[0].text.contains("turned it off"),
            "true after an adoption too"
        );

        let on_1 = model(1, Some(1));
        let shown = notices(&on_1, &[]);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].release, None, "Discord gets Track 1");
        assert_eq!(
            shown[0].text,
            "Track 1 is OBS's streaming and recording track, and D-Streamy manages it for Discord, so new \
             sources start without it. Pick Track 6 for Discord, manage it, then stop managing Track 1."
        );

        let mut other = model(6, Some(6));
        other.take_over(3);
        let shown = notices(&other, &[]);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].release, Some(3));
        assert_eq!(
            shown[0].text,
            "D-Streamy also manages Track 3, which Discord doesn't get, so new sources start without it. Stop \
             managing it, then turn it back on in Edit › Advanced Audio Properties where needed."
        );
        assert!(notices(&model(6, Some(6)), &[]).is_empty());
        assert_eq!(release_label(3), "Stop managing Track 3");
    }

    #[test]
    fn taking_over_track_1_or_the_live_track_says_what_it_costs() {
        let AudioView::Unmanaged {
            effect, instead, ..
        } = audio_view(&model(1, None), 1, &[])
        else {
            panic!()
        };
        assert_eq!(
            effect,
            "Track 1 is OBS's streaming and recording track: managing it turns it off on every audio source, \
             so your stream and recordings lose their audio. Use Track 6 for Discord instead."
        );
        assert_eq!(instead, Some(6));
        assert_eq!(instead_label(6), "Use Track 6 instead");
        let live = live_on(picked(6, None));
        let AudioView::Unmanaged { effect, help, .. } = audio_view(&live, 6, &[]) else {
            panic!()
        };
        assert!(
            effect.ends_with("Discord goes silent until you tick what it hears."),
            "{effect}"
        );
        assert!(
            help.starts_with("Discord hears every source on Track 6 (live)."),
            "{help}"
        );
    }

    #[test]
    fn while_live_track_1_offers_no_other_track_and_says_what_waits_for_the_stream_to_end() {
        let mut on_six = live_on(picked(6, None));
        on_six.select_audio_track(1);
        let AudioView::Unmanaged {
            effect, instead, ..
        } = audio_view(&on_six, 1, &[])
        else {
            panic!()
        };
        assert_eq!(
            instead,
            Some(6),
            "live on Track 6, the pick for the next Go Live can still change"
        );
        assert!(
            effect.ends_with("Use Track 6 for Discord instead."),
            "{effect}"
        );
        let live = live_on(picked(1, None));
        let AudioView::Unmanaged {
            effect, instead, ..
        } = audio_view(&live, 1, &[])
        else {
            panic!()
        };
        assert_eq!(instead, None, "a pick would only change the next Go Live");
        assert_eq!(
            effect,
            "Track 1 is OBS's streaming and recording track: managing it turns it off on every audio source, \
             so your stream and recordings lose their audio, and Discord goes silent until you tick what it hears."
        );
        let mut managed = live_on(picked(1, Some(1)));
        let text = |m: &DockModel| notices(m, &[]).remove(0);
        let shown = text(&managed);
        assert_eq!(shown.release, None);
        assert!(
            shown
                .text
                .ends_with("Pick Track 6 for the next Go Live now; after this stream, manage it and stop managing Track 1."),
            "{}",
            shown.text
        );
        managed.select_audio_track(6);
        let shown = text(&managed);
        assert_eq!(shown.release, None, "Discord still hears Track 1");
        assert!(
            shown
                .text
                .ends_with("After this stream, manage Track 6 and stop managing Track 1."),
            "{}",
            shown.text
        );
        managed.take_over(6);
        assert!(
            text(&managed)
                .text
                .ends_with("After this stream, stop managing Track 1 here."),
            "{}",
            text(&managed).text
        );
        assert!(!managed.release(1));
    }

    #[test]
    fn while_live_the_next_pick_is_not_offered_for_release() {
        let mut m = live_on(picked(6, Some(6)));
        m.select_audio_track(3);
        m.take_over(3);
        assert!(notices(&m, &[]).is_empty(), "the next Go Live uses Track 3");
        assert!(!m.release(3), "the next Go Live's track stays managed");
        assert!(!m.release(6), "the live track stays managed");
        m.take_over(4);
        let shown = notices(&m, &[]);
        assert_eq!(shown.len(), 1, "{shown:?}");
        assert_eq!(shown[0].release, Some(4));
        assert!(m.release(4));
        m.select_audio_track(5);
        assert_eq!(
            notices(&m, &[])
                .iter()
                .map(|n| n.release)
                .collect::<Vec<_>>(),
            [Some(3)],
            "Track 3 is no longer anyone's pick"
        );
    }

    #[test]
    fn a_released_track_keeps_its_way_back_until_it_is_on_again_or_forgotten() {
        let mut m = model(6, Some(6));
        m.take_over(1);
        let off = [
            source("m", "Mic", 0b11_1110),
            source("a", "Music", 0b11_1110),
            source("d", "Desktop", 0b11_1111),
        ];
        assert!(m.release(1));
        let shown = notices(&m, &off);
        assert_eq!(shown.len(), 1, "{shown:?}");
        assert_eq!(shown[0].release, None);
        assert_eq!(
            shown[0].text,
            "D-Streamy no longer manages Track 1, but it is still off on Mic and Music, so your stream and \
             recordings miss them. Turn Track 1 back on for them in Edit › Advanced Audio Properties."
        );
        assert!(
            notices(&m, &[source("d", "Desktop", 0b11_1111)]).is_empty(),
            "nothing has it off any more"
        );
        let many: Vec<AudioSource> = (0..5)
            .map(|i| source(&format!("{i}"), &format!("S{i}"), 0))
            .collect();
        assert!(
            notices(&m, &many)[0]
                .text
                .contains("still off on S0, S1, S2 and 2 more,"),
            "{:?}",
            notices(&m, &many)
        );
        m.forget_released();
        assert_eq!(
            notices(&m, &off).len(),
            1,
            "the stream still misses them after the views close"
        );
        m.take_over(1);
        assert!(
            notices(&m, &off)
                .iter()
                .all(|n| !n.text.contains("no longer"))
        );

        let mut m = model(6, Some(6));
        m.take_over(3);
        m.release(3);
        assert!(
            notices(&m, &[source("m", "Mic", 0b11_1011)])[0]
                .text
                .starts_with(
                    "D-Streamy no longer manages Track 3. It is still off on Mic: turn it back on"
                ),
        );
        m.take_over(3);
        assert!(
            notices(&m, &[source("m", "Mic", 0b11_1011)])
                .iter()
                .all(|n| n.release == Some(3)),
            "managed again"
        );
        m.release(3);
        m.forget_released();
        assert!(
            notices(&m, &[source("m", "Mic", 0b11_1011)]).is_empty(),
            "other tracks' follow-ups end with the views that showed them"
        );
        m.take_over(3);
        m.release(3);
        m.restore_managed(track_bit(6) | track_bit(3));
        assert_eq!(m.released_mixers(), 0, "a Cancel brought it back");
        m.release(3);
        m.collection_loaded(track_bit(6));
        assert_eq!(m.released_mixers(), 0, "another scene collection");
    }

    #[test]
    fn a_cancel_that_could_not_undo_says_which_track_to_check() {
        let mut m = model(6, Some(6));
        m.restore_failed(track_bit(6));
        let shown = notices(&m, &[]);
        assert_eq!(
            shown,
            [Notice {
                text: "D-Streamy couldn't undo the settings dialog's changes to Track 6. Check Track 6 in Edit › \
                       Advanced Audio Properties."
                    .into(),
                release: None,
            }]
        );
        m.forget_unrestored();
        assert!(notices(&m, &[]).is_empty());
        m.restore_failed(track_bit(3));
        m.take_over(3);
        assert_eq!(m.unrestored_mixers(), 0, "taken over again");
    }

    #[test]
    fn the_mic_check_names_what_it_cannot_see() {
        let guard = mic_guard(&model(6, None), &[mic("m", "Mic", 0b11_1111)]).unwrap();
        assert!(
            guard.tooltip().ends_with(
                "D-Streamy checks microphones and Mic/Aux sources, not video capture devices such as webcams \
                 and capture cards."
            ),
            "{}",
            guard.tooltip()
        );
    }

    #[test]
    fn the_managed_list_names_its_track_and_the_next_one() {
        let mut live = live_on(picked(6, Some(6)));
        let AudioView::Managed { help, .. } = audio_view(&live, 6, &[]) else {
            panic!()
        };
        assert!(
            help.starts_with("Discord hears the ticked sources on Track 6 (live)."),
            "{help}"
        );
        live.select_audio_track(3);
        let AudioView::Managed { help, .. } = audio_view(&live, 6, &[]) else {
            panic!()
        };
        assert!(
            help.ends_with("at once. Discord hears Track 3 from the next Go Live."),
            "{help}"
        );
        let AudioView::Managed { help, .. } = audio_view(&model(6, Some(6)), 6, &[]) else {
            panic!()
        };
        assert_eq!(
            help,
            "Discord would hear the ticked sources on Track 6. Ticking changes Track 6 in \
             Edit › Advanced Audio Properties at once."
        );
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
            help.starts_with("Discord would hear the ticked sources on Track 6."),
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
                help: managed_line(2, false, None),
                sources: vec![],
                empty: Some(NO_SOURCES.to_owned()),
            }
        );
    }
}
