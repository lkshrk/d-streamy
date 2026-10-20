use discord_stream_rs::dave::DaveHandler;
use discord_stream_rs::voice::webrtc::WebRtcWrapper;
use discord_stream_rs::voice::{WebRtcParams, codec_payload};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::mpsc;
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS, MediaEngine};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::dtls_transport::dtls_role::DTLSRole;
use webrtc::ice::network_type::NetworkType;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};

const AUDIO_SSRC: u32 = 1111;
const VIDEO_SSRC: u32 = 2222;

fn loopback_settings(answerer: bool) -> SettingEngine {
    let mut s = SettingEngine::default();
    s.set_include_loopback_candidate(true);
    s.set_ip_filter(Box::new(|ip| ip.is_loopback()));
    s.set_network_types(vec![NetworkType::Udp4]);
    if answerer {
        s.set_answering_dtls_role(DTLSRole::Server).unwrap();
    }
    s
}

async fn fake_discord_peer() -> Arc<RTCPeerConnection> {
    let mut m = MediaEngine::default();
    m.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48_000,
                channels: 2,
                ..Default::default()
            },
            payload_type: codec_payload::OPUS.payload_type,
            ..Default::default()
        },
        RTPCodecType::Audio,
    )
    .unwrap();
    m.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90_000,
                ..Default::default()
            },
            payload_type: codec_payload::H264.payload_type,
            ..Default::default()
        },
        RTPCodecType::Video,
    )
    .unwrap();
    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_setting_engine(loopback_settings(true))
        .build();
    Arc::new(
        api.new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap(),
    )
}

/// Reduce a full SDP answer to the lines Discord's SELECT_PROTOCOL_ACK carries.
fn discord_style_sdp(answer: &str) -> String {
    let port = answer
        .lines()
        .find_map(|l| l.strip_prefix("a=candidate:"))
        .and_then(|c| c.split_whitespace().nth(5))
        .unwrap()
        .to_owned();
    let mut out = format!("m=audio {port} ICE/SDP\na=rtcp:{port}\n");
    for prefix in [
        "c=",
        "a=ice-ufrag",
        "a=ice-pwd",
        "a=fingerprint",
        "a=candidate",
    ] {
        let line = answer
            .lines()
            .map(str::trim_end)
            .find(|l| l.starts_with(prefix))
            .unwrap();
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn annexb(nalus: &[&[u8]]) -> Vec<u8> {
    nalus
        .iter()
        .flat_map(|n| [0u8, 0, 0, 1].iter().chain(n.iter()).copied())
        .collect()
}

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

    let server = fake_discord_peer().await;
    let (rtp_tx, mut rtp_rx) = mpsc::unbounded_channel();
    server.on_track(Box::new(move |track, _, _| {
        let tx = rtp_tx.clone();
        tokio::spawn(async move {
            while let Ok((pkt, _)) = track.read_rtp().await {
                let _ = tx.send(pkt);
            }
        });
        Box::pin(async {})
    }));
    server
        .set_remote_description(RTCSessionDescription::offer(offer).unwrap())
        .await
        .unwrap();
    let answer = server.create_answer(None).await.unwrap();
    let mut gathered = server.gathering_complete_promise().await;
    server.set_local_description(answer).await.unwrap();
    let _ = gathered.recv().await;
    let answer = server.local_description().await.unwrap().sdp;

    client
        .set_remote_answer(&discord_style_sdp(&answer))
        .await
        .unwrap();
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
        use webrtc::rtp::packetizer::Depacketizer;
        rebuilt.extend_from_slice(&depacketizer.depacketize(&p.payload).unwrap());
    }
    assert_eq!(rebuilt, frame);

    client.close().await;
    server.close().await.unwrap();
}
