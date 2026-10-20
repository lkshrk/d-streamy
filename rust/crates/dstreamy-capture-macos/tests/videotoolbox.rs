#![cfg(target_os = "macos")]

use std::ptr::{self, NonNull};
use std::sync::mpsc;
use std::time::Duration;

use dstreamy_capture_macos::{VtEncoder, VtEncoderConfig};
use dstreamy_media::annexb::{h264_nal_type, h265_nal_type, nal_units};
use dstreamy_media::{EncodedVideoFrame, VideoCodec, VideoEncoder};
use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32BGRA, kCVReturnSuccess,
};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const FRAMES: u64 = 12;

fn bgra_frame(index: u64) -> CFRetained<CVPixelBuffer> {
    let mut raw: *mut CVPixelBuffer = ptr::null_mut();
    // SAFETY: valid arguments and out-pointer.
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            WIDTH as usize,
            HEIGHT as usize,
            kCVPixelFormatType_32BGRA,
            None,
            NonNull::from(&mut raw),
        )
    };
    assert_eq!(status, kCVReturnSuccess, "CVPixelBufferCreate");
    // SAFETY: Create returned a +1 reference.
    let buffer = unsafe { CFRetained::from_raw(NonNull::new(raw).unwrap()) };
    // SAFETY: the buffer is locked while its memory is written, within bytes-per-row bounds.
    unsafe {
        assert_eq!(
            CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags::empty()),
            kCVReturnSuccess
        );
        let base = CVPixelBufferGetBaseAddress(&buffer).cast::<u8>();
        let stride = CVPixelBufferGetBytesPerRow(&buffer);
        for y in 0..HEIGHT as usize {
            let row = std::slice::from_raw_parts_mut(base.add(y * stride), WIDTH as usize * 4);
            for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let shift = index as usize * 4;
                *px = [(x + shift) as u8, y as u8, ((x ^ y) + shift) as u8, 0xff];
            }
        }
        CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::empty());
    }
    buffer
}

fn config(codec: VideoCodec) -> VtEncoderConfig {
    VtEncoderConfig {
        width: WIDTH,
        height: HEIGHT,
        fps: 30,
        bitrate_bps: 2_000_000,
        codec,
    }
}

fn encode_all(encoder: &mut VtEncoder) -> Vec<EncodedVideoFrame> {
    let mut units = Vec::new();
    for i in 0..FRAMES {
        let frame = bgra_frame(i);
        units.extend(encoder.encode(&frame, i * 33_333, i == 0).unwrap());
    }
    units.extend(encoder.flush().unwrap());
    units
}

fn nal_types(unit: &EncodedVideoFrame) -> Vec<u8> {
    let parse = match unit.codec {
        VideoCodec::H264 => h264_nal_type,
        VideoCodec::H265 => h265_nal_type,
    };
    nal_units(&unit.data).filter_map(parse).collect()
}

#[test]
fn h264_keyframe_has_parameter_sets_and_decodes() {
    let mut encoder = VtEncoder::new(config(VideoCodec::H264)).expect("H.264 encoder");
    assert_eq!(encoder.codec(), VideoCodec::H264);
    let units = encode_all(&mut encoder);
    assert!(!units.is_empty(), "no access units");

    let first = &units[0];
    assert!(first.keyframe);
    assert_eq!(first.pts_us, 0);
    let types = nal_types(first);
    for (nal, name) in [(7, "SPS"), (8, "PPS"), (5, "IDR")] {
        assert!(types.contains(&nal), "missing {name} in {types:?}");
    }
    assert!(
        units
            .iter()
            .skip(1)
            .all(|u| !nal_types(u).contains(&7) || u.keyframe)
    );
    let pts: Vec<u64> = units.iter().map(|u| u.pts_us).collect();
    assert!(
        pts.windows(2).all(|w| w[0] < w[1]),
        "no frame reordering: {pts:?}"
    );

    let mut decoder = openh264::decoder::Decoder::new().unwrap();
    let mut decoded = 0;
    for unit in &units {
        if let Some(yuv) = decoder.decode(&unit.data).expect("decodable AU") {
            use openh264::formats::YUVSource;
            assert_eq!(yuv.dimensions(), (WIDTH as usize, HEIGHT as usize));
            decoded += 1;
        }
    }
    decoded += decoder.flush_remaining().unwrap().len();
    assert_eq!(decoded, units.len());
}

#[test]
fn h265_keyframe_has_parameter_sets() {
    let mut encoder = match VtEncoder::new(config(VideoCodec::H265)) {
        Ok(encoder) => encoder,
        Err(e) => {
            eprintln!("skipping: HEVC encoder unavailable on this machine: {e}");
            return;
        }
    };
    let units = encode_all(&mut encoder);
    assert!(!units.is_empty(), "no access units");
    let first = &units[0];
    assert!(first.keyframe);
    let types = nal_types(first);
    for (nal, name) in [(32, "VPS"), (33, "SPS"), (34, "PPS")] {
        assert!(types.contains(&nal), "missing {name} in {types:?}");
    }
    assert!(
        types.iter().any(|t| (16..=21).contains(t)),
        "missing IRAP slice in {types:?}"
    );
}

#[test]
fn forced_keyframe_mid_stream_carries_parameter_sets() {
    let mut encoder = VtEncoder::new(config(VideoCodec::H264)).unwrap();
    let mut units = Vec::new();
    for i in 0..6 {
        units.extend(
            encoder
                .encode(&bgra_frame(i), i * 33_333, i == 0 || i == 4)
                .unwrap(),
        );
    }
    units.extend(encoder.flush().unwrap());
    let forced = units
        .iter()
        .find(|u| u.pts_us == 4 * 33_333)
        .expect("frame 4");
    assert!(forced.keyframe);
    assert!(nal_types(forced).contains(&7));
    assert!(units.iter().filter(|u| u.keyframe).count() >= 2);
    assert!(units.iter().any(|u| !u.keyframe));
}

#[test]
fn sink_receives_units_without_flush_polling() {
    let (tx, rx) = mpsc::channel();
    let mut encoder = VtEncoder::with_sink(config(VideoCodec::H264), move |unit| {
        let _ = tx.send(unit);
    })
    .unwrap();
    assert!(encoder.encode(&bgra_frame(0), 0, true).unwrap().is_empty());
    let unit = rx.recv_timeout(Duration::from_secs(5)).expect("sink unit");
    assert!(unit.keyframe);
    assert!(encoder.flush().unwrap().is_empty());
}

#[test]
fn odd_or_zero_sizes_are_rejected() {
    for (width, height) in [(321, 240), (0, 240), (320, 0)] {
        let cfg = VtEncoderConfig {
            width,
            height,
            ..config(VideoCodec::H264)
        };
        assert!(VtEncoder::new(cfg).is_err(), "{width}x{height}");
    }
}
