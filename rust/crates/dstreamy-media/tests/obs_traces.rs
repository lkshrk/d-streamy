//! Parses the OBS traces under `obs/tests/traces` and checks them against the `docs/specs.md` OBS rows.

use std::path::{Path, PathBuf};

use dstreamy_media::annexb::{self, H264_NAL_IDR, H264_NAL_PPS, H264_NAL_SPS};

const MAGIC: &[u8; 8] = b"DSTRACE1";
const HEADER: usize = 40;

struct Packet {
    video: bool,
    keyframe: bool,
    timebase: (u32, u32),
    pts: i64,
    dts: i64,
    data: Vec<u8>,
}

struct Trace {
    name: String,
    meta: serde_json::Value,
    packets: Vec<Packet>,
    video_extra: Vec<u8>,
    audio_extra: Vec<u8>,
}

impl Trace {
    fn video(&self) -> impl Iterator<Item = &Packet> {
        self.packets.iter().filter(|p| p.video)
    }

    fn audio(&self) -> impl Iterator<Item = &Packet> {
        self.packets.iter().filter(|p| !p.video)
    }

    fn setting(&self, key: &str) -> Option<&serde_json::Value> {
        self.meta["video_settings"].get(key)
    }
}

fn traces_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../obs/tests/traces")
}

fn parse_packets(bytes: &[u8]) -> Vec<Packet> {
    let mut rest = bytes.strip_prefix(MAGIC).expect("DSTRACE1 magic");
    let mut packets = Vec::new();
    while !rest.is_empty() {
        let (hdr, tail) = rest.split_at(HEADER);
        let u32_at = |o: usize| u32::from_le_bytes(hdr[o..o + 4].try_into().unwrap());
        let i64_at = |o: usize| i64::from_le_bytes(hdr[o..o + 8].try_into().unwrap());
        let (data, tail) = tail.split_at(u32_at(12) as usize);
        packets.push(Packet {
            video: hdr[0] == 1,
            keyframe: hdr[1] == 1,
            timebase: (u32_at(4), u32_at(8)),
            pts: i64_at(16),
            dts: i64_at(24),
            data: data.to_vec(),
        });
        rest = tail;
    }
    packets
}

fn load_all() -> Vec<Trace> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(traces_dir())
        .expect("obs/tests/traces")
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .map(|dir| {
            let read =
                |f: &str| std::fs::read(dir.join(f)).unwrap_or_else(|e| panic!("{dir:?}/{f}: {e}"));
            let provenance = String::from_utf8(read("PROVENANCE.md")).unwrap();
            for field in [
                "| libobs | 32.2.2 |",
                "| OBS commit | ba2f32bdf791005443988a4955e963663e16b1ed |",
                "| CI run | ",
                "| OS | Linux",
                "| Date (UTC) | ",
                "| Video encoder | ",
                "| Audio encoder | ",
                "Command line:",
            ] {
                assert!(
                    provenance.contains(field),
                    "{dir:?}: PROVENANCE.md lacks {field:?}"
                );
            }
            assert!(
                !provenance.contains("| CI run | local |"),
                "{dir:?} was not recorded on CI"
            );
            Trace {
                name: dir.file_name().unwrap().to_string_lossy().into_owned(),
                meta: serde_json::from_slice(&read("trace.json")).unwrap(),
                packets: parse_packets(&read("packets.bin")),
                video_extra: read("video-extra.bin"),
                audio_extra: read("audio-extra.bin"),
            }
        })
        .collect()
}

fn nal_types(au: &[u8]) -> Vec<u8> {
    annexb::nal_units(au)
        .filter_map(annexb::h264_nal_type)
        .collect()
}

// RFC 6716 §3.1: frame duration in 2.5 ms units from the TOC config, times the frame count from code c.
fn opus_duration_quarter_ms(packet: &[u8]) -> Option<u32> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    let frame = match config {
        0..=11 => [40, 80, 160, 240][usize::from(config & 3)],
        12..=15 => [40, 80][usize::from(config & 1)],
        _ => [10, 20, 40, 80][usize::from(config & 3)],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u32::from(*packet.get(1)? & 0x3f),
    };
    Some(frame * frames)
}

#[test]
fn every_trace_is_complete_and_from_the_pinned_obs() {
    let traces = load_all();
    assert!(traces.len() >= 9, "only {} traces", traces.len());
    for t in &traces {
        assert_eq!(t.meta["libobs"], "32.2.2", "{}", t.name);
        assert_eq!(t.meta["video_codec"], "h264", "{}", t.name);
        assert_eq!(
            t.meta["obs_commit"], "ba2f32bdf791005443988a4955e963663e16b1ed",
            "{}",
            t.name
        );
        assert_eq!(t.meta["lagged_frames"], 0, "{}", t.name);
        assert_eq!(t.meta["skipped_frames"], 0, "{}", t.name);
        let video = t.video().count() as i64;
        assert_eq!(
            video,
            t.meta["video_packets"].as_i64().unwrap(),
            "{}",
            t.name
        );
        assert!(video >= 60 && t.audio().count() >= 100, "{}", t.name);
    }
}

#[test]
fn h264_is_annex_b_with_sps_pps_in_extra_data_and_every_idr() {
    for t in load_all() {
        assert_eq!(
            &t.video_extra[..4],
            &annexb::START_CODE,
            "{}: extra data is not Annex B",
            t.name
        );
        assert_eq!(
            nal_types(&t.video_extra),
            [H264_NAL_SPS, H264_NAL_PPS],
            "{}",
            t.name
        );

        let fps = t.meta["fps"].as_u64().unwrap() as u32;
        let mut keyframes = Vec::new();
        for p in t.video() {
            assert!(
                p.data.starts_with(&[0, 0, 1]) || p.data.starts_with(&annexb::START_CODE),
                "{}: packet without a start code",
                t.name
            );
            assert_eq!(p.timebase, (1, fps), "{}: video timebase", t.name);
            let types = nal_types(&p.data);
            assert_eq!(
                p.keyframe,
                types.contains(&H264_NAL_IDR),
                "{}: keyframe flag vs IDR",
                t.name
            );
            assert_eq!(annexb::h264_is_keyframe(&p.data), p.keyframe, "{}", t.name);
            if p.keyframe {
                keyframes.push(p.dts);
                assert!(
                    types.contains(&H264_NAL_SPS) && types.contains(&H264_NAL_PPS),
                    "{}: IDR without in-band SPS/PPS {types:?}",
                    t.name
                );
            }
        }
        assert!(
            t.video().next().unwrap().keyframe,
            "{}: first packet is not an IDR",
            t.name
        );
        assert!(!keyframes.is_empty(), "{}", t.name);
    }
}

#[test]
fn idr_spacing_follows_keyint_sec_with_scenecut_off() {
    for t in load_all() {
        assert_eq!(
            t.setting("x264opts").and_then(|v| v.as_str()),
            Some("scenecut=0"),
            "{}",
            t.name
        );
        if t.setting("bf").and_then(|v| v.as_i64()) != Some(0) {
            continue;
        }
        let fps = t.meta["fps"].as_i64().unwrap();
        let frames = t.video().count() as i64;
        let idrs: Vec<i64> = t.video().filter(|p| p.keyframe).map(|p| p.dts).collect();
        match t.setting("keyint_sec").and_then(|v| v.as_i64()) {
            Some(0) => {
                // keyint_sec=0 leaves x264's default keyint of 250 frames: one IDR in a few seconds.
                let expected: Vec<i64> = (0..frames).step_by(250).collect();
                assert_eq!(idrs, expected, "{}", t.name);
            }
            Some(k) => {
                let expected: Vec<i64> = (0..frames).step_by((k * fps) as usize).collect();
                assert!(
                    expected.len() >= 2,
                    "{}: trace too short for keyint {k}",
                    t.name
                );
                assert_eq!(idrs, expected, "{}: IDRs not every {k} s", t.name);
            }
            None => panic!("{}: keyint_sec not set", t.name),
        }
    }
}

#[test]
fn bf0_gives_pts_equal_dts_and_b_frames_do_not() {
    for t in load_all() {
        let pts_ne_dts = t.video().filter(|p| p.pts != p.dts).count();
        match t.setting("bf").and_then(|v| v.as_i64()) {
            Some(0) => {
                assert_eq!(pts_ne_dts, 0, "{}", t.name);
                let dts: Vec<i64> = t.video().map(|p| p.dts).collect();
                assert!(
                    dts.windows(2).all(|w| w[1] == w[0] + 1),
                    "{}: dts not consecutive",
                    t.name
                );
                assert_eq!(dts[0], 0, "{}", t.name);
            }
            _ => {
                assert!(pts_ne_dts > 0, "{}: B-frames expected", t.name);
                assert!(
                    t.video().next().unwrap().dts < 0,
                    "{}: B-frame dts starts negative",
                    t.name
                );
            }
        }
    }
}

#[test]
fn opus_packets_are_20ms_code_0_with_priming() {
    let opus: Vec<Trace> = load_all()
        .into_iter()
        .filter(|t| t.meta["audio_codec"] == "opus")
        .collect();
    assert!(!opus.is_empty());
    for t in opus {
        assert_eq!(&t.audio_extra[..8], b"OpusHead", "{}", t.name);
        assert_eq!(t.audio_extra[9], 2, "{}: OpusHead channel count", t.name);
        let pre_skip = u16::from_le_bytes([t.audio_extra[10], t.audio_extra[11]]);
        assert_eq!(t.meta["audio_frame_size"], 960, "{}", t.name);

        let audio: Vec<&Packet> = t.audio().collect();
        assert_eq!(
            audio[0].pts,
            -i64::from(pre_skip),
            "{}: first pts is minus the pre-skip",
            t.name
        );
        for (i, p) in audio.iter().enumerate() {
            assert_eq!(p.timebase, (1, 48_000), "{}", t.name);
            assert!(p.keyframe, "{}: Opus packet without keyframe flag", t.name);
            assert_eq!(p.pts, p.dts, "{}", t.name);
            assert_eq!(p.pts, audio[0].pts + 960 * i as i64, "{}: pts step", t.name);
            assert_eq!(
                opus_duration_quarter_ms(&p.data),
                Some(80),
                "{}: TOC {:#04x}",
                t.name,
                p.data[0]
            );
            assert_eq!(p.data[0] & 3, 0, "{}: frame-count code", t.name);
        }
    }
}

#[test]
fn aac_trace_is_not_opus() {
    let aac: Vec<Trace> = load_all()
        .into_iter()
        .filter(|t| t.meta["audio_codec"] == "aac")
        .collect();
    assert_eq!(aac.len(), 1);
    let asc = &aac[0].audio_extra;
    assert!(!asc.starts_with(b"OpusHead"));
    // AudioSpecificConfig layout: docs/specs.md pin (FFmpeg libavcodec/mpeg4audio.c:77-101).
    assert!(asc.len() >= 2, "AudioSpecificConfig too short");
    assert_eq!(asc[0] >> 3, 2, "object type is not AAC LC");
    assert_eq!(
        ((asc[0] & 7) << 1) | (asc[1] >> 7),
        3,
        "sampling index is not 48 kHz"
    );
    assert_eq!(
        (asc[1] >> 3) & 0xf,
        2,
        "channel configuration is not stereo"
    );
    assert_eq!(aac[0].meta["audio_frame_size"], 1024);
    let audio: Vec<&Packet> = aac[0].audio().collect();
    for (i, p) in audio.iter().enumerate() {
        assert_eq!(p.timebase, (1, 48_000), "AAC timebase");
        assert_eq!(
            p.pts,
            audio[0].pts + 1024 * i as i64,
            "AAC pts step at packet {i}"
        );
    }
}

#[test]
fn opus_toc_durations_follow_rfc_6716() {
    assert_eq!(opus_duration_quarter_ms(&[0xfc, 0xff, 0xfe]), Some(80));
    assert_eq!(opus_duration_quarter_ms(&[0xf8, 0xff, 0xfe]), Some(80));
    assert_eq!(opus_duration_quarter_ms(&[0x18]), Some(240));
    assert_eq!(opus_duration_quarter_ms(&[0xe1]), Some(20));
    assert_eq!(opus_duration_quarter_ms(&[0xe3, 0x04]), Some(40));
    assert_eq!(opus_duration_quarter_ms(&[]), None);
}

fn pts_us(p: &Packet) -> i64 {
    p.pts * 1_000_000 * i64::from(p.timebase.0) / i64::from(p.timebase.1)
}

/// Feeds a trace's packets through `Ingest` as the plugin will send them; the first refusal, if any.
fn ingest(trace: &Trace) -> Result<(), String> {
    let mut ingest = dstreamy_media::Ingest::default();
    for (i, p) in trace.packets.iter().enumerate() {
        let checked = if p.video {
            let has_sps =
                annexb::nal_units(&p.data).any(|n| annexb::h264_nal_type(n) == Some(H264_NAL_SPS));
            let mut au = Vec::new();
            if p.keyframe && !has_sps {
                au.extend_from_slice(&trace.video_extra);
            }
            au.extend_from_slice(&p.data);
            ingest.video(&au, pts_us(p), p.keyframe)
        } else {
            ingest.audio(&p.data, pts_us(p))
        };
        checked.map_err(|e| format!("{} packet {i}: {e}", trace.name))?;
    }
    Ok(())
}

#[test]
fn every_opus_trace_passes_ingest_unless_it_uses_b_frames() {
    let mut seen = 0;
    for trace in load_all() {
        if trace.meta["audio_codec"] != "opus" {
            continue;
        }
        seen += 1;
        let result = ingest(&trace);
        if trace.name.contains("-bf2-") || trace.name.contains("-bfunset-") {
            let e = result.expect_err(&trace.name);
            assert!(e.contains("B-frames are enabled"), "{e}");
        } else {
            result.unwrap();
        }
    }
    assert_eq!(seen, 8, "Opus traces");
}
