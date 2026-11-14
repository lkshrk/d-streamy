#![cfg(target_os = "linux")]

use std::time::Duration;

use dstreamy_capture_linux::{LinuxCaptureSource, PORTAL_TARGET_ID};
use dstreamy_media::{CaptureConfig, CaptureSource, FrameReceiver, MediaFrame, annexb};
use openh264::decoder::Decoder;

async fn collect(rx: &mut FrameReceiver, wanted_video: usize) -> (Vec<Vec<u8>>, usize) {
    let mut video = Vec::new();
    let mut audio = 0;
    while video.len() < wanted_video {
        let frame = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("no frame within 10 s")
            .expect("capture ended early");
        match frame {
            MediaFrame::Video(v) => {
                if video.is_empty() {
                    assert!(v.keyframe, "first AU must be a keyframe");
                }
                video.push(v.data);
            }
            MediaFrame::Audio(_) => audio += 1,
        }
    }
    (video, audio)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a desktop session with the ScreenCast portal; pick a screen in the dialog"]
async fn portal_capture_yields_decodable_video_audio_and_a_restore_token() {
    let mut source = LinuxCaptureSource::new();
    let targets = source.list_targets().await.unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].id, PORTAL_TARGET_ID);

    let mut rx = source.start(CaptureConfig::default()).await.unwrap();
    eprintln!("encoder: {:?}", source.encoder());
    let (video, audio) = collect(&mut rx, 60).await;
    source.request_keyframe();
    source.stop().await.unwrap();
    while rx.recv().await.is_some() {}

    assert!(annexb::h264_is_keyframe(&video[0]));
    let mut decoder = Decoder::new().unwrap();
    let decoded = video
        .iter()
        .filter(|au| matches!(decoder.decode(au), Ok(Some(_))))
        .count();
    assert!(decoded >= 30, "only {decoded} AUs decoded");
    assert!(audio > 0, "no system audio frames");

    let token = source.restore_token().map(str::to_owned);
    assert!(token.is_some(), "portal returned no restore token");

    let mut restored = LinuxCaptureSource::new().with_restore_token(token);
    let mut rx = restored.start(CaptureConfig::default()).await.unwrap();
    collect(&mut rx, 30).await;
    restored.stop().await.unwrap();
}
