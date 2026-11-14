mod common;

use common::{Client, T};
use discord_stream_rs::voice::VideoAttributes;
use discord_stream_rs::{MediaConnection, rewrite_sps_vui, split_nalu};
use dstreamy_media::{CaptureConfig, CaptureSource, MediaFrame, TestPatternSource};
use dstreamy_mock_discord::depacketize::{annexb, nal_units};
use dstreamy_mock_discord::opcodes::{gateway as gw, voice as vo};
use dstreamy_mock_discord::{
    MOCK_GUILD_ID, MOCK_VOICE_CHANNEL_ID, MockConfig, MockDiscord, VideoCodec, VoiceKind,
};
use std::time::Duration;

const VIDEO_FRAMES: usize = 60;

/// What the receiver must reassemble: the AU with SPS VUI rewritten, as NAL units.
fn expected_nalus(au: &[u8]) -> Vec<Vec<u8>> {
    split_nalu(au)
        .into_iter()
        .map(|n| {
            if n[0] & 0x1F == 7 {
                rewrite_sps_vui(n)
            } else {
                n.to_vec()
            }
        })
        .collect()
}

async fn send(conn: &MediaConnection, frame: &MediaFrame) {
    match frame {
        MediaFrame::Video(v) => conn
            .send_video_frame_at(&v.data, Duration::from_micros(v.pts_us))
            .await
            .unwrap(),
        MediaFrame::Audio(a) => conn
            .send_audio_frame_at(&a.data, Duration::from_micros(a.pts_us))
            .await
            .unwrap(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_test_pattern_bit_identical_with_dave() {
    let mock = MockDiscord::start().await.unwrap();
    let mut client = Client::connect(&mock).await;
    let guild = &client.ready["guilds"][0];
    assert_eq!(guild["id"], MOCK_GUILD_ID);
    assert!(
        guild["channels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == MOCK_VOICE_CHANNEL_ID && c["type"] == 2)
    );

    let mut voice = client
        .streamer
        .join_voice(Some(MOCK_GUILD_ID.into()), MOCK_VOICE_CHANNEL_ID.into());
    let _voice_conn = client.ready(&mut voice).await;
    let mut stream = client.streamer.create_stream().unwrap();
    let conn = client.ready(&mut stream).await;
    assert!(mock.dave_active(VoiceKind::Stream).await);
    assert_eq!(
        conn.voice_privacy_code(),
        mock.voice_privacy_code(VoiceKind::Stream).await
    );

    conn.set_packetizer("H264").await.unwrap();
    conn.set_speaking(true).unwrap();
    conn.set_video_attributes(Some(VideoAttributes {
        width: 320,
        height: 180,
        fps: 30,
    }))
    .unwrap();

    let mut source = TestPatternSource::unpaced();
    let mut rx = source
        .start(CaptureConfig {
            width: 320,
            height: 180,
            fps: 30,
            bitrate_bps: 400_000,
            ..CaptureConfig::default()
        })
        .await
        .unwrap();
    let (mut video, mut audio) = (Vec::new(), Vec::new());
    while video.len() < VIDEO_FRAMES {
        match tokio::time::timeout(T, rx.recv()).await.unwrap().unwrap() {
            MediaFrame::Video(v) => video.push(v),
            MediaFrame::Audio(a) => audio.push(a),
        }
    }
    source.stop().await.unwrap();
    assert!(video[0].keyframe);
    assert!(audio.len() >= 50);

    // Audio overtaking the paced first keyframe would make davey reject it (shared nonce sequence).
    send(&conn, &MediaFrame::Video(video[0].clone())).await;
    mock.wait_for(T, |r| {
        r.video
            .iter()
            .any(|f| f.kind == VoiceKind::Stream)
            .then_some(())
    })
    .await
    .expect("first keyframe recorded");
    let mut frames: Vec<MediaFrame> = video[1..]
        .iter()
        .cloned()
        .map(MediaFrame::Video)
        .chain(audio.iter().cloned().map(MediaFrame::Audio))
        .collect();
    frames.sort_by_key(MediaFrame::pts_us);
    let start = tokio::time::Instant::now();
    let first_pts = frames[0].pts_us();
    for frame in &frames {
        tokio::time::sleep_until(start + Duration::from_micros(frame.pts_us() - first_pts)).await;
        send(&conn, frame).await;
    }

    let (n_video, n_audio) = (video.len(), audio.len());
    let recorded = mock
        .wait_for(T, |r| {
            let v = r
                .video
                .iter()
                .filter(|f| f.kind == VoiceKind::Stream)
                .count();
            let a = r
                .audio
                .iter()
                .filter(|f| f.kind == VoiceKind::Stream)
                .count();
            (v >= n_video && a >= n_audio).then(|| r.clone())
        })
        .await
        .unwrap_or_else(|| {
            let r = mock.recorded();
            panic!(
                "media not recorded: {} of {n_video} video, {} of {n_audio} audio, dropped {:?}",
                r.video.len(),
                r.audio.len(),
                r.dropped_frames
            )
        });
    assert!(
        recorded.dropped_frames.is_empty(),
        "{:?}",
        recorded.dropped_frames
    );

    let rec_video: Vec<_> = recorded
        .video
        .iter()
        .filter(|f| f.kind == VoiceKind::Stream)
        .collect();
    assert_eq!(rec_video.len(), n_video);
    for (sent, got) in video.iter().zip(&rec_video) {
        assert!(got.e2ee, "video frame was not DAVE-encrypted");
        assert_eq!(got.codec, VideoCodec::H264);
        assert_eq!(got.payload_type, 101);
        assert_eq!(got.playout_delay, Some([0, 0, 10]));
        let got_nalus: Vec<Vec<u8>> = nal_units(&got.data)
            .into_iter()
            .map(<[u8]>::to_vec)
            .collect();
        assert_eq!(
            got_nalus,
            expected_nalus(&sent.data),
            "AU at {} µs differs",
            sent.pts_us
        );
        let ticks = got.rtp_timestamp.wrapping_sub(rec_video[0].rtp_timestamp);
        assert_eq!(u64::from(ticks), (sent.pts_us * 9 + 50) / 100);
    }
    assert!(rec_video.windows(2).all(|w| w[0].ssrc == w[1].ssrc));

    let rec_audio: Vec<_> = recorded
        .audio
        .iter()
        .filter(|f| f.kind == VoiceKind::Stream)
        .collect();
    assert_eq!(rec_audio.len(), n_audio);
    for (sent, got) in audio.iter().zip(&rec_audio) {
        assert!(got.e2ee, "audio frame was not DAVE-encrypted");
        assert_eq!(got.payload_type, 120);
        assert_eq!(got.data, sent.data);
        let ticks = got.rtp_timestamp.wrapping_sub(rec_audio[0].rtp_timestamp);
        assert_eq!(
            u64::from(ticks),
            (sent.pts_us - audio[0].pts_us) * 48 / 1000
        );
    }

    let ops = mock.received_ops();
    let stream_op = |op: u8| ops.iter().find(|o| o.is(Some(VoiceKind::Stream), op));
    let identify = stream_op(vo::OP_IDENTIFY).unwrap().json().unwrap().clone();
    assert_eq!(identify["max_dave_protocol_version"], 1);
    assert_eq!(
        identify["server_id"],
        mock.streams()[0].rtc_server_id.as_str()
    );
    for op in [
        vo::OP_SELECT_PROTOCOL,
        vo::OP_MLS_KEY_PACKAGE,
        vo::OP_MLS_COMMIT_WELCOME,
        vo::OP_DAVE_TRANSITION_READY,
        vo::OP_VIDEO,
    ] {
        assert!(stream_op(op).is_some(), "stream op {op} missing");
    }
    assert_eq!(
        stream_op(vo::OP_SPEAKING).unwrap().json().unwrap()["speaking"],
        2
    );
    assert!(mock.dave_active(VoiceKind::Voice).await);

    client.streamer.stop_stream();
    mock.wait_for_op(None, gw::OP_STREAM_DELETE, 0, T)
        .await
        .unwrap();
    assert!(mock.streams().is_empty());
    client.streamer.leave_voice();
    mock.wait_for(T, |r| {
        r.ops
            .iter()
            .filter(|o| o.is(None, gw::OP_VOICE_STATE_UPDATE))
            .any(|o| o.json().unwrap()["channel_id"].is_null())
            .then_some(())
    })
    .await
    .unwrap();
    assert!(mock.voice_state().is_none());
    mock.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn h265_access_units_reassemble_on_the_voice_connection() {
    let mock = MockDiscord::start_with(MockConfig {
        video_codec: VideoCodec::H265,
        ..MockConfig::default()
    })
    .await
    .unwrap();
    let mut client = Client::connect(&mock).await;
    let mut voice = client
        .streamer
        .join_voice(Some(MOCK_GUILD_ID.into()), MOCK_VOICE_CHANNEL_ID.into());
    let conn = client.ready(&mut voice).await;
    conn.set_packetizer("H265").await.unwrap();

    let mut idr = vec![0x26, 0x01];
    idr.extend((0..5000u32).map(|i| (i % 251) as u8 + 1));
    let au = annexb(&[
        &[0x40, 0x01, 0x0C],
        &[0x42, 0x01, 0x01],
        &[0x44, 0x01, 0xC1],
        &idr,
    ]);
    for i in 0..3u64 {
        conn.send_video_frame_at(&au, Duration::from_millis(33 * i))
            .await
            .unwrap();
    }
    let frames = mock
        .wait_for(T, |r| (r.video.len() >= 3).then(|| r.video.clone()))
        .await
        .unwrap();
    for f in frames {
        assert_eq!(
            (f.kind, f.codec, f.payload_type),
            (VoiceKind::Voice, VideoCodec::H265, 103)
        );
        assert!(f.e2ee);
        assert_eq!(f.data, au);
    }
    mock.shutdown().await;
}
