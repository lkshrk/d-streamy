#![cfg(target_os = "macos")]
//! Real ScreenCaptureKit capture; needs the Screen Recording permission, so run with `--ignored`.

use std::time::Duration;

use dstreamy_capture_macos::MacCaptureSource;
use dstreamy_media::annexb::{h264_nal_type, nal_units};
use dstreamy_media::{CaptureConfig, CaptureSource, CropRect, MediaFrame, TargetKind, VideoCodec};

async fn collect(config: CaptureConfig, secs: u64) -> (Vec<MediaFrame>, MacCaptureSource) {
    let mut source = MacCaptureSource::new();
    let mut rx = source.start(config).await.expect("start capture");
    let mut frames = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while let Ok(Some(frame)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        frames.push(frame);
    }
    source.stop().await.expect("stop");
    let drained = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(frame) = rx.recv().await {
            frames.push(frame);
        }
    });
    assert!(drained.await.is_ok(), "channel still open after stop");
    (frames, source)
}

fn videos(frames: &[MediaFrame]) -> Vec<&dstreamy_media::EncodedVideoFrame> {
    frames
        .iter()
        .filter_map(|f| match f {
            MediaFrame::Video(v) => Some(v),
            MediaFrame::Audio(_) => None,
        })
        .collect()
}

#[tokio::test]
#[ignore = "needs Screen Recording permission"]
async fn lists_displays_and_windows() {
    let targets = MacCaptureSource::new().list_targets().await.unwrap();
    for t in &targets {
        println!(
            "{:?} {} {:?} {}x{} {}",
            t.kind, t.id, t.app, t.width, t.height, t.title
        );
    }
    assert!(
        targets
            .iter()
            .any(|t| t.kind == TargetKind::Display && t.id.starts_with("display:"))
    );
}

#[tokio::test]
#[ignore = "needs Screen Recording permission"]
async fn captures_main_display_as_h264_with_audio() {
    let (frames, _) = collect(CaptureConfig::default(), 3).await;
    let video = videos(&frames);
    assert!(!video.is_empty(), "no video");
    assert!(video[0].keyframe);
    assert!(nal_units(&video[0].data).any(|n| h264_nal_type(n) == Some(7)));
    let mut decoder = openh264::decoder::Decoder::new().unwrap();
    for v in &video {
        decoder.decode(&v.data).expect("decodable AU");
    }
    let audio = frames
        .iter()
        .filter(|f| matches!(f, MediaFrame::Audio(_)))
        .count();
    println!("video {} audio {audio}", video.len());
}

#[tokio::test]
#[ignore = "needs Screen Recording permission"]
async fn captures_cropped_display_as_h265() {
    let config = CaptureConfig {
        crop: Some(CropRect {
            x: 0,
            y: 0,
            width: 640,
            height: 360,
        }),
        codec: VideoCodec::H265,
        audio: false,
        ..CaptureConfig::default()
    };
    let (frames, _) = collect(config, 2).await;
    let video = videos(&frames);
    assert!(!video.is_empty(), "no video");
    assert!(video[0].keyframe);
    assert!(frames.iter().all(|f| matches!(f, MediaFrame::Video(_))));
}

#[tokio::test]
#[ignore = "needs Screen Recording permission"]
async fn captures_first_user_window() {
    let mut source = MacCaptureSource::new();
    let targets = source.list_targets().await.unwrap();
    let Some(window) = targets.into_iter().find(|t| t.kind == TargetKind::Window) else {
        eprintln!("no user window open");
        return;
    };
    let mut rx = source
        .start(CaptureConfig {
            target_id: Some(window.id),
            ..CaptureConfig::default()
        })
        .await
        .unwrap();
    source.request_keyframe();
    let first = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match rx.recv().await {
                Some(MediaFrame::Video(v)) => return v,
                Some(MediaFrame::Audio(_)) => {}
                None => panic!("channel closed"),
            }
        }
    })
    .await
    .expect("video frame");
    assert!(first.keyframe);
    source.stop().await.unwrap();
}
