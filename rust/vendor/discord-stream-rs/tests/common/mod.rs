#![allow(dead_code)]

use discord_stream_rs::voice::codec_payload;
use std::sync::Arc;
use tokio::sync::mpsc;
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS, MediaEngine};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::dtls_transport::dtls_role::DTLSRole;
use webrtc::ice::network_type::NetworkType;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp::packet::Packet;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};

pub fn loopback_settings(answerer: bool) -> SettingEngine {
    let mut s = SettingEngine::default();
    s.set_include_loopback_candidate(true);
    s.set_ip_filter(Box::new(|ip| ip.is_loopback()));
    s.set_network_types(vec![NetworkType::Udp4]);
    if answerer {
        s.set_answering_dtls_role(DTLSRole::Server).unwrap();
    }
    s
}

/// A webrtc-rs peer standing in for Discord's media server; received RTP goes to the channel.
pub async fn fake_discord_peer() -> (Arc<RTCPeerConnection>, mpsc::UnboundedReceiver<Packet>) {
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
    let pc = Arc::new(
        api.new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap(),
    );
    let (tx, rx) = mpsc::unbounded_channel();
    pc.on_track(Box::new(move |track, _, _| {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Ok((pkt, _)) = track.read_rtp().await {
                let _ = tx.send(pkt);
            }
        });
        Box::pin(async {})
    }));
    (pc, rx)
}

/// Answer the client's offer and return it reduced to the lines of Discord's SELECT_PROTOCOL_ACK SDP.
pub async fn answer_like_discord(server: &RTCPeerConnection, offer: String) -> String {
    server
        .set_remote_description(RTCSessionDescription::offer(offer).unwrap())
        .await
        .unwrap();
    let answer = server.create_answer(None).await.unwrap();
    let mut gathered = server.gathering_complete_promise().await;
    server.set_local_description(answer).await.unwrap();
    let _ = gathered.recv().await;
    discord_style_sdp(&server.local_description().await.unwrap().sdp)
}

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

pub fn annexb(nalus: &[&[u8]]) -> Vec<u8> {
    nalus
        .iter()
        .flat_map(|n| [0u8, 0, 0, 1].iter().chain(n.iter()).copied())
        .collect()
}
