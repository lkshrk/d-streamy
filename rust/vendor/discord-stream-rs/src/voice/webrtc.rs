//! WebRTC transport, ported from `WebRtcWrapper.ts`.

use crate::dave::DaveHandler;
use crate::processing::{
    AnnexBHelpers, H264Helpers, H264NalUnitType, START_CODE_3, rewrite_sps_vui, split_nalu,
};
use crate::utils::{VideoCodec, normalize_video_codec};
use crate::voice::codec_payload::{self, ALL_VIDEO_CODECS, VideoCodecInfo};
use crate::voice::connection::WebRtcParams;
use crate::voice::packetizer::{PayloadFormat, RtpPacketizer};
use async_trait::async_trait;
use davey::{Codec, MediaType};
use std::any::Any;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, warn};
use webrtc::{
    api::{
        APIBuilder,
        interceptor_registry::configure_rtcp_reports,
        media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS, MIME_TYPE_VP8, MIME_TYPE_VP9, MediaEngine},
        setting_engine::SettingEngine,
    },
    ice_transport::ice_server::RTCIceServer,
    interceptor::{nack::responder::Responder, registry::Registry},
    peer_connection::{
        RTCPeerConnection, configuration::RTCConfiguration,
        peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
    rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication,
    rtp::packet::Packet,
    rtp_transceiver::{
        RTCPFeedback, SSRC,
        rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType},
    },
    track::track_local::{TrackLocal, TrackLocalContext, TrackLocalWriter},
};

/// ICE servers configured by the JS reference.
pub const DEFAULT_ICE_SERVERS: &[&str] = &["stun:stun.l.google.com:19302"];

/// Video pacing rate of the JS reference (`PacingHandler(25 * 1000 * 1000, 1)`).
pub const VIDEO_PACING_BITS_PER_SECOND: f64 = 25_000_000.0;

const AUDIO_PLAYOUT_DELAY: (u16, u16) = (0, 1);
const VIDEO_PLAYOUT_DELAY: (u16, u16) = (0, 10);

#[derive(Debug, Error)]
pub enum WebRtcError {
    #[error("WebRTC not initialized")]
    NotInitialized,
    #[error("Packetizer not configured")]
    PacketizerNotConfigured,
    #[error("Peer connection is not connected")]
    NotConnected,
    #[error("Unknown video codec: {0}")]
    UnknownCodec(String),
    #[error("No packetizer for video codec {0}")]
    UnsupportedCodec(VideoCodec),
    #[error("WebRTC error: {0}")]
    Webrtc(String),
    #[error("DAVE error: {0}")]
    Dave(String),
}

fn webrtc_err(e: impl std::fmt::Display) -> WebRtcError {
    WebRtcError::Webrtc(e.to_string())
}

/// A local track that sends pre-built RTP packets unchanged on a fixed SSRC.
pub struct DiscordTrack {
    id: String,
    kind: RTPCodecType,
    mime_type: String,
    ssrc: SSRC,
    writer: Mutex<Option<Arc<dyn TrackLocalWriter + Send + Sync>>>,
}

impl DiscordTrack {
    pub fn new(kind: RTPCodecType, mime_type: &str, ssrc: SSRC) -> Self {
        let id = match kind {
            RTPCodecType::Audio => "audio",
            _ => "video",
        };
        Self {
            id: id.to_owned(),
            kind,
            mime_type: mime_type.to_owned(),
            ssrc,
            writer: Mutex::new(None),
        }
    }

    pub async fn write_rtp(&self, packet: &Packet) -> Result<usize, WebRtcError> {
        let writer = self.writer.lock().await.clone();
        match writer {
            Some(w) => w.write_rtp(packet).await.map_err(webrtc_err),
            None => Ok(0),
        }
    }
}

#[async_trait]
impl TrackLocal for DiscordTrack {
    async fn bind(&self, t: &TrackLocalContext) -> webrtc::error::Result<RTCRtpCodecParameters> {
        let codec = t
            .codec_parameters()
            .iter()
            .find(|c| c.capability.mime_type.eq_ignore_ascii_case(&self.mime_type))
            .or_else(|| t.codec_parameters().first())
            .cloned()
            .ok_or(webrtc::Error::ErrUnsupportedCodec)?;
        *self.writer.lock().await = Some(t.write_stream());
        Ok(codec)
    }

    async fn unbind(&self, _t: &TrackLocalContext) -> webrtc::error::Result<()> {
        *self.writer.lock().await = None;
        Ok(())
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn rid(&self) -> Option<&str> {
        None
    }

    fn stream_id(&self) -> &str {
        "discord-stream-rs"
    }

    fn kind(&self) -> RTPCodecType {
        self.kind
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn preferred_ssrc(&self) -> Option<SSRC> {
        Some(self.ssrc)
    }
}

struct Pacer {
    tx: mpsc::UnboundedSender<Packet>,
    task: JoinHandle<()>,
}

impl Pacer {
    fn spawn(track: Arc<DiscordTrack>, bits_per_second: f64) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_pacer(track, rx, bits_per_second / 8.0));
        Self { tx, task }
    }
}

impl Drop for Pacer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run_pacer(
    track: Arc<DiscordTrack>,
    mut rx: mpsc::UnboundedReceiver<Packet>,
    bytes_per_second: f64,
) {
    let burst = bytes_per_second / 1000.0;
    let mut budget = burst;
    let mut last = Instant::now();
    while let Some(packet) = rx.recv().await {
        let size = (packet.payload.len() + 12) as f64;
        loop {
            let now = Instant::now();
            budget = (budget + now.duration_since(last).as_secs_f64() * bytes_per_second)
                .min(burst.max(size));
            last = now;
            if budget >= size {
                budget -= size;
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        if let Err(e) = track.write_rtp(&packet).await {
            debug!("video RTP write failed: {}", e);
        }
    }
}

pub struct WebRtcWrapper {
    peer_connection: Option<Arc<RTCPeerConnection>>,
    audio_track: Option<Arc<DiscordTrack>>,
    video_track: Option<Arc<DiscordTrack>>,
    pacer: Option<Pacer>,
    audio: Option<RtpPacketizer>,
    video: Option<RtpPacketizer>,
    video_codec: Option<VideoCodec>,
    ice_servers: Vec<String>,
    setting_engine: SettingEngine,
    dave: Arc<StdMutex<DaveHandler>>,
}

impl WebRtcWrapper {
    pub fn new(dave: Arc<StdMutex<DaveHandler>>) -> Self {
        Self {
            peer_connection: None,
            audio_track: None,
            video_track: None,
            pacer: None,
            audio: None,
            video: None,
            video_codec: None,
            ice_servers: DEFAULT_ICE_SERVERS.iter().map(|s| s.to_string()).collect(),
            setting_engine: SettingEngine::default(),
            dave,
        }
    }

    pub fn set_setting_engine(&mut self, engine: SettingEngine) {
        self.setting_engine = engine;
    }

    pub fn set_ice_servers(&mut self, urls: Vec<String>) {
        self.ice_servers = urls;
    }

    /// Packetizer state survives re-initialisation, like `_setMediaHandler()`.
    pub async fn init(
        &mut self,
        params: &WebRtcParams,
    ) -> Result<Arc<RTCPeerConnection>, WebRtcError> {
        self.close_peer_connection().await;

        let mut media_engine = MediaEngine::default();
        media_engine
            .register_codec(
                RTCRtpCodecParameters {
                    capability: RTCRtpCodecCapability {
                        mime_type: MIME_TYPE_OPUS.to_owned(),
                        clock_rate: codec_payload::OPUS.clock_rate,
                        channels: 2,
                        sdp_fmtp_line: "minptime=10;useinbandfec=1;usedtx=1".to_owned(),
                        rtcp_feedback: vec![feedback("nack", "")],
                    },
                    payload_type: codec_payload::OPUS.payload_type,
                    ..Default::default()
                },
                RTPCodecType::Audio,
            )
            .map_err(webrtc_err)?;
        for vc in ALL_VIDEO_CODECS {
            media_engine
                .register_codec(
                    RTCRtpCodecParameters {
                        capability: RTCRtpCodecCapability {
                            mime_type: video_mime(vc).to_owned(),
                            clock_rate: vc.clock_rate,
                            channels: 0,
                            sdp_fmtp_line: String::new(),
                            rtcp_feedback: vec![
                                feedback("ccm", "fir"),
                                feedback("nack", ""),
                                feedback("nack", "pli"),
                                feedback("goog-remb", ""),
                            ],
                        },
                        payload_type: vc.payload_type,
                        ..Default::default()
                    },
                    RTPCodecType::Video,
                )
                .map_err(webrtc_err)?;
        }

        let mut registry = Registry::new();
        registry.add(Box::new(Responder::builder()));
        registry = configure_rtcp_reports(registry);

        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_setting_engine(self.setting_engine.clone())
            .build();
        let config = RTCConfiguration {
            ice_servers: if self.ice_servers.is_empty() {
                vec![]
            } else {
                vec![RTCIceServer {
                    urls: self.ice_servers.clone(),
                    ..Default::default()
                }]
            },
            ..Default::default()
        };
        let pc = Arc::new(api.new_peer_connection(config).await.map_err(webrtc_err)?);

        let audio = Arc::new(DiscordTrack::new(
            RTPCodecType::Audio,
            MIME_TYPE_OPUS,
            params.audio_ssrc,
        ));
        let video = Arc::new(DiscordTrack::new(
            RTPCodecType::Video,
            self.video_codec
                .map_or(MIME_TYPE_H264, |c| video_mime(&video_info(c))),
            params.video_ssrc,
        ));
        let tracks: [Arc<dyn TrackLocal + Send + Sync>; 2] = [audio.clone(), video.clone()];
        for track in tracks {
            let sender = pc.add_track(track).await.map_err(webrtc_err)?;
            // RTCP must be read for the NACK responder and report interceptors to run.
            tokio::spawn(async move {
                while let Ok((packets, _)) = sender.read_rtcp().await {
                    if packets
                        .iter()
                        .any(|p| p.as_any().is::<PictureLossIndication>())
                    {
                        debug!("PLI received");
                    }
                }
            });
        }

        self.pacer = Some(Pacer::spawn(video.clone(), VIDEO_PACING_BITS_PER_SECOND));
        self.audio_track = Some(audio);
        self.video_track = Some(video);
        self.peer_connection = Some(pc.clone());
        debug!("WebRTC peer connection initialized");
        Ok(pc)
    }

    /// Create the local offer that goes into SELECT_PROTOCOL.
    pub async fn create_offer(&self) -> Result<String, WebRtcError> {
        let pc = self
            .peer_connection
            .as_ref()
            .ok_or(WebRtcError::NotInitialized)?;
        let offer = pc.create_offer(None).await.map_err(webrtc_err)?;
        pc.set_local_description(offer).await.map_err(webrtc_err)?;
        let local = pc
            .local_description()
            .await
            .ok_or(WebRtcError::NotInitialized)?;
        Ok(local.sdp)
    }

    /// Apply Discord's SELECT_PROTOCOL_ACK SDP as the remote answer.
    pub async fn set_remote_answer(&self, discord_sdp: &str) -> Result<(), WebRtcError> {
        let pc = self
            .peer_connection
            .as_ref()
            .ok_or(WebRtcError::NotInitialized)?;
        let answer =
            RTCSessionDescription::answer(build_answer_sdp(discord_sdp)).map_err(webrtc_err)?;
        pc.set_remote_description(answer).await.map_err(webrtc_err)
    }

    /// Equivalent of `setPacketizer()`.
    pub fn set_packetizer(
        &mut self,
        params: &WebRtcParams,
        video_codec: &str,
    ) -> Result<(), WebRtcError> {
        let codec =
            normalize_video_codec(video_codec).map_err(|e| WebRtcError::UnknownCodec(e.0))?;
        if !matches!(codec, VideoCodec::H264 | VideoCodec::H265 | VideoCodec::Av1) {
            return Err(WebRtcError::UnsupportedCodec(codec));
        }
        let info = video_info(codec);
        self.audio = Some(RtpPacketizer::new(
            params.audio_ssrc,
            codec_payload::OPUS.payload_type,
            codec_payload::OPUS.clock_rate,
            PayloadFormat::Opus,
            AUDIO_PLAYOUT_DELAY,
        ));
        self.video = Some(RtpPacketizer::new(
            params.video_ssrc,
            info.payload_type,
            info.clock_rate,
            PayloadFormat::Video(codec),
            VIDEO_PLAYOUT_DELAY,
        ));
        self.video_codec = Some(codec);
        debug!(
            "RTP packetizer configured: audio_ssrc={} video_ssrc={} codec={:?}",
            params.audio_ssrc, params.video_ssrc, codec
        );
        Ok(())
    }

    /// Equivalent of `sendAudioFrame(frame, frametime)` for one Opus frame.
    pub async fn send_audio_frame(
        &mut self,
        frame: &[u8],
        frametime_ms: f64,
    ) -> Result<(), WebRtcError> {
        self.send_audio(frame, Timing::Advance(frametime_ms)).await
    }

    /// Send one Opus frame with an RTP timestamp derived from `pts`.
    pub async fn send_audio_frame_at(
        &mut self,
        frame: &[u8],
        pts: Duration,
    ) -> Result<(), WebRtcError> {
        self.send_audio(frame, Timing::Pts(pts)).await
    }

    /// Equivalent of `sendVideoFrame(frame, frametime)` for one Annex B access unit.
    pub async fn send_video_frame(
        &mut self,
        frame: &[u8],
        frametime_ms: f64,
    ) -> Result<(), WebRtcError> {
        self.send_video(frame, Timing::Advance(frametime_ms))
    }

    /// Send one Annex B access unit with an RTP timestamp derived from `pts`.
    pub async fn send_video_frame_at(
        &mut self,
        frame: &[u8],
        pts: Duration,
    ) -> Result<(), WebRtcError> {
        self.send_video(frame, Timing::Pts(pts))
    }

    async fn send_audio(&mut self, frame: &[u8], timing: Timing) -> Result<(), WebRtcError> {
        if !self.is_ready() {
            return Err(WebRtcError::NotConnected);
        }
        let track = self
            .audio_track
            .clone()
            .ok_or(WebRtcError::NotInitialized)?;
        let packetizer = self
            .audio
            .as_mut()
            .ok_or(WebRtcError::PacketizerNotConfigured)?;
        let payload = lock_dave(&self.dave)
            .encrypt_opus(frame)
            .map_err(|e| WebRtcError::Dave(e.to_string()))?;
        let packets = timing.packetize(packetizer, &payload);
        for packet in &packets {
            track.write_rtp(packet).await?;
        }
        Ok(())
    }

    fn send_video(&mut self, frame: &[u8], timing: Timing) -> Result<(), WebRtcError> {
        if !self.is_ready() {
            return Err(WebRtcError::NotConnected);
        }
        let pacer = self.pacer.as_ref().ok_or(WebRtcError::NotInitialized)?;
        let codec = self
            .video_codec
            .ok_or(WebRtcError::PacketizerNotConfigured)?;
        let packetizer = self
            .video
            .as_mut()
            .ok_or(WebRtcError::PacketizerNotConfigured)?;
        let rewritten;
        let frame = if codec == VideoCodec::H264 {
            rewritten = rewrite_h264_sps_vui(frame);
            rewritten.as_deref().unwrap_or(frame)
        } else {
            frame
        };
        let payload = lock_dave(&self.dave)
            .encrypt(MediaType::VIDEO, dave_codec(codec), frame)
            .map_err(|e| WebRtcError::Dave(e.to_string()))?;
        for packet in timing.packetize(packetizer, &payload) {
            if pacer.tx.send(packet).is_err() {
                warn!("video pacer stopped");
                return Err(WebRtcError::NotInitialized);
            }
        }
        Ok(())
    }

    pub fn is_ready(&self) -> bool {
        self.peer_connection
            .as_ref()
            .is_some_and(|pc| pc.connection_state() == RTCPeerConnectionState::Connected)
    }

    async fn close_peer_connection(&mut self) {
        self.pacer = None;
        self.audio_track = None;
        self.video_track = None;
        if let Some(pc) = self.peer_connection.take() {
            let _ = pc.close().await;
        }
    }

    pub async fn close(&mut self) {
        self.close_peer_connection().await;
        self.audio = None;
        self.video = None;
    }

    pub fn peer_connection(&self) -> Option<&Arc<RTCPeerConnection>> {
        self.peer_connection.as_ref()
    }

    pub fn video_codec(&self) -> Option<VideoCodec> {
        self.video_codec
    }
}

#[derive(Clone, Copy)]
enum Timing {
    Advance(f64),
    Pts(Duration),
}

impl Timing {
    fn packetize(self, packetizer: &mut RtpPacketizer, payload: &[u8]) -> Vec<Packet> {
        match self {
            Timing::Advance(ms) => {
                let packets = packetizer.packetize(payload);
                packetizer.advance(ms);
                packets
            }
            Timing::Pts(pts) => {
                packetizer.set_pts(pts);
                packetizer.packetize(payload)
            }
        }
    }
}

// A poisoned DAVE lock only means another task panicked mid-call; the state is still usable.
fn lock_dave(dave: &StdMutex<DaveHandler>) -> std::sync::MutexGuard<'_, DaveHandler> {
    dave.lock().unwrap_or_else(|e| e.into_inner())
}

fn feedback(typ: &str, parameter: &str) -> RTCPFeedback {
    RTCPFeedback {
        typ: typ.to_owned(),
        parameter: parameter.to_owned(),
    }
}

fn video_info(codec: VideoCodec) -> VideoCodecInfo {
    match codec {
        VideoCodec::H264 => codec_payload::H264,
        VideoCodec::H265 => codec_payload::H265,
        VideoCodec::Vp8 => codec_payload::VP8,
        VideoCodec::Vp9 => codec_payload::VP9,
        VideoCodec::Av1 => codec_payload::AV1,
    }
}

fn video_mime(info: &VideoCodecInfo) -> &'static str {
    match info.name {
        "H264" => MIME_TYPE_H264,
        "H265" => "video/H265",
        "VP8" => MIME_TYPE_VP8,
        "VP9" => MIME_TYPE_VP9,
        _ => "video/AV1",
    }
}

fn dave_codec(codec: VideoCodec) -> Codec {
    match codec {
        VideoCodec::H264 => Codec::H264,
        VideoCodec::H265 => Codec::H265,
        VideoCodec::Vp8 => Codec::VP8,
        VideoCodec::Vp9 => Codec::VP9,
        VideoCodec::Av1 => Codec::AV1,
    }
}

/// `None` when the frame has no SPS, so it is sent untouched.
fn rewrite_h264_sps_vui(frame: &[u8]) -> Option<Vec<u8>> {
    let nalus = split_nalu(frame);
    if !nalus
        .iter()
        .any(|n| H264Helpers::nal_unit_type(n) == H264NalUnitType::Sps as u8)
    {
        return None;
    }
    let mut out = Vec::with_capacity(frame.len() + 16);
    for nalu in nalus {
        out.extend_from_slice(&START_CODE_3);
        if H264Helpers::nal_unit_type(nalu) == H264NalUnitType::Sps as u8 {
            out.extend_from_slice(&rewrite_sps_vui(nalu));
        } else {
            out.extend_from_slice(nalu);
        }
    }
    Some(out)
}

/// Build the remote answer like `handleProtocolAck()`, but `recvonly`: webrtc-rs never sends on `inactive`.
pub fn build_answer_sdp(discord_sdp: &str) -> String {
    let (mut ip, mut port, mut ufrag, mut pwd, mut fingerprint, mut candidate) = (
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
    );
    for line in discord_sdp.lines().map(str::trim_end) {
        if line.starts_with("c=") {
            ip = line.to_owned();
        } else if line.starts_with("a=rtcp") {
            port = line.split(':').nth(1).unwrap_or_default().to_owned();
        } else if line.starts_with("a=ice-ufrag") {
            ufrag = line.to_owned();
        } else if line.starts_with("a=ice-pwd") {
            pwd = line.to_owned();
        } else if line.starts_with("a=fingerprint") {
            fingerprint = line.to_owned();
        } else if line.starts_with("a=candidate") {
            candidate = line.to_owned();
        }
    }
    let common = format!(
        "{ip}\r\n{{EXT}}a=setup:passive\r\n{{MID}}a=recvonly\r\n{ufrag}\r\n{pwd}\r\n{fingerprint}\r\n{candidate}\r\na=rtcp-mux\r\n"
    );
    let opus = codec_payload::OPUS.payload_type;
    let audio = format!("m=audio {port} UDP/TLS/RTP/SAVPF {opus}\r\n")
        + &common
            .replace(
                "{EXT}",
                "a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\n\
                 a=extmap:3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n",
            )
            .replace("{MID}", "a=mid:0\r\na=maxptime:60\r\n")
        + &format!(
            "a=rtpmap:{opus} opus/48000/2\r\n\
             a=fmtp:{opus} minptime=10;useinbandfec=1;usedtx=1\r\n\
             a=rtcp-fb:{opus} transport-cc\r\n\
             a=rtcp-fb:{opus} nack\r\n"
        );
    let video_pts: Vec<String> = ALL_VIDEO_CODECS
        .iter()
        .flat_map(|c| [c.payload_type.to_string(), c.rtx_payload_type.to_string()])
        .collect();
    let mut video = format!("m=video {port} UDP/TLS/RTP/SAVPF {}\r\n", video_pts.join(" "))
        + &common
            .replace(
                "{EXT}",
                "a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\n\
                 a=extmap:3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n\
                 a=extmap:14 urn:ietf:params:rtp-hdrext:toffset\r\n\
                 a=extmap:13 urn:3gpp:video-orientation\r\n\
                 a=extmap:5 http://www.webrtc.org/experiments/rtp-hdrext/playout-delay\r\n",
            )
            .replace("{MID}", "a=mid:1\r\n");
    for c in ALL_VIDEO_CODECS {
        let (pt, rtx) = (c.payload_type, c.rtx_payload_type);
        video += &format!(
            "a=rtpmap:{pt} {}/90000\r\n\
             a=rtpmap:{rtx} rtx/90000\r\n\
             a=fmtp:{rtx} apt={pt}\r\n\
             a=rtcp-fb:{pt} ccm fir\r\n\
             a=rtcp-fb:{pt} nack\r\n\
             a=rtcp-fb:{pt} nack pli\r\n\
             a=rtcp-fb:{pt} goog-remb\r\n\
             a=rtcp-fb:{pt} transport-cc\r\n",
            c.name
        );
    }
    format!(
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1\r\na=ice-lite\r\n{audio}{video}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISCORD_SDP: &str = "m=audio 50003 ICE/SDP\n\
        a=fingerprint:sha-256 4A:79:94:16:44:3F:BD:05:41:5A:C7:20:F3:12:54:70:00:73:5D:33:00:2D:2C:80:9B:39:E1:9F:2D:A7:49:87\n\
        c=IN IP4 203.0.113.7\n\
        a=rtcp:50003\n\
        a=ice-ufrag:PkLE\n\
        a=ice-pwd:o9QGn2N6YizFOM/UNojYai\n\
        a=fingerprint:sha-256 4A:79:94:16:44:3F:BD:05:41:5A:C7:20:F3:12:54:70:00:73:5D:33:00:2D:2C:80:9B:39:E1:9F:2D:A7:49:87\n\
        a=candidate:1 1 UDP 4261412862 203.0.113.7 50003 typ host\n";

    #[test]
    fn answer_is_parseable_and_carries_discord_transport() {
        let sdp = build_answer_sdp(DISCORD_SDP);
        RTCSessionDescription::answer(sdp.clone()).unwrap();
        let parsed =
            webrtc::sdp::SessionDescription::unmarshal(&mut std::io::Cursor::new(sdp.as_bytes()))
                .unwrap();
        assert_eq!(parsed.media_descriptions.len(), 2);
        for md in &parsed.media_descriptions {
            assert_eq!(md.media_name.port.value, 50003);
            assert_eq!(md.attribute("ice-ufrag").flatten(), Some("PkLE"));
            assert!(md.attribute("recvonly").is_some());
        }
        assert!(sdp.contains("a=candidate:1 1 UDP 4261412862 203.0.113.7 50003 typ host\r\n"));
        assert!(sdp.contains("a=rtpmap:103 H265/90000\r\n"));
        assert!(
            sdp.contains(
                "a=extmap:5 http://www.webrtc.org/experiments/rtp-hdrext/playout-delay\r\n"
            )
        );
    }

    #[test]
    fn sps_rewrite_only_when_sps_present() {
        let idr = [0, 0, 0, 1, 0x65, 0x88, 0x84];
        assert!(rewrite_h264_sps_vui(&idr).is_none());
        let with_sps = [
            0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1F, 0xED, 0x00, 0xA0, 0x0B, 0x72, 0, 0, 1, 0x65, 0x88,
        ];
        let out = rewrite_h264_sps_vui(&with_sps).unwrap();
        let nalus = split_nalu(&out);
        assert_eq!(nalus.len(), 2);
        assert!(nalus[0].len() > 9);
        assert_eq!(nalus[1], [0x65, 0x88]);
    }
}
