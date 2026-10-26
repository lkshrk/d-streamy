//! The rules any encoder feeding D-Streamy must pass, over settings as libobs stores them (docs/specs.md, OBS).

use serde_json::{Map, Value};

/// Where OBS shows an encoder's streaming settings, in the pinned frontend's labels.
const STREAM_ENCODER_PATH: &str = "Settings › Output (Output Mode: Advanced) › Streaming";
const VIDEO_PATH: &str = "Settings › Video";

/// The H.264 encoder families at the pin, each with its own setting keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    X264,
    Nvenc,
    Vaapi,
    Amf,
    Qsv,
    VideoToolbox,
}

impl Family {
    pub fn of(id: &str) -> Option<Self> {
        Some(match id {
            "obs_x264" => Self::X264,
            "obs_nvenc_h264_tex"
            | "obs_nvenc_h264_soft"
            | "jim_nvenc"
            | "obs_nvenc_h264_cuda"
            | "ffmpeg_nvenc" => Self::Nvenc,
            "ffmpeg_vaapi" | "ffmpeg_vaapi_tex" => Self::Vaapi,
            "h264_texture_amf" | "h264_fallback_amf" => Self::Amf,
            "obs_qsv11" | "obs_qsv11_soft" | "obs_qsv11_v2" | "obs_qsv11_soft_v2" => Self::Qsv,
            _ if id.starts_with("com.apple.videotoolbox.") => Self::VideoToolbox,
            _ => return None,
        })
    }

    fn b_frames_label(self) -> &'static str {
        match self {
            Self::X264 => "Tune: zerolatency, or Profile: baseline",
            Self::Nvenc => "B-Frames: 0",
            Self::Vaapi | Self::Amf => "Max B-frames: 0",
            Self::Qsv => "B Frames: 0",
            Self::VideoToolbox => "Use B-Frames: off",
        }
    }

    fn keyint_label(self) -> &'static str {
        match self {
            Self::Nvenc => "Keyframe interval (seconds, 0 = auto)",
            _ => "Keyframe Interval (0=auto)",
        }
    }

    /// The free-form options field, whose contents can override any other key.
    fn custom_options(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::X264 => Some(("x264opts", "x264 Options (separated by space)")),
            Self::Nvenc => Some(("opts", "Custom Encoder Options")),
            Self::Vaapi => Some(("ffmpeg_opts", "FFmpeg Options")),
            Self::Amf => Some(("ffmpeg_opts", "AMF/FFmpeg Options")),
            Self::Qsv | Self::VideoToolbox => None,
        }
    }
}

/// An encoder's settings: every value including defaults, and the keys the user set.
#[derive(Debug, Clone)]
pub struct EncoderSettings {
    pub id: String,
    pub codec: String,
    values: Map<String, Value>,
    user: Map<String, Value>,
}

impl EncoderSettings {
    /// `with_defaults` and `user` are `obs_data_get_json_with_defaults` and `obs_data_get_json` of the same object.
    pub fn from_json(
        id: &str,
        codec: &str,
        with_defaults: &str,
        user: &str,
    ) -> Result<Self, String> {
        let object = |text: &str| match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(map)) => Ok(map),
            Ok(_) => Err(format!("{id} settings are not a JSON object")),
            Err(e) => Err(format!("{id} settings: {e}")),
        };
        Ok(Self::new(id, codec, object(with_defaults)?, object(user)?))
    }

    pub fn new(
        id: &str,
        codec: &str,
        values: Map<String, Value>,
        user: Map<String, Value>,
    ) -> Self {
        let mut values = values;
        for (key, value) in &user {
            values.insert(key.clone(), value.clone());
        }
        Self {
            id: id.to_owned(),
            codec: codec.to_owned(),
            values,
            user,
        }
    }

    fn int(&self, key: &str) -> i64 {
        self.values.get(key).and_then(Value::as_i64).unwrap_or(0)
    }

    fn bool(&self, key: &str) -> bool {
        self.values
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    fn string(&self, key: &str) -> &str {
        self.values.get(key).and_then(Value::as_str).unwrap_or("")
    }

    fn user_int(&self, key: &str) -> Option<i64> {
        self.user.get(key).and_then(Value::as_i64)
    }
}

/// One rule an encoder broke, with the setting to change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    NotH264 { codec: String },
    UnknownEncoder { id: String },
    BFrames { family: Family },
    KeyframeInterval { family: Family, seconds: i64 },
    CustomOptions { label: &'static str },
    Size { width: u32, height: u32 },
    Fps { num: u32, den: u32 },
    Audio { detail: String },
}

impl Problem {
    pub fn message(&self) -> String {
        match self {
            Self::NotH264 { codec } => {
                format!(
                    "D-Streamy needs an H.264 video encoder, not {codec}: {STREAM_ENCODER_PATH} › Video Encoder"
                )
            }
            Self::UnknownEncoder { id } => {
                format!(
                    "D-Streamy can't check the settings of the video encoder {id}: {STREAM_ENCODER_PATH} › Video Encoder"
                )
            }
            Self::BFrames { family } => format!(
                "D-Streamy needs B-frames off: {STREAM_ENCODER_PATH} › {}",
                family.b_frames_label()
            ),
            Self::KeyframeInterval { family, seconds } => format!(
                "D-Streamy needs a keyframe interval of 1 or 2 seconds, not {seconds}: {STREAM_ENCODER_PATH} › {}",
                family.keyint_label()
            ),
            Self::CustomOptions { label } => {
                format!("D-Streamy needs the custom options empty: {STREAM_ENCODER_PATH} › {label}")
            }
            Self::Size { width, height } => format!(
                "D-Streamy needs an output size of at most 1920×1080 with even sides, not {width}×{height}: {VIDEO_PATH} › Output (Scaled) Resolution"
            ),
            Self::Fps { num, den } => {
                let rate = if *den == 1 {
                    num.to_string()
                } else {
                    format!("{num}/{den}")
                };
                format!(
                    "D-Streamy needs 15, 30 or 60 FPS, not {rate}: {VIDEO_PATH} › Common FPS Values"
                )
            }
            Self::Audio { detail } => format!("D-Streamy's audio encoder {detail}"),
        }
    }
}

/// The user values D-Streamy gives its own encoder of `family`: B-frames off and a 2 s keyframe interval.
pub fn own_video_settings(family: Family, bitrate_kbps: i64) -> Value {
    let mut v = serde_json::json!({"bitrate": bitrate_kbps, "keyint_sec": 2});
    let map = v.as_object_mut().expect("an object");
    match family {
        Family::Qsv => {
            map.insert("bframes".into(), 0.into());
        }
        Family::VideoToolbox => {
            map.insert("bframes".into(), false.into());
        }
        Family::X264 | Family::Nvenc | Family::Vaapi | Family::Amf => {
            map.insert("bf".into(), 0.into());
        }
    }
    v
}

/// x264's B-frames in obs-x264's order: preset and tune, then a user-set `bf`, then the profile.
fn x264_b_frames(s: &EncoderSettings) -> i64 {
    let mut b = match s.string("preset").to_ascii_lowercase().as_str() {
        "ultrafast" => 0,
        "veryslow" => 8,
        "placebo" => 16,
        _ => 3,
    };
    for tune in s.string("tune").split([',', '.', '/', '-', '+']) {
        match tune.to_ascii_lowercase().as_str() {
            "animation" => b += 2,
            "zerolatency" => b = 0,
            _ => {}
        }
    }
    if let Some(bf) = s.user_int("bf") {
        b = bf;
    }
    if s.string("profile").eq_ignore_ascii_case("baseline") {
        b = 0;
    }
    b
}

fn b_frames(family: Family, s: &EncoderSettings) -> i64 {
    match family {
        Family::X264 => x264_b_frames(s),
        Family::Nvenc | Family::Vaapi | Family::Amf => s.int("bf"),
        Family::Qsv => s.user_int("bf").unwrap_or_else(|| s.int("bframes")),
        Family::VideoToolbox => i64::from(s.bool("bframes")),
    }
}

/// The video encoder's own settings: codec, B-frames, keyframe interval and custom options.
pub fn check_video_encoder(s: &EncoderSettings) -> Vec<Problem> {
    if s.codec != "h264" {
        return vec![Problem::NotH264 {
            codec: s.codec.clone(),
        }];
    }
    let Some(family) = Family::of(&s.id) else {
        return vec![Problem::UnknownEncoder { id: s.id.clone() }];
    };
    let mut problems = Vec::new();
    if let Some((key, label)) = family.custom_options()
        && !s.string(key).trim().is_empty()
    {
        problems.push(Problem::CustomOptions { label });
    }
    if b_frames(family, s) != 0 {
        problems.push(Problem::BFrames { family });
    }
    let seconds = s.int("keyint_sec");
    if !(1..=2).contains(&seconds) {
        problems.push(Problem::KeyframeInterval { family, seconds });
    }
    problems
}

/// What the encoder will produce: an even size within 1920×1080 at 15, 30 or 60 fps.
pub fn check_shape(width: u32, height: u32, fps_num: u32, fps_den: u32) -> Vec<Problem> {
    let mut problems = Vec::new();
    if width == 0
        || height == 0
        || width > 1920
        || height > 1080
        || width % 2 == 1
        || height % 2 == 1
    {
        problems.push(Problem::Size { width, height });
    }
    let whole = fps_den != 0 && fps_num.is_multiple_of(fps_den);
    if !whole || ![15, 30, 60].contains(&(fps_num / fps_den)) {
        problems.push(Problem::Fps {
            num: fps_num,
            den: fps_den,
        });
    }
    problems
}

pub fn check_audio(
    codec: &str,
    sample_rate: u32,
    channels: u32,
    frame_size: usize,
) -> Vec<Problem> {
    let mut problems = Vec::new();
    if codec != "opus" {
        problems.push(Problem::Audio {
            detail: format!("must be Opus, not {codec}"),
        });
    }
    if sample_rate != 48_000 {
        problems.push(Problem::Audio {
            detail: format!("must run at 48 kHz, not {sample_rate} Hz"),
        });
    }
    if channels != 2 {
        problems.push(Problem::Audio {
            detail: format!("must be stereo, not {channels} channels"),
        });
    }
    if frame_size != 960 {
        problems.push(Problem::Audio {
            detail: format!("must use 20 ms (960-sample) frames, not {frame_size} samples"),
        });
    }
    problems
}

/// The largest of 60, 30 and 15 not above a whole canvas rate that divides it evenly, and that divisor.
pub fn target_fps(canvas_num: u32, canvas_den: u32) -> Result<(u32, u32), Problem> {
    let too_odd = Problem::Fps {
        num: canvas_num,
        den: canvas_den,
    };
    if canvas_den == 0 || !canvas_num.is_multiple_of(canvas_den) {
        return Err(too_odd);
    }
    let canvas = canvas_num / canvas_den;
    [60, 30, 15]
        .into_iter()
        .find(|&fps| fps <= canvas && canvas.is_multiple_of(fps))
        .map(|fps| (fps, canvas / fps))
        .ok_or(too_odd)
}

/// The canvas scaled to fit within 1920×1080, keeping its aspect ratio, with even sides.
pub fn fit_1080p(width: u32, height: u32) -> (u32, u32) {
    let even = |v: u64| u32::try_from(v & !1).unwrap_or(u32::MAX);
    let (w, h) = (u64::from(width), u64::from(height));
    if w <= 1920 && h <= 1080 {
        return (even(w), even(h));
    }
    if w * 1080 >= h * 1920 {
        (1920, even(h * 1920 / w))
    } else {
        (even(w * 1080 / h), 1080)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn settings(id: &str, values: Value, user: Value) -> EncoderSettings {
        let (Value::Object(values), Value::Object(user)) = (values, user) else {
            panic!("objects")
        };
        EncoderSettings::new(id, "h264", values, user)
    }

    #[test]
    fn x264_b_frames_follow_obs_x264s_order() {
        let defaults = json!({"preset": "veryfast", "tune": "", "profile": "", "keyint_sec": 2, "x264opts": ""});
        let case = |user: Value| check_video_encoder(&settings("obs_x264", defaults.clone(), user));
        let b = || Problem::BFrames {
            family: Family::X264,
        };
        assert_eq!(
            case(json!({})),
            [b()],
            "veryfast with bf unset has B-frames"
        );
        assert!(case(json!({"bf": 0})).is_empty());
        assert_eq!(case(json!({"bf": 2})), [b()]);
        assert!(case(json!({"tune": "zerolatency"})).is_empty());
        assert!(case(json!({"tune": "film,zerolatency"})).is_empty());
        assert_eq!(
            case(json!({"tune": "zerolatency", "bf": 2})),
            [b()],
            "bf comes after the tune"
        );
        assert!(
            case(json!({"profile": "baseline", "bf": 2})).is_empty(),
            "the profile comes last"
        );
        assert!(case(json!({"preset": "ultrafast"})).is_empty());
        assert_eq!(
            case(json!({"preset": "ultrafast", "tune": "animation"})),
            [b()]
        );
    }

    #[test]
    fn custom_options_must_be_empty_whatever_they_say() {
        let defaults = json!({"preset": "veryfast", "keyint_sec": 2, "x264opts": ""});
        let problems = check_video_encoder(&settings(
            "obs_x264",
            defaults,
            json!({"bf": 0, "x264opts": "bframes=0"}),
        ));
        assert_eq!(
            problems,
            [Problem::CustomOptions {
                label: "x264 Options (separated by space)"
            }]
        );
    }

    #[test]
    fn keyframe_interval_must_be_one_or_two_seconds() {
        for (seconds, ok) in [(0, false), (1, true), (2, true), (3, false)] {
            let p = check_video_encoder(&settings(
                "ffmpeg_vaapi",
                json!({"bf": 0, "keyint_sec": 0, "ffmpeg_opts": ""}),
                json!({"keyint_sec": seconds}),
            ));
            assert_eq!(p.is_empty(), ok, "keyint_sec {seconds}: {p:?}");
        }
    }

    #[test]
    fn qsv_takes_bf_over_bframes_only_when_user_set() {
        let defaults = json!({"bframes": 3, "keyint_sec": 2});
        let b = |user: Value| {
            !check_video_encoder(&settings("obs_qsv11_v2", defaults.clone(), user)).is_empty()
        };
        assert!(b(json!({})));
        assert!(!b(json!({"bf": 0})));
        assert!(!b(json!({"bframes": 0})));
        assert!(b(json!({"bframes": 0, "bf": 2})));
    }

    #[test]
    fn videotoolbox_b_frames_are_a_switch() {
        let id = "com.apple.videotoolbox.videoencoder.ave.avc";
        let defaults = json!({"bframes": true, "keyint_sec": 2});
        assert!(!check_video_encoder(&settings(id, defaults.clone(), json!({}))).is_empty());
        assert!(check_video_encoder(&settings(id, defaults, json!({"bframes": false}))).is_empty());
    }

    #[test]
    fn unknown_and_non_h264_encoders_are_refused() {
        let s = EncoderSettings::new("ffmpeg_aac", "aac", Map::new(), Map::new());
        assert_eq!(
            check_video_encoder(&s),
            [Problem::NotH264 {
                codec: "aac".into()
            }]
        );
        let s = EncoderSettings::new("some_plugin_h264", "h264", Map::new(), Map::new());
        assert!(matches!(
            check_video_encoder(&s)[..],
            [Problem::UnknownEncoder { .. }]
        ));
    }

    #[test]
    fn own_settings_pass_for_every_family() {
        for id in [
            "obs_x264",
            "obs_nvenc_h264_tex",
            "ffmpeg_vaapi",
            "h264_texture_amf",
            "obs_qsv11_v2",
            "com.apple.videotoolbox.videoencoder.ave.avc",
        ] {
            let family = Family::of(id).unwrap();
            let defaults = json!({"bf": 2, "bframes": true, "keyint_sec": 0, "preset": "veryfast"});
            let qsv_defaults = json!({"bframes": 3, "keyint_sec": 0});
            let defaults = if family == Family::Qsv {
                qsv_defaults
            } else {
                defaults
            };
            let s = settings(id, defaults, own_video_settings(family, 6000));
            assert!(
                check_video_encoder(&s).is_empty(),
                "{id}: {:?}",
                check_video_encoder(&s)
            );
        }
    }

    #[test]
    fn shape_and_fps() {
        assert!(check_shape(1920, 1080, 60, 1).is_empty());
        assert!(check_shape(1280, 720, 30, 1).is_empty());
        assert_eq!(check_shape(2560, 1440, 30, 1).len(), 1);
        assert_eq!(check_shape(1279, 720, 30, 1).len(), 1);
        assert_eq!(check_shape(1280, 720, 24, 1).len(), 1);
        assert_eq!(check_shape(1280, 720, 30000, 1001).len(), 1);
    }

    #[test]
    fn target_fps_divides_the_canvas_rate() {
        assert_eq!(target_fps(60, 1), Ok((60, 1)));
        assert_eq!(target_fps(30, 1), Ok((30, 1)));
        assert_eq!(target_fps(120, 1), Ok((60, 2)));
        assert_eq!(target_fps(90, 1), Ok((30, 3)));
        assert_eq!(target_fps(45, 1), Ok((15, 3)));
        for (num, den) in [(24, 1), (25, 1), (50, 1), (30000, 1001), (10, 1)] {
            assert!(target_fps(num, den).is_err(), "{num}/{den}");
        }
    }

    #[test]
    fn fit_1080p_keeps_the_aspect_and_even_sides() {
        assert_eq!(fit_1080p(1280, 720), (1280, 720));
        assert_eq!(fit_1080p(1921, 1080), (1920, 1078));
        assert_eq!(fit_1080p(2560, 1440), (1920, 1080));
        assert_eq!(fit_1080p(3440, 1440), (1920, 802));
        assert_eq!(fit_1080p(1080, 1920), (606, 1080));
        assert_eq!(fit_1080p(1279, 719), (1278, 718));
    }

    #[test]
    fn audio_must_be_20_ms_stereo_opus_at_48_khz() {
        assert!(check_audio("opus", 48_000, 2, 960).is_empty());
        assert_eq!(check_audio("aac", 48_000, 2, 1024).len(), 2);
        assert_eq!(check_audio("opus", 44_100, 1, 960).len(), 2);
    }

    #[test]
    fn messages_name_the_setting() {
        let m = Problem::BFrames {
            family: Family::Nvenc,
        }
        .message();
        assert_eq!(
            m,
            "D-Streamy needs B-frames off: Settings › Output (Output Mode: Advanced) › Streaming › B-Frames: 0"
        );
        assert!(
            Problem::Fps { num: 24, den: 1 }
                .message()
                .ends_with("not 24: Settings › Video › Common FPS Values")
        );
    }
}
