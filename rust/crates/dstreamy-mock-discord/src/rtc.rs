//! The WebRTC side of Discord's media server: a webrtc-rs answerer that receives SRTP.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::depacketize::VideoCodec;
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

pub type RtcError = webrtc::Error;

/// Loopback-only ICE over UDPv4; `answerer` makes this side the DTLS server, as Discord is.
pub fn loopback_settings(answerer: bool) -> SettingEngine {
    let mut s = SettingEngine::default();
    s.set_include_loopback_candidate(true);
    s.set_ip_filter(Box::new(|ip| ip.is_loopback()));
    s.set_network_types(vec![NetworkType::Udp4]);
    if answerer {
        s.set_answering_dtls_role(DTLSRole::Server)
            .expect("answering DTLS role");
    }
    s
}

/// A webrtc-rs peer standing in for Discord's media server, accepting Opus and `video_codec`; RTP goes to the channel.
pub async fn answerer_peer(
    video_codec: VideoCodec,
    audio_payload_type: u8,
    video_payload_type: u8,
) -> Result<(Arc<RTCPeerConnection>, mpsc::UnboundedReceiver<Packet>), RtcError> {
    let mut m = MediaEngine::default();
    // webrtc-rs drops a partially matched H264 (no fmtp in the offer) when another codec matches exactly.
    let video = match video_codec {
        VideoCodec::H264 => (
            MIME_TYPE_H264,
            90_000,
            0,
            video_payload_type,
            RTPCodecType::Video,
        ),
        VideoCodec::H265 => (
            "video/H265",
            90_000,
            0,
            video_payload_type,
            RTPCodecType::Video,
        ),
    };
    let codecs = [
        (
            MIME_TYPE_OPUS,
            48_000,
            2,
            audio_payload_type,
            RTPCodecType::Audio,
        ),
        video,
    ];
    for (mime_type, clock_rate, channels, payload_type, kind) in codecs {
        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: mime_type.to_owned(),
                    clock_rate,
                    channels,
                    ..Default::default()
                },
                payload_type,
                ..Default::default()
            },
            kind,
        )?;
    }
    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_setting_engine(loopback_settings(true))
        .build();
    let pc = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await?);
    let (tx, rx) = mpsc::unbounded_channel();
    pc.on_track(Box::new(move |track, _, _| {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Ok((pkt, _)) = track.read_rtp().await {
                if tx.send(pkt).is_err() {
                    break;
                }
            }
        });
        Box::pin(async {})
    }));
    Ok((pc, rx))
}

/// Answer the client's offer and return it reduced to the lines of Discord's SELECT_PROTOCOL_ACK SDP.
pub async fn answer_like_discord(
    server: &RTCPeerConnection,
    offer: String,
) -> Result<String, RtcError> {
    server
        .set_remote_description(RTCSessionDescription::offer(offer)?)
        .await?;
    let answer = server.create_answer(None).await?;
    let mut gathered = server.gathering_complete_promise().await;
    server.set_local_description(answer).await?;
    let _ = gathered.recv().await;
    let local = server
        .local_description()
        .await
        .ok_or(webrtc::Error::ErrNoRemoteDescription)?;
    discord_style_sdp(&local.sdp).ok_or(webrtc::Error::ErrSessionDescriptionMissingIceUfrag)
}

/// Discord's SDP: `m=audio <port> ICE/SDP`, `a=rtcp:<port>` and the transport lines only.
pub fn discord_style_sdp(answer: &str) -> Option<String> {
    let port = answer
        .lines()
        .find_map(|l| l.strip_prefix("a=candidate:"))
        .and_then(|c| c.split_whitespace().nth(5))?
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
            .find(|l| l.starts_with(prefix))?;
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}
