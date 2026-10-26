//! The settings rules over every H.264 encoder at the OBS pin, from obs/tests/encoder-settings (captured or source-derived).

use std::path::{Path, PathBuf};

use dstreamy_obs_producer::settings::{
    EncoderSettings, Family, Problem, check_video_encoder, own_video_settings,
};
use serde_json::{Map, Value, json};

struct Fixture {
    id: String,
    family: Family,
    defaults: Map<String, Value>,
    properties: Vec<String>,
}

fn fixtures() -> Vec<Fixture> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../obs/tests/encoder-settings");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    assert!(
        paths.len() >= 15,
        "{} fixtures in {}",
        paths.len(),
        dir.display()
    );
    paths
        .iter()
        .map(|path| {
            let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            let id = v["id"].as_str().unwrap().to_owned();
            assert_eq!(v["codec"], "h264", "{id}");
            assert!(
                matches!(v["kind"].as_str(), Some("source" | "captured")),
                "{id}: kind must say whether the row was captured or derived from source"
            );
            assert!(
                v["source"].as_str().is_some_and(|s| !s.is_empty()),
                "{id}: no source"
            );
            Fixture {
                family: Family::of(&id).unwrap_or_else(|| panic!("{id}: no family")),
                defaults: v["defaults"].as_object().unwrap().clone(),
                properties: v["properties"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| p["name"].as_str().unwrap().to_owned())
                    .collect(),
                id,
            }
        })
        .collect()
}

fn check(f: &Fixture, user: Value) -> Vec<Problem> {
    let Value::Object(user) = user else {
        panic!("an object")
    };
    check_video_encoder(&EncoderSettings::new(
        &f.id,
        "h264",
        f.defaults.clone(),
        user,
    ))
}

fn b_frames_off(family: Family) -> Value {
    match family {
        Family::X264 => json!({"tune": "zerolatency"}),
        Family::Qsv => json!({"bframes": 0}),
        Family::VideoToolbox => json!({"bframes": false}),
        Family::Nvenc | Family::Vaapi | Family::Amf => json!({"bf": 0}),
    }
}

fn merged(a: Value, b: Value) -> Value {
    let (Value::Object(mut a), Value::Object(b)) = (a, b) else {
        panic!("objects")
    };
    a.extend(b);
    Value::Object(a)
}

#[test]
fn every_rule_key_is_a_real_setting_of_its_encoder() {
    for f in fixtures() {
        let keys: &[&str] = match f.family {
            Family::X264 => &["preset", "tune", "profile", "keyint_sec", "x264opts"],
            Family::Nvenc => &["bf", "keyint_sec"],
            Family::Vaapi | Family::Amf => &["bf", "keyint_sec", "ffmpeg_opts"],
            Family::Qsv => &["bframes", "keyint_sec"],
            Family::VideoToolbox => &["bframes", "keyint_sec"],
        };
        for key in keys {
            assert!(
                f.defaults.contains_key(*key) || f.properties.iter().any(|p| p == key),
                "{}: {key} is neither a default nor a property",
                f.id
            );
        }
    }
}

#[test]
fn defaults_alone_never_pass_except_where_the_encoder_defaults_to_no_b_frames() {
    for f in fixtures() {
        let problems = check(&f, json!({}));
        let b = problems
            .iter()
            .any(|p| matches!(p, Problem::BFrames { .. }));
        let keyint = problems
            .iter()
            .any(|p| matches!(p, Problem::KeyframeInterval { .. }));
        let defaults_b_off = f.family == Family::Vaapi;
        assert_eq!(b, !defaults_b_off, "{}: {problems:?}", f.id);
        let defaults_keyint_ok = f.family == Family::VideoToolbox;
        assert_eq!(keyint, !defaults_keyint_ok, "{}: {problems:?}", f.id);
    }
}

#[test]
fn b_frames_off_and_a_short_keyframe_interval_pass() {
    for f in fixtures() {
        for seconds in [0, 1, 2, 3] {
            let user = merged(b_frames_off(f.family), json!({"keyint_sec": seconds}));
            let problems = check(&f, user);
            assert_eq!(
                problems.is_empty(),
                (1..=2).contains(&seconds),
                "{} keyint_sec {seconds}: {problems:?}",
                f.id
            );
        }
    }
}

#[test]
fn explicit_b_frames_fail() {
    for f in fixtures() {
        let on = match f.family {
            Family::X264 | Family::Nvenc | Family::Vaapi | Family::Amf => json!({"bf": 2}),
            Family::Qsv => json!({"bframes": 2}),
            Family::VideoToolbox => json!({"bframes": true}),
        };
        let problems = check(&f, merged(on, json!({"keyint_sec": 2})));
        assert_eq!(
            problems,
            [Problem::BFrames { family: f.family }],
            "{}",
            f.id
        );
    }
}

#[test]
fn x264_with_bf_unset_or_b_frames_in_its_options_fails() {
    let f = fixtures().into_iter().find(|f| f.id == "obs_x264").unwrap();
    assert!(
        check(&f, json!({"keyint_sec": 2})).contains(&Problem::BFrames {
            family: Family::X264
        })
    );
    let opts = check(
        &f,
        json!({"keyint_sec": 2, "bf": 0, "x264opts": "bframes=0"}),
    );
    assert!(
        matches!(opts[..], [Problem::CustomOptions { .. }]),
        "custom options are refused even when they turn B-frames off: {opts:?}"
    );
}

#[test]
fn custom_options_fail_wherever_the_encoder_has_them() {
    for f in fixtures() {
        let key = match f.family {
            Family::X264 => "x264opts",
            Family::Nvenc if f.properties.iter().any(|p| p == "opts") => "opts",
            Family::Vaapi | Family::Amf => "ffmpeg_opts",
            _ => continue,
        };
        let user = merged(
            merged(b_frames_off(f.family), json!({"keyint_sec": 2})),
            json!({key: "bf=2"}),
        );
        assert!(
            matches!(check(&f, user)[..], [Problem::CustomOptions { .. }]),
            "{}",
            f.id
        );
    }
}

#[test]
fn d_streamys_own_settings_pass_on_every_encoder() {
    for f in fixtures() {
        let problems = check(&f, own_video_settings(f.family, 6000));
        assert!(problems.is_empty(), "{}: {problems:?}", f.id);
    }
}

/// Share mode with the frontend's x264, whose sheet has no B-frames property (obs-x264.c:179-219).
#[test]
fn x264_share_mode_cases() {
    let f = fixtures().into_iter().find(|f| f.id == "obs_x264").unwrap();
    let base = json!({"keyint_sec": 2});
    let default = check(&f, base.clone());
    assert_eq!(
        default,
        [Problem::BFrames {
            family: Family::X264
        }],
        "veryfast with bf unset has 3 B-frames"
    );
    assert!(
        default[0]
            .message()
            .ends_with("› Tune: zerolatency, or Profile: baseline")
    );
    for user in [
        json!({"tune": "zerolatency"}),
        json!({"profile": "baseline"}),
        json!({"preset": "ultrafast"}),
        json!({"bf": 0}),
    ] {
        let problems = check(&f, merged(base.clone(), user.clone()));
        assert!(problems.is_empty(), "{user}: {problems:?}");
    }
    for opts in ["bframes=0", "bframes=2", "keyint=60"] {
        let problems = check(&f, merged(base.clone(), json!({"bf": 0, "x264opts": opts})));
        assert!(
            matches!(problems[..], [Problem::CustomOptions { .. }]),
            "x264opts {opts}: {problems:?}"
        );
    }
}
