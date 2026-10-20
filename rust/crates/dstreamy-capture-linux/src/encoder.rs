use dstreamy_media::VideoCodec;

/// One GStreamer encoder element with the properties and caps it needs for low-latency CBR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderSpec {
    pub factory: &'static str,
    /// Applied in order; properties the installed element lacks are skipped.
    pub properties: Vec<(&'static str, String)>,
    /// Raw format the encoder is fed.
    pub input_format: &'static str,
    /// Caps forced between encoder and parser, e.g. the H.264 profile.
    pub output_caps: String,
}

/// Rate and GOP settings the encoder is configured with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderSettings {
    pub codec: VideoCodec,
    pub fps: u32,
    pub bitrate_bps: u32,
}

impl EncoderSettings {
    fn keyframe_interval(&self) -> String {
        self.fps.saturating_mul(2).to_string()
    }

    fn bitrate_kbps(&self) -> String {
        (self.bitrate_bps / 1000).max(1).to_string()
    }
}

const H264_ORDER: [&str; 4] = ["vah264enc", "vaapih264enc", "nvh264enc", "x264enc"];
const H265_ORDER: [&str; 4] = ["vah265enc", "vaapih265enc", "nvh265enc", "x265enc"];

/// Encoders to try for `settings.codec`, best first, limited to factories in `available`.
pub fn encoder_candidates<S: AsRef<str>>(
    settings: EncoderSettings,
    available: &[S],
) -> Vec<EncoderSpec> {
    let order = match settings.codec {
        VideoCodec::H264 => H264_ORDER,
        VideoCodec::H265 => H265_ORDER,
    };
    order
        .into_iter()
        .filter(|name| available.iter().any(|a| a.as_ref() == *name))
        .map(|name| encoder_spec(name, settings))
        .collect()
}

fn encoder_spec(factory: &'static str, s: EncoderSettings) -> EncoderSpec {
    let gop = s.keyframe_interval();
    let kbps = s.bitrate_kbps();
    let properties: Vec<(&'static str, String)> = match factory {
        "vah264enc" | "vah265enc" => vec![
            ("rate-control", "cbr".into()),
            ("bitrate", kbps),
            ("key-int-max", gop),
            ("b-frames", "0".into()),
            ("target-usage", "7".into()),
        ],
        "vaapih264enc" | "vaapih265enc" => vec![
            ("rate-control", "cbr".into()),
            ("bitrate", kbps),
            ("keyframe-period", gop),
            ("max-bframes", "0".into()),
            ("tune", "low-power".into()),
        ],
        "nvh264enc" | "nvh265enc" => vec![
            ("rc-mode", "cbr".into()),
            ("bitrate", kbps),
            ("gop-size", gop),
            ("bframes", "0".into()),
            ("zerolatency", "true".into()),
            ("preset", "low-latency-hq".into()),
        ],
        "x264enc" => vec![
            ("tune", "zerolatency".into()),
            ("speed-preset", "ultrafast".into()),
            ("pass", "cbr".into()),
            ("bitrate", kbps),
            ("key-int-max", gop),
            ("bframes", "0".into()),
        ],
        "x265enc" => vec![
            ("tune", "zerolatency".into()),
            ("speed-preset", "ultrafast".into()),
            ("bitrate", kbps),
            ("key-int-max", gop),
            (
                "option-string",
                format!(
                    "bframes=0:strict-cbr=1:vbv-maxrate={k}:vbv-bufsize={k}",
                    k = s.bitrate_kbps()
                ),
            ),
        ],
        other => unreachable!("no encoder spec for {other}"),
    };
    let output_caps = match s.codec {
        VideoCodec::H264 => "video/x-h264,profile=(string){constrained-baseline,baseline}".into(),
        VideoCodec::H265 => "video/x-h265,profile=(string)main".into(),
    };
    let input_format = match factory {
        "x264enc" | "x265enc" => "I420",
        _ => "NV12",
    };
    EncoderSpec {
        factory,
        properties,
        input_format,
        output_caps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(codec: VideoCodec) -> EncoderSettings {
        EncoderSettings {
            codec,
            fps: 30,
            bitrate_bps: 2_500_000,
        }
    }

    fn factories(specs: &[EncoderSpec]) -> Vec<&str> {
        specs.iter().map(|s| s.factory).collect()
    }

    #[test]
    fn h264_prefers_hardware_then_falls_back_to_x264() {
        let all = [
            "x264enc",
            "nvh264enc",
            "vaapih264enc",
            "vah264enc",
            "x265enc",
        ];
        assert_eq!(
            factories(&encoder_candidates(settings(VideoCodec::H264), &all)),
            ["vah264enc", "vaapih264enc", "nvh264enc", "x264enc"]
        );
        assert_eq!(
            factories(&encoder_candidates(
                settings(VideoCodec::H264),
                &["nvh264enc", "x264enc"]
            )),
            ["nvh264enc", "x264enc"]
        );
        assert_eq!(
            factories(&encoder_candidates(
                settings(VideoCodec::H264),
                &["x264enc"]
            )),
            ["x264enc"]
        );
    }

    #[test]
    fn h265_uses_h265_elements_only() {
        let all = ["x264enc", "vah264enc", "x265enc", "vaapih265enc"];
        assert_eq!(
            factories(&encoder_candidates(settings(VideoCodec::H265), &all)),
            ["vaapih265enc", "x265enc"]
        );
    }

    #[test]
    fn no_matching_encoder_yields_nothing() {
        assert!(encoder_candidates(settings(VideoCodec::H265), &["x264enc"]).is_empty());
        assert!(encoder_candidates::<&str>(settings(VideoCodec::H264), &[]).is_empty());
    }

    #[test]
    fn x264_is_cbr_zerolatency_baseline_with_two_second_gop() {
        let spec = &encoder_candidates(settings(VideoCodec::H264), &["x264enc"])[0];
        let prop = |k: &str| {
            spec.properties
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(prop("tune"), Some("zerolatency"));
        assert_eq!(prop("speed-preset"), Some("ultrafast"));
        assert_eq!(prop("key-int-max"), Some("60"));
        assert_eq!(prop("bframes"), Some("0"));
        assert_eq!(prop("bitrate"), Some("2500"));
        assert_eq!(prop("pass"), Some("cbr"));
        assert_eq!(spec.input_format, "I420");
        assert!(spec.output_caps.contains("baseline"));
    }

    #[test]
    fn hardware_encoders_take_nv12_and_cbr() {
        for spec in encoder_candidates(
            settings(VideoCodec::H264),
            &["vah264enc", "vaapih264enc", "nvh264enc"],
        ) {
            assert_eq!(spec.input_format, "NV12");
            assert!(
                spec.properties.iter().any(|(_, v)| v == "cbr"),
                "{} not CBR",
                spec.factory
            );
        }
    }
}
