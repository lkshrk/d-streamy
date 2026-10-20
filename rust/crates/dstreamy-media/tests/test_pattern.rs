use std::time::{Duration, Instant};

use dstreamy_media::annexb::{self, H264_NAL_IDR, H264_NAL_PPS, H264_NAL_SPS};
use dstreamy_media::{
    AudioFrame, CaptureConfig, CaptureSource, EncodedVideoFrame, FrameReceiver, MediaError,
    MediaFrame, OPUS_CHANNELS, OPUS_FRAME_DURATION_US, OPUS_FRAME_SAMPLES, TEST_PATTERN_TARGET_ID,
    TestPatternSource, VideoCodec,
};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

const ONE_SECOND_US: u64 = 1_000_000;

fn config() -> CaptureConfig {
    CaptureConfig {
        width: 320,
        height: 180,
        fps: 30,
        bitrate_bps: 500_000,
        ..CaptureConfig::default()
    }
}

async fn collect_until(
    rx: &mut FrameReceiver,
    end_us: u64,
) -> (Vec<EncodedVideoFrame>, Vec<AudioFrame>) {
    let (mut video, mut audio) = (Vec::new(), Vec::new());
    while let Some(frame) = tokio::time::timeout(Duration::from_secs(30), rx.recv())
        .await
        .expect("frame within timeout")
    {
        match frame {
            MediaFrame::Video(v) if v.pts_us < end_us => video.push(v),
            MediaFrame::Audio(a) if a.pts_us < end_us => audio.push(a),
            MediaFrame::Video(_) => break,
            MediaFrame::Audio(_) => {}
        }
    }
    (video, audio)
}

fn nal_types(au: &[u8]) -> Vec<u8> {
    annexb::nal_units(au)
        .filter_map(annexb::h264_nal_type)
        .collect()
}

#[tokio::test]
async fn emits_one_second_of_decodable_keyframe_led_h264() {
    let mut source = TestPatternSource::unpaced();
    let cfg = config();
    let mut rx = source.start(cfg.clone()).await.unwrap();
    let (video, _) = collect_until(&mut rx, ONE_SECOND_US).await;
    source.stop().await.unwrap();

    assert!(video.len() >= 30, "only {} AUs in 1s", video.len());
    let first = &video[0];
    assert!(first.keyframe);
    let types = nal_types(&first.data);
    for t in [H264_NAL_SPS, H264_NAL_PPS, H264_NAL_IDR] {
        assert!(types.contains(&t), "keyframe NAL types {types:?}");
    }

    let mut decoder = Decoder::new().unwrap();
    let mut decoded = 0;
    let mut last_pts = None;
    for au in &video {
        assert_eq!(au.codec, VideoCodec::H264);
        assert!(au.data.starts_with(&annexb::START_CODE));
        assert_eq!(au.keyframe, annexb::h264_is_keyframe(&au.data));
        assert!(last_pts.is_none_or(|p| au.pts_us > p));
        last_pts = Some(au.pts_us);
        let picture = decoder
            .decode(&au.data)
            .unwrap_or_else(|e| panic!("AU at {} µs failed to decode: {e}", au.pts_us))
            .unwrap_or_else(|| panic!("AU at {} µs produced no picture", au.pts_us));
        assert_eq!(
            picture.dimensions(),
            (cfg.width as usize, cfg.height as usize)
        );
        decoded += 1;
    }
    assert!(decoded >= 30);
}

#[tokio::test]
async fn emits_opus_frames_every_20ms_that_decode_to_stereo() {
    let mut source = TestPatternSource::unpaced();
    let mut rx = source.start(config()).await.unwrap();
    let (_, audio) = collect_until(&mut rx, ONE_SECOND_US).await;
    source.stop().await.unwrap();

    assert!(audio.len() >= 49, "only {} Opus frames in 1s", audio.len());
    let mut err = 0;
    // SAFETY: valid constants; the decoder is destroyed below.
    let dec = unsafe { unsafe_libopus::opus_decoder_create(48_000, 2, &mut err) };
    assert_eq!(err, unsafe_libopus::OPUS_OK);
    let mut pcm = vec![0i16; OPUS_FRAME_SAMPLES * OPUS_CHANNELS];
    let mut peak = 0i16;
    for (i, frame) in audio.iter().enumerate() {
        assert_eq!(frame.pts_us, i as u64 * OPUS_FRAME_DURATION_US);
        // SAFETY: `pcm` holds one 20 ms stereo frame.
        let n = unsafe {
            unsafe_libopus::opus_decode(
                dec,
                frame.data.as_ptr(),
                frame.data.len() as i32,
                pcm.as_mut_ptr(),
                OPUS_FRAME_SAMPLES as i32,
                0,
            )
        };
        assert_eq!(n, OPUS_FRAME_SAMPLES as i32);
        peak = peak.max(pcm.iter().map(|s| s.saturating_abs()).max().unwrap());
    }
    // SAFETY: created above and not used afterwards.
    unsafe { unsafe_libopus::opus_decoder_destroy(dec) };
    assert!(peak > 3_000, "tone peak {peak}");
}

#[tokio::test]
async fn request_keyframe_forces_idr_mid_stream() {
    let mut source = TestPatternSource::unpaced();
    let mut rx = source.start(config()).await.unwrap();
    let mut seen = 0;
    let mut requested_at = None;
    while let Some(frame) = rx.recv().await {
        let MediaFrame::Video(v) = frame else {
            continue;
        };
        seen += 1;
        if seen == 5 {
            source.request_keyframe();
            requested_at = Some(seen);
        } else if requested_at.is_some() && v.keyframe {
            break;
        }
        assert!(seen < 5 + 20, "no keyframe after request");
    }
    source.stop().await.unwrap();
}

#[tokio::test]
async fn stop_closes_stream_and_allows_restart() {
    let mut source = TestPatternSource::unpaced();
    let mut rx = source.start(config()).await.unwrap();
    assert!(matches!(
        source.start(config()).await,
        Err(MediaError::AlreadyRunning)
    ));
    rx.recv().await.unwrap();
    source.stop().await.unwrap();
    while rx.recv().await.is_some() {}
    source.stop().await.unwrap();

    let mut rx = source.start(config()).await.unwrap();
    let MediaFrame::Video(first) = rx.recv().await.unwrap() else {
        panic!("stream must start with video");
    };
    assert!(first.keyframe);
    source.stop().await.unwrap();
}

#[tokio::test]
async fn stop_does_not_hang_when_receiver_stalls() {
    let mut source = TestPatternSource::unpaced();
    let _rx = source.start(config()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    tokio::time::timeout(Duration::from_secs(5), source.stop())
        .await
        .expect("stop returned")
        .unwrap();
}

#[tokio::test]
async fn paces_frames_in_real_time() {
    let mut source = TestPatternSource::new();
    let mut rx = source.start(config()).await.unwrap();
    let started = Instant::now();
    let mut video = 0;
    while video < 10 {
        if let MediaFrame::Video(_) = rx.recv().await.unwrap() {
            video += 1;
        }
    }
    source.stop().await.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(250));
}

#[tokio::test]
async fn rejects_unsupported_config() {
    let mut source = TestPatternSource::unpaced();
    let h265 = CaptureConfig {
        codec: VideoCodec::H265,
        ..config()
    };
    assert!(matches!(
        source.start(h265).await,
        Err(MediaError::Unsupported(_))
    ));
    let odd = CaptureConfig {
        width: 321,
        ..config()
    };
    assert!(matches!(
        source.start(odd).await,
        Err(MediaError::InvalidConfig(_))
    ));
}

#[tokio::test]
async fn audio_can_be_disabled() {
    let mut source = TestPatternSource::unpaced();
    let mut rx = source
        .start(CaptureConfig {
            audio: false,
            ..config()
        })
        .await
        .unwrap();
    let (video, audio) = collect_until(&mut rx, ONE_SECOND_US / 2).await;
    source.stop().await.unwrap();
    assert!(!video.is_empty());
    assert!(audio.is_empty());
}

#[tokio::test]
async fn dropping_the_source_ends_the_stream() {
    let mut source = TestPatternSource::new();
    let mut rx = source.start(config()).await.unwrap();
    drop(source);
    tokio::time::timeout(Duration::from_secs(5), async {
        while rx.recv().await.is_some() {}
    })
    .await
    .expect("stream closed after drop");
}

#[tokio::test]
async fn lists_the_test_pattern_display() {
    let targets = TestPatternSource::new().list_targets().await.unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].id, TEST_PATTERN_TARGET_ID);
}
