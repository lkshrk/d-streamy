use std::path::{Path, PathBuf};

use proptest::prelude::*;

use super::*;

const SPS: &[u8] = &[0x67, 0x42, 0xc0, 0x1f, 0xda, 0x01];
const PPS: &[u8] = &[0x68, 0xce, 0x3c, 0x80];
const AUD: &[u8] = &[0x09, 0xf0];
const SEI: &[u8] = &[0x06, 0x05, 0x01, 0xaa, 0x80];
const IDR_A: &[u8] = &[0x65, 0x88, 0x84, 0x00, 0x00, 0x03, 0x01, 0x21];
const IDR_B: &[u8] = &[0x65, 0x00, 0xa0, 0x00, 0x00, 0x03, 0x00, 0x7f];
const SLICE: &[u8] = &[0x41, 0x9a, 0x00, 0x00, 0x03, 0x02, 0x10];

fn au(nals: &[(usize, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(code, nal) in nals {
        out.extend_from_slice(&START_CODE[4 - code..]);
        out.extend_from_slice(nal);
    }
    out
}

fn with_extra(code: usize) -> Converter {
    let mut conv = Converter::new();
    conv.set_video_extra_data(&au(&[(code, SPS), (code, PPS)]))
        .unwrap();
    conv
}

fn key<'a>(conv: &Converter, data: &'a [u8]) -> Result<VideoFrame<'a>, PacketError> {
    conv.convert_video(data, true, 0, 0, (1, 30))
}

/// An independent reading of H.264 §B.1: NAL payloads between start codes, trailing zeros dropped.
fn payloads(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &s)| {
            let mut e = starts.get(k + 1).map_or(data.len(), |&n| n - 3);
            while e > s && data[e - 1] == 0 {
                e -= 1;
            }
            &data[s..e]
        })
        .collect()
}

fn kinds(data: &[u8]) -> Vec<u8> {
    payloads(data).iter().map(|n| n[0] & 0x1f).collect()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn three_and_four_byte_start_codes() {
    for code in [3, 4] {
        let conv = with_extra(code);
        let idr = au(&[(code, IDR_A)]);
        let out = key(&conv, &idr).unwrap();
        assert_eq!(kinds(&out.data), [NAL_SPS, NAL_PPS, NAL_IDR], "{code}");
        assert_eq!(out.data.len(), 4 + SPS.len() + 4 + PPS.len() + idr.len());
        assert!(out.data.ends_with(&idr), "{code}-byte IDR not kept");
        let p = au(&[(code, SLICE)]);
        let out = conv.convert_video(&p, false, 1, 1, (1, 30)).unwrap();
        assert!(matches!(out.data, Cow::Borrowed(d) if d == p.as_slice()));
    }
}

#[test]
fn mixed_start_codes_in_one_access_unit() {
    let conv = with_extra(4);
    let idr = au(&[(3, SEI), (4, IDR_A), (3, IDR_B), (4, IDR_A)]);
    let out = key(&conv, &idr).unwrap();
    assert_eq!(
        kinds(&out.data),
        [NAL_SPS, NAL_PPS, 6, NAL_IDR, NAL_IDR, NAL_IDR]
    );
    assert!(out.data.ends_with(&idr));
}

#[test]
fn aud_stays_first_and_sei_follows_the_parameter_sets() {
    let conv = with_extra(4);
    let idr = au(&[(4, AUD), (3, SEI), (3, IDR_A)]);
    let out = key(&conv, &idr).unwrap();
    assert_eq!(kinds(&out.data), [NAL_AUD, NAL_SPS, NAL_PPS, 6, NAL_IDR]);
    assert!(contains(&out.data, SEI) && contains(&out.data, IDR_A));

    let idr = au(&[(4, SEI), (4, IDR_A)]);
    assert_eq!(
        kinds(&key(&conv, &idr).unwrap().data),
        [NAL_SPS, NAL_PPS, 6, NAL_IDR]
    );

    let inband = au(&[(4, AUD), (4, SPS), (4, PPS), (4, IDR_A)]);
    assert!(matches!(
        key(&conv, &inband).unwrap().data,
        Cow::Borrowed(_)
    ));
}

#[test]
fn multiple_slices_get_one_set_of_parameter_sets() {
    let conv = with_extra(4);
    let idr = au(&[(4, IDR_A), (3, IDR_B), (3, IDR_A)]);
    let out = key(&conv, &idr).unwrap();
    assert_eq!(
        kinds(&out.data),
        [NAL_SPS, NAL_PPS, NAL_IDR, NAL_IDR, NAL_IDR]
    );
    assert!(out.data.ends_with(&idr));
    let p = au(&[(4, SLICE), (3, SLICE)]);
    assert!(matches!(
        conv.convert_video(&p, false, 1, 1, (1, 30)).unwrap().data,
        Cow::Borrowed(_)
    ));
}

#[test]
fn emulation_prevention_bytes_are_untouched() {
    let conv = with_extra(4);
    let idr = au(&[(4, IDR_A), (4, IDR_B)]);
    let out = key(&conv, &idr).unwrap();
    assert!(out.data.ends_with(&idr));
    assert!(contains(&out.data, &[0, 0, 3, 1]) && contains(&out.data, &[0, 0, 3, 0]));
}

#[test]
fn a_missing_pps_goes_after_the_last_in_band_sps() {
    let conv = with_extra(4);
    let idr = au(&[(4, SEI), (4, SPS), (4, SEI), (4, IDR_A)]);
    let out = key(&conv, &idr).unwrap();
    assert_eq!(kinds(&out.data), [6, NAL_SPS, NAL_PPS, 6, NAL_IDR]);
}

#[test]
fn a_missing_sps_goes_before_the_in_band_pps() {
    let conv = with_extra(4);
    let idr = au(&[(4, AUD), (4, PPS), (4, IDR_A)]);
    let out = key(&conv, &idr).unwrap();
    assert_eq!(kinds(&out.data), [NAL_AUD, NAL_SPS, NAL_PPS, NAL_IDR]);
}

#[test]
fn a_pps_is_added_only_to_the_extra_data_sps() {
    let conv = with_extra(4);
    let other_sps: &[u8] = &[0x67, 0x4d, 0x40, 0x28, 0xda, 0x01];
    let idr = au(&[(4, other_sps), (4, IDR_A)]);
    assert_eq!(key(&conv, &idr), Err(PacketError::ParameterSetMismatch));
    let other_pps: &[u8] = &[0x68, 0xee, 0x3c, 0x80];
    let idr = au(&[(4, other_pps), (4, IDR_A)]);
    assert_eq!(key(&conv, &idr), Err(PacketError::ParameterSetMismatch));
}

#[test]
fn present_parameter_sets_are_never_duplicated() {
    let conv = with_extra(4);
    let both = au(&[(4, SPS), (4, PPS), (4, IDR_A)]);
    let out = key(&conv, &both).unwrap();
    assert!(matches!(out.data, Cow::Borrowed(d) if d == both.as_slice()));
    for _ in 0..3 {
        let idr = au(&[(4, IDR_A)]);
        assert_eq!(
            kinds(&key(&conv, &idr).unwrap().data),
            [NAL_SPS, NAL_PPS, NAL_IDR]
        );
    }
}

#[test]
fn extra_data_is_read_lazily() {
    let mut conv = Converter::new();
    let idr = au(&[(4, IDR_A)]);
    assert!(!conv.has_video_extra_data());
    assert_eq!(key(&conv, &idr), Err(PacketError::NoParameterSets));
    let inband = au(&[(4, SPS), (4, PPS), (4, IDR_A)]);
    assert!(matches!(
        key(&conv, &inband).unwrap().data,
        Cow::Borrowed(_)
    ));
    conv.set_video_extra_data(&au(&[(4, SPS), (4, PPS)]))
        .unwrap();
    assert!(conv.has_video_extra_data());
    assert_eq!(
        kinds(&key(&conv, &idr).unwrap().data),
        [NAL_SPS, NAL_PPS, NAL_IDR]
    );
}

#[test]
fn a_refused_extra_data_update_keeps_the_previous_sets() {
    let mut conv = with_extra(4);
    assert_eq!(
        conv.set_video_extra_data(&au(&[(4, SPS)])),
        Err(PacketError::ExtraDataIncomplete)
    );
    assert_eq!(
        conv.set_video_extra_data(&[1, 0x42, 0xc0, 0x1f, 0xff, 0xe1]),
        Err(PacketError::AvccExtraData)
    );
    assert!(conv.has_video_extra_data());
    assert_eq!(
        kinds(&key(&conv, &au(&[(4, IDR_A)])).unwrap().data),
        [NAL_SPS, NAL_PPS, NAL_IDR]
    );
}

#[test]
fn avcc_is_rejected() {
    let avcc = [
        0x01, 0x42, 0xc0, 0x1f, 0xff, 0xe1, 0x00, 0x06, 0x67, 0x42, 0xc0, 0x1f, 0xda, 0x01, 0x01,
        0x00, 0x04, 0x68, 0xce, 0x3c, 0x80,
    ];
    let mut conv = Converter::new();
    assert_eq!(
        conv.set_video_extra_data(&avcc),
        Err(PacketError::AvccExtraData)
    );
    assert_eq!(
        conv.set_video_extra_data(&[]),
        Err(PacketError::AvccExtraData)
    );
    assert!(!conv.has_video_extra_data());
    let length_prefixed = [0, 0, 0, 8, 0x65, 0x88, 0x84, 0, 0, 3, 1, 0x21];
    assert_eq!(key(&conv, &length_prefixed), Err(PacketError::NotAnnexB));
    assert_eq!(
        conv.convert_video(&length_prefixed[..3], false, 0, 0, (1, 30)),
        Err(PacketError::NotAnnexB)
    );
}

#[test]
fn leading_zero_bytes_before_the_first_start_code_are_annex_b() {
    let mut conv = Converter::new();
    let mut extra = vec![0; 5];
    extra.extend(au(&[(3, SPS), (4, PPS)]));
    conv.set_video_extra_data(&extra).unwrap();
    let mut idr = vec![0; 7];
    idr.extend(au(&[(3, IDR_A)]));
    assert_eq!(
        kinds(&key(&conv, &idr).unwrap().data),
        [NAL_SPS, NAL_PPS, NAL_IDR]
    );
    assert_eq!(key(&conv, &[0, 0, 0, 0]), Err(PacketError::NotAnnexB));
}

#[test]
fn b_frames_are_refused() {
    let conv = with_extra(4);
    let idr = au(&[(4, SPS), (4, PPS), (4, IDR_A)]);
    assert_eq!(
        conv.convert_video(&idr, true, 0, -2, (1, 30)),
        Err(PacketError::BFrames)
    );
    assert_eq!(
        conv.convert_video(&idr, false, 1, 2, (1, 30)),
        Err(PacketError::BFrames)
    );
    assert!(
        PacketError::BFrames
            .to_string()
            .starts_with("B-frames are enabled")
    );
    assert!(conv.convert_video(&idr, true, 3, 3, (1, 30)).is_ok());
}

#[test]
fn pts_conversion_known_values() {
    for (pts, tb, us) in [
        (0, (1, 30), 0),
        (1, (1, 30), 33_333),
        (149, (1, 30), 4_966_666),
        (1, (1, 60), 16_666),
        (299, (1, 60), 4_983_333),
        (1, (1, 15), 66_666),
        (-312, (1, 48_000), -6_500),
        (648, (1, 48_000), 13_500),
        (960, (1, 48_000), 20_000),
        (-1024, (1, 48_000), -21_333),
        (-1, (1, 30), -33_333),
        (-2, (1, 30), -66_666),
        (1001, (1001, 30_000), 33_366),
        (3, (3, 90), 33_333),
        (7, (1, 1_000_000), 7),
        (30_000_000_000_001, (1, 30), 1_000_000_000_000_033_333),
    ] {
        assert_eq!(pts_to_us(pts, tb), Ok(us), "{pts} at {tb:?}");
    }
    assert_eq!(pts_to_us(i64::MAX, (1, 1)), Err(PacketError::PtsOverflow));
    assert_eq!(pts_to_us(i64::MIN, (1, 1)), Err(PacketError::PtsOverflow));
    for tb in [(0, 30), (1, 0), (1, -30), (-1, 30), (i32::MIN, 1)] {
        assert_eq!(pts_to_us(1, tb), Err(PacketError::BadTimebase), "{tb:?}");
    }
    assert_eq!(convert_audio(-312, -312, (1, 48_000)), Ok(-6_500));
    assert_eq!(
        convert_audio(0, 1, (1, 48_000)),
        Err(PacketError::AudioPtsNeDts)
    );
}

fn reference_us(pts: i64, (_, den): (i32, i32)) -> Option<i64> {
    i64::try_from(i128::from(pts) * 1_000_000 / i128::from(den)).ok()
}

fn timebase() -> impl Strategy<Value = (i32, i32)> {
    prop_oneof![
        Just((1, 30)),
        Just((1, 60)),
        Just((1, 48_000)),
        Just((1001, 30_000)),
        Just((1, i32::MAX)),
        Just((i32::MAX, 1)),
        Just((i32::MAX, i32::MAX)),
        (1..=i32::MAX, 1..=i32::MAX),
    ]
}

fn pts() -> impl Strategy<Value = i64> {
    prop_oneof![
        -1_000_000i64..1_000_000,
        any::<i64>(),
        Just(i64::MAX),
        Just(i64::MIN),
        Just(i64::MAX / 1_000_000),
        Just(i64::MIN / 1_000_000),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200_000))]

    #[test]
    fn pts_conversion_matches_a_128_bit_reference(pts in pts(), tb in timebase()) {
        prop_assert_eq!(pts_to_us(pts, tb).ok(), reference_us(pts, tb));
    }
}

fn nal_strategy() -> impl Strategy<Value = (usize, Vec<u8>)> {
    let kind = prop_oneof![
        Just(1u8),
        Just(5),
        Just(6),
        Just(7),
        Just(8),
        Just(9),
        0u8..32
    ];
    (
        prop_oneof![Just(3usize), Just(4)],
        kind,
        proptest::collection::vec(any::<u8>(), 0..24),
    )
        .prop_map(|(code, kind, mut body)| {
            body.insert(0, 0x60 | kind);
            (code, body)
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20_000))]

    #[test]
    fn random_packets_never_panic_and_keyframes_carry_one_sps_and_pps(
        nals in proptest::collection::vec(nal_strategy(), 0..8),
        raw in proptest::collection::vec(any::<u8>(), 0..64),
        keyframe: bool,
        extra: bool,
        pts in pts(),
        tb in timebase(),
        use_raw: bool,
    ) {
        let conv = if extra { with_extra(4) } else { Converter::new() };
        let data = if use_raw {
            raw
        } else {
            let parts: Vec<(usize, &[u8])> = nals.iter().map(|(c, n)| (*c, n.as_slice())).collect();
            au(&parts)
        };
        let mut other = Converter::new();
        let _ = other.set_video_extra_data(&data);
        if let Ok(out) = conv.convert_video(&data, keyframe, pts, pts, tb) {
            prop_assert_eq!(Some(out.pts_us), reference_us(pts, tb));
            if !keyframe {
                prop_assert!(matches!(out.data, Cow::Borrowed(_)));
            } else if matches!(out.data, Cow::Owned(_)) {
                let types = kinds(&out.data);
                let first_vcl = types.iter().position(|&t| (1..=5).contains(&t)).unwrap_or(types.len());
                let head = &types[..first_vcl];
                prop_assert_eq!(head.iter().filter(|&&t| t == NAL_SPS).count(), 1, "{:?}", types);
                prop_assert_eq!(head.iter().filter(|&&t| t == NAL_PPS).count(), 1, "{:?}", types);
                let sps_at = head.iter().position(|&t| t == NAL_SPS);
                let pps_at = head.iter().position(|&t| t == NAL_PPS);
                prop_assert!(sps_at < pps_at, "{:?}", types);
                let rest: Vec<&[u8]> = payloads(&out.data)
                    .into_iter()
                    .filter(|n| !matches!(n[0] & 0x1f, NAL_SPS | NAL_PPS))
                    .collect();
                let before: Vec<&[u8]> = payloads(&data)
                    .into_iter()
                    .filter(|n| !matches!(n[0] & 0x1f, NAL_SPS | NAL_PPS))
                    .collect();
                prop_assert_eq!(rest, before);
            }
        }
    }
}

fn traces_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../obs/tests/traces")
}

struct TracePacket {
    video: bool,
    keyframe: bool,
    timebase: (i32, i32),
    pts: i64,
    dts: i64,
    data: Vec<u8>,
}

/// `packets.bin` as described in each trace's `PROVENANCE.md`.
fn read_trace(dir: &Path) -> (Vec<TracePacket>, Vec<u8>) {
    let bytes = std::fs::read(dir.join("packets.bin")).unwrap();
    let extra = std::fs::read(dir.join("video-extra.bin")).unwrap();
    let mut rest = bytes.strip_prefix(b"DSTRACE1").expect("magic");
    let mut packets = Vec::new();
    while !rest.is_empty() {
        let (h, tail) = rest.split_at(40);
        let i32_at = |o: usize| i32::from_le_bytes(h[o..o + 4].try_into().unwrap());
        let i64_at = |o: usize| i64::from_le_bytes(h[o..o + 8].try_into().unwrap());
        let (data, tail) = tail.split_at(i32_at(12) as usize);
        packets.push(TracePacket {
            video: h[0] == 1,
            keyframe: h[1] == 1,
            timebase: (i32_at(4), i32_at(8)),
            pts: i64_at(16),
            dts: i64_at(24),
            data: data.to_vec(),
        });
        rest = tail;
    }
    (packets, extra)
}

fn trace_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(traces_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    assert!(dirs.len() >= 9, "{dirs:?}");
    dirs
}

fn strip(data: &[u8], drop: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for n in payloads(data) {
        if !drop.contains(&(n[0] & 0x1f)) {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(n);
        }
    }
    out
}

fn decodes(aus: &[Vec<u8>]) -> usize {
    let mut decoder = openh264::decoder::Decoder::new().unwrap();
    let mut pictures = 0;
    for au in aus {
        if decoder.decode(au).expect("decodes").is_some() {
            pictures += 1;
        }
    }
    pictures + decoder.flush_remaining().unwrap().len()
}

#[test]
fn every_trace_converts_or_is_refused_for_b_frames() {
    for dir in trace_dirs() {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let b_frames = name.contains("-bf2-") || name.contains("-bfunset-");
        let (packets, extra) = read_trace(&dir);
        let mut conv = Converter::new();
        let mut stripped = Converter::new();
        stripped.set_video_extra_data(&extra).unwrap();
        let (mut videos, mut keyframes, mut refusal, mut last) = (0, 0, None, i64::MIN);
        let mut opus_first = None;
        for p in &packets {
            if !p.video {
                let us = convert_audio(p.pts, p.dts, p.timebase).unwrap();
                assert_eq!(Some(us), reference_us(p.pts, p.timebase), "{name}");
                opus_first.get_or_insert(us);
                continue;
            }
            if videos == 0 {
                assert!(!conv.has_video_extra_data());
            }
            let out = conv.convert_video(&p.data, p.keyframe, p.pts, p.dts, p.timebase);
            if videos == 0 {
                conv.set_video_extra_data(&extra).unwrap();
            }
            videos += 1;
            let out = match out {
                Ok(out) => out,
                Err(e) => {
                    refusal.get_or_insert(e);
                    continue;
                }
            };
            assert!(out.pts_us > last, "{name}: pts not increasing");
            last = out.pts_us;
            assert!(
                matches!(out.data, Cow::Borrowed(_)),
                "{name}: x264 keyframes carry SPS/PPS"
            );
            if !p.keyframe {
                continue;
            }
            keyframes += 1;
            for drop in [&[NAL_SPS, NAL_PPS][..], &[NAL_PPS], &[NAL_SPS]] {
                let input = strip(&p.data, drop);
                let rebuilt = stripped
                    .convert_video(&input, true, p.pts, p.dts, p.timebase)
                    .unwrap();
                let types = kinds(&rebuilt.data);
                let head = &types[..types.iter().position(|&t| t == NAL_IDR).unwrap()];
                assert_eq!(
                    head.iter().filter(|&&t| t == NAL_SPS).count(),
                    1,
                    "{name} {drop:?}"
                );
                assert_eq!(
                    head.iter().filter(|&&t| t == NAL_PPS).count(),
                    1,
                    "{name} {drop:?}"
                );
                assert!(
                    head.iter().position(|&t| t == NAL_SPS)
                        < head.iter().position(|&t| t == NAL_PPS),
                    "{name} {drop:?}: {types:?}"
                );
            }
        }
        if b_frames {
            assert_eq!(refusal, Some(PacketError::BFrames), "{name}");
        } else {
            assert_eq!(refusal, None, "{name}");
            assert!(keyframes > 0, "{name}");
        }
        if !name.contains("aac") {
            assert_eq!(opus_first, Some(-6_500), "{name}: first Opus pts");
        }
    }
}

#[test]
fn keyframes_missing_a_pps_or_sps_decode_after_conversion() {
    let dir = traces_dir().join("linux-x264-main-bf0-keyint2-30fps");
    let (packets, extra) = read_trace(&dir);
    let mut conv = Converter::new();
    conv.set_video_extra_data(&extra).unwrap();
    let video: Vec<&TracePacket> = packets.iter().filter(|p| p.video).take(30).collect();
    for drop in [&[NAL_PPS][..], &[NAL_SPS], &[NAL_SPS, NAL_PPS]] {
        let aus: Vec<Vec<u8>> = video
            .iter()
            .map(|p| {
                let input = if p.keyframe {
                    strip(&p.data, drop)
                } else {
                    p.data.clone()
                };
                conv.convert_video(&input, p.keyframe, p.pts, p.dts, p.timebase)
                    .unwrap()
                    .data
                    .into_owned()
            })
            .collect();
        assert_eq!(decodes(&aus), video.len(), "{drop:?}");
    }
}

#[test]
fn extra_data_over_the_cap_is_refused_and_keeps_the_previous_sets() {
    let mut conv = with_extra(4);
    let mut extra = au(&[(4, SPS), (4, PPS)]);
    extra.resize(MAX_EXTRA_DATA + 1, 0xff);
    assert_eq!(
        conv.set_video_extra_data(&extra),
        Err(PacketError::ExtraDataTooLarge)
    );
    assert!(conv.has_video_extra_data());
}

#[test]
fn the_nal_scan_stops_at_the_first_slice() {
    let slices: Vec<(usize, &[u8])> = std::iter::repeat_n((3, IDR_B), 10_000).collect();
    let mut nals = vec![(4, AUD), (3, IDR_A)];
    nals.extend(slices);
    let data = au(&nals);
    assert_eq!(nal_units(&data).len(), 2);
    let out = key(&with_extra(4), &data).unwrap();
    assert!(out.data.ends_with(&data[6..]));
    assert_eq!(&kinds(&out.data)[..4], [NAL_AUD, NAL_SPS, NAL_PPS, NAL_IDR]);
}
