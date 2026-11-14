use dstreamy_media::{MediaError, VideoCodec};
use gstreamer as gst;
use gstreamer::prelude::*;

/// The GStreamer package that ships `factory`, by its upstream module name.
pub(crate) fn package_of(factory: &str) -> Option<&'static str> {
    Some(match factory {
        "pipewiresrc" => "gst-plugin-pipewire (part of PipeWire)",
        "pulsesrc" | "videocrop" => "gst-plugins-good",
        "videoconvert" | "videoscale" | "videorate" | "queue" | "capsfilter" | "appsink"
        | "audioconvert" | "audioresample" | "volume" => "gst-plugins-base",
        "x264enc" => "gst-plugins-ugly",
        "x265enc" | "h264parse" | "h265parse" => "gst-plugins-bad",
        f if f.starts_with("vaapi") => "gstreamer-vaapi",
        f if f.starts_with("va") => "gst-plugins-bad with its VA plugin (gst-plugin-va)",
        f if f.starts_with("nv") => "gst-plugins-bad with its nvcodec plugin and the NVIDIA driver",
        _ => return None,
    })
}

pub(crate) fn no_encoder(codec: VideoCodec) -> MediaError {
    let software = match codec {
        VideoCodec::H264 => "gst-plugins-ugly (x264enc)",
        VideoCodec::H265 => "gst-plugins-bad (x265enc)",
    };
    MediaError::Unsupported(format!(
        "no GStreamer encoder for {codec:?}; install {software} for software encoding, \
         or gst-plugins-bad with its VA plugin (Intel/AMD) or nvcodec plugin (NVIDIA) for hardware encoding"
    ))
}

pub(super) fn make(factory: &str) -> Result<gst::Element, MediaError> {
    gst::ElementFactory::make(factory).build().map_err(|e| {
        let install = package_of(factory).map_or_else(String::new, |p| format!("; install {p}"));
        MediaError::Backend(format!("cannot create {factory}: {e}{install}"))
    })
}

pub(super) fn capsfilter(caps: gst::Caps) -> Result<gst::Element, MediaError> {
    gst::ElementFactory::make("capsfilter")
        .property("caps", caps)
        .build()
        .map_err(|e| MediaError::Backend(format!("cannot create capsfilter: {e}")))
}

/// Sets a property from its string form; skips it if this element version lacks or rejects it.
pub(super) fn set_if_present(element: &gst::Element, name: &str, value: &str) -> bool {
    let Some(pspec) = element.find_property(name) else {
        return false;
    };
    if !pspec.flags().contains(glib::ParamFlags::WRITABLE) {
        return false;
    }
    match glib::Value::deserialize(value, pspec.value_type()) {
        Ok(v) if pspec.value_is_valid(&v) => {
            element.set_property_from_value(name, &v);
            true
        }
        _ => false,
    }
}

pub(super) fn link_chain(
    bin: &impl IsA<gst::Bin>,
    chain: &[gst::Element],
) -> Result<(), MediaError> {
    bin.add_many(chain)
        .map_err(|e| MediaError::Backend(format!("cannot add elements: {e}")))?;
    gst::Element::link_many(chain)
        .map_err(|e| MediaError::Backend(format!("cannot link pipeline: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::test_support::init;

    #[test]
    fn unknown_or_invalid_encoder_properties_are_skipped() {
        init();
        let enc = make("x264enc").unwrap();
        assert!(!set_if_present(&enc, "no-such-property", "1"));
        assert!(!set_if_present(&enc, "pass", "not-a-mode"));
        assert!(!set_if_present(&enc, "bframes", "9999"));
        assert!(set_if_present(&enc, "pass", "cbr"));
        assert!(set_if_present(&enc, "key-int-max", "60"));
        assert_eq!(enc.property::<u32>("key-int-max"), 60);
    }

    #[test]
    fn missing_elements_and_encoders_say_what_to_install() {
        init();
        assert_eq!(package_of("vah264lpenc"), package_of("vah264enc"));
        assert_ne!(package_of("vah264enc"), package_of("vaapih264enc"));
        assert_eq!(package_of("dstreamy-missing"), None);
        let err = make("dstreamy-missing-enc").unwrap_err().to_string();
        assert!(!err.contains("install"), "{err}");
        assert!(package_of("pipewiresrc").unwrap().contains("PipeWire"));
        let err = no_encoder(VideoCodec::H264).to_string();
        assert!(err.contains("x264enc") && err.contains("VA"), "{err}");
    }
}
