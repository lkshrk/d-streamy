mod common;

use common::{annexb, answer_like_discord, fake_discord_peer, loopback_settings};
use discord_stream_rs::dave::DaveHandler;
use discord_stream_rs::voice::webrtc::WebRtcWrapper;
use discord_stream_rs::voice::{WebRtcParams, codec_payload};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp::packetizer::Depacketizer;

const AUDIO_SSRC: u32 = 1111;
const VIDEO_SSRC: u32 = 2222;

#[tokio::test(flavor = "multi_thread")]
async fn sends_rtp_on_discord_ssrcs_and_payload_types() {
    let params = WebRtcParams {
        address: "127.0.0.1".into(),
        port: 0,
        audio_ssrc: AUDIO_SSRC,
        video_ssrc: VIDEO_SSRC,
        rtx_ssrc: VIDEO_SSRC + 1,
        supported_encryption_modes: vec![],
    };
    let mut client = WebRtcWrapper::new(Arc::new(StdMutex::new(DaveHandler::new(1, 2))));
    client.set_ice_servers(vec![]);
    client.set_setting_engine(loopback_settings(false));
    let client_pc = client.init(&params).await.unwrap();
    let offer = client.create_offer().await.unwrap();
    assert!(offer.contains(&format!("a=ssrc:{VIDEO_SSRC} ")));
    assert!(offer.contains(&format!(
        "a=fmtp:{} {}\r\n",
        codec_payload::H264.payload_type,
        codec_payload::H264.fmtp
    )));

    let (server, mut rtp_rx) = fake_discord_peer().await;
    let discord_sdp = answer_like_discord(&server, offer).await;
    client.set_remote_answer(&discord_sdp).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while client_pc.connection_state() != RTCPeerConnectionState::Connected {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("peer connection did not connect");

    client.set_packetizer(&params, "H264").unwrap();
    let mut idr = vec![0x65];
    idr.extend((0..4000u32).map(|i| (i % 200) as u8 + 1));
    let frame = annexb(&[&[0x09, 0xF0], &[0x68, 0xCE, 0x3C, 0x80], &idr]);
    let opus = [0xFC, 0xFF, 0xFE];

    let mut video = Vec::new();
    let mut audio = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            client
                .send_video_frame(&frame, 1000.0 / 30.0)
                .await
                .unwrap();
            client.send_audio_frame(&opus, 20.0).await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
            while let Ok(pkt) = rtp_rx.try_recv() {
                match pkt.header.ssrc {
                    VIDEO_SSRC => video.push(pkt),
                    AUDIO_SSRC => audio.push(pkt),
                    other => panic!("unexpected ssrc {other}"),
                }
            }
            let complete_frame = video.iter().filter(|p| p.header.marker).count() >= 2;
            if complete_frame && !audio.is_empty() {
                break;
            }
        }
    })
    .await
    .expect("no media received");

    assert!(
        video
            .iter()
            .all(|p| p.header.payload_type == codec_payload::H264.payload_type)
    );
    assert!(
        audio
            .iter()
            .all(|p| p.header.payload_type == codec_payload::OPUS.payload_type)
    );
    assert_eq!(audio[0].payload[..], opus);

    let start = video.iter().position(|p| p.payload[0] & 0x1F == 9).unwrap();
    let end = start + video[start..].iter().position(|p| p.header.marker).unwrap();
    let mut depacketizer = webrtc::rtp::codecs::h264::H264Packet::default();
    let mut rebuilt = Vec::new();
    for p in &video[start..=end] {
        rebuilt.extend_from_slice(&depacketizer.depacketize(&p.payload).unwrap());
    }
    assert_eq!(rebuilt, frame);

    let mut keyframe_requests = client.keyframe_requests();
    let pli = PictureLossIndication {
        sender_ssrc: 0,
        media_ssrc: VIDEO_SSRC,
    };
    server.write_rtcp(&[Box::new(pli)]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), keyframe_requests.changed())
        .await
        .expect("PLI not surfaced")
        .unwrap();
    assert_eq!(*keyframe_requests.borrow(), 1);

    client.close().await;
    server.close().await.unwrap();
}
