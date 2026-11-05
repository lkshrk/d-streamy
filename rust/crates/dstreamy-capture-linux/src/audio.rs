use dstreamy_media::{AudioSource, AudioSourceKind, MediaError};
use gstreamer as gst;
use gstreamer::prelude::*;

/// Audio sources `pulsesrc` can capture, monitors of output devices first.
pub(crate) fn audio_sources() -> Result<Vec<AudioSource>, MediaError> {
    gst::init().map_err(|e| MediaError::Backend(format!("GStreamer init: {e}")))?;
    let Some(factory) = gst::DeviceProviderFactory::find("pulsedeviceprovider") else {
        return Ok(Vec::new());
    };
    let provider = factory.get().ok_or_else(|| {
        MediaError::Backend("cannot create the PulseAudio device provider".into())
    })?;
    let mut sources: Vec<AudioSource> = provider
        .devices()
        .iter()
        .filter(|d| d.device_class().starts_with("Audio/Source"))
        .filter_map(|device| {
            let element = device.create_element(None).ok()?;
            element.find_property("device")?;
            let id = element.property::<Option<String>>("device")?;
            let monitor = device
                .properties()
                .and_then(|p| p.get::<String>("device.class").ok())
                .is_some_and(|class| class == "monitor");
            Some(AudioSource {
                id,
                kind: if monitor {
                    AudioSourceKind::Output
                } else {
                    AudioSourceKind::Input
                },
                name: device.display_name().to_string(),
            })
        })
        .collect();
    sort(&mut sources);
    Ok(sources)
}

fn sort(sources: &mut [AudioSource]) {
    sources.sort_by(|a, b| {
        let rank = |s: &AudioSource| s.kind != AudioSourceKind::Output;
        rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name))
    });
}

/// Errors unless `id` is one of `sources`; an empty list means the devices are unknown.
pub(crate) fn check_known(id: &str, sources: &[AudioSource]) -> Result<(), MediaError> {
    // An empty device makes pulsesrc capture the default input, the microphone.
    if !id.trim().is_empty() && (sources.is_empty() || sources.iter().any(|s| s.id == id)) {
        return Ok(());
    }
    Err(MediaError::InvalidConfig(format!(
        "unknown audio source {id:?}"
    )))
}

/// Errors for an id that pipewire-pulse would read as a device index: it `atoi`s source names.
pub(crate) fn check_pulse_name(id: &str) -> Result<(), MediaError> {
    let rest = id.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let rest = rest.strip_prefix(['+', '-']).unwrap_or(rest);
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if rest[..digits].bytes().any(|b| b != b'0') {
        return Err(MediaError::InvalidConfig(format!(
            "audio source {id:?} starts with a number, which PulseAudio on PipeWire reads as a \
             device index, so another device could be captured; rename the device or pick \
             another from `dstreamy audio-sources`"
        )));
    }
    Ok(())
}

/// Errors for a server alias such as `@DEFAULT_SOURCE@`: it names whatever the default is, not one device.
pub(crate) fn check_not_alias(id: &str) -> Result<(), MediaError> {
    if id.len() > 1 && id.starts_with('@') && id.ends_with('@') {
        return Err(MediaError::InvalidConfig(format!(
            "audio source {id:?} stands for whatever the default is, not one device; leave out \
             --audio-source to capture the default output, or pick a device from \
             `dstreamy audio-sources`"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str, kind: AudioSourceKind, name: &str) -> AudioSource {
        AudioSource {
            id: id.into(),
            kind,
            name: name.into(),
        }
    }

    #[test]
    fn outputs_come_first_then_by_name() {
        let mut sources = vec![
            source("mic", AudioSourceKind::Input, "Mic"),
            source("b.monitor", AudioSourceKind::Output, "Monitor of B"),
            source("stream", AudioSourceKind::Input, "Alpha Stream"),
            source("a.monitor", AudioSourceKind::Output, "Monitor of A"),
        ];
        sort(&mut sources);
        let ids: Vec<&str> = sources.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["a.monitor", "b.monitor", "stream", "mic"]);
    }

    #[test]
    fn names_that_pipewire_pulse_reads_as_an_index_are_refused() {
        assert!(check_pulse_name("pipeweaver_stream").is_ok());
        assert!(check_pulse_name("0abc").is_ok());
        assert!(check_pulse_name("alsa_input.usb-2").is_ok());
        assert!(check_pulse_name("+0x").is_ok());
        for id in ["12abc", "3", "007x", "+5x", "-5x", " 5x", "\t\n7"] {
            assert!(
                matches!(check_pulse_name(id), Err(MediaError::InvalidConfig(_))),
                "{id}"
            );
        }
    }

    #[test]
    fn server_aliases_cannot_be_chosen_as_a_device() {
        for id in ["@DEFAULT_SOURCE@", "@DEFAULT_MONITOR@", "@x@"] {
            assert!(
                matches!(check_not_alias(id), Err(MediaError::InvalidConfig(_))),
                "{id}"
            );
        }
        for id in ["pipeweaver_stream", "@", "@half", "mail@host"] {
            assert!(check_not_alias(id).is_ok(), "{id}");
        }
    }

    #[test]
    fn only_listed_sources_are_accepted_when_the_list_is_known() {
        let sources = [source("stream", AudioSourceKind::Input, "Stream")];
        assert!(check_known("stream", &sources).is_ok());
        assert!(matches!(
            check_known("nope", &sources),
            Err(MediaError::InvalidConfig(_))
        ));
        assert!(check_known("anything", &[]).is_ok());
        assert!(check_known("", &[]).is_err());
        assert!(check_known(" ", &sources).is_err());
    }
}
