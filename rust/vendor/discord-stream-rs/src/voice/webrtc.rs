//! WebRTC transport, ported from `WebRtcWrapper.ts`.

use crate::dave::DaveHandler;
use crate::processing::{
    AnnexBHelpers, H264Helpers, H264NalUnitType, H265Helpers, START_CODE_3, rewrite_sps_vui,
    split_nalu,
};
use crate::utils::{VideoCodec, normalize_video_codec};
use crate::voice::codec_payload::{self, ALL_VIDEO_CODECS, VideoCodecInfo};
use crate::voice::connection::WebRtcParams;
use crate::voice::packetizer::{PayloadFormat, RtpPacketizer};
use async_trait::async_trait;
use davey::{Codec, MediaType};
use std::any::Any;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{Mutex, Notify, watch};
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
    rtcp::payload_feedbacks::{
        full_intra_request::FullIntraRequest, picture_loss_indication::PictureLossIndication,
    },
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
/// Queued video bytes; whole frames are dropped beyond it.
pub const PACER_QUEUE_BYTES: usize = 10 * 1024 * 1024;
/// An uncompressed 1920x1080 4:2:0 picture; a larger frame would hold the send order (and audio) for over a second.
pub const MAX_VIDEO_FRAME_BYTES: usize = 1920 * 1080 * 3 / 2;

#[derive(Debug, Error)]
pub enum WebRtcError {
    #[error("WebRTC not initialized")]
    NotInitialized,
    #[error("Packetizer not configured")]
    PacketizerNotConfigured,
    #[error("Peer connection is not connected")]
    NotConnected,
    #[error("Video pacer queue is full; frame dropped")]
    PacerQueueFull,
    #[error("Video pacer stopped")]
    PacerStopped,
    #[error("Video frame of {0} bytes exceeds the {MAX_VIDEO_FRAME_BYTES}-byte limit")]
    FrameTooLarge(usize),
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

/// What became of the video frames `send_video` accepted, cumulative across peer connection restarts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VideoProgress {
    /// Frames `send_video` queued; once the pacer settles, the sum of sent, dropped and flushed.
    pub accepted_frames: u64,
    /// Frames whose last packet was written without a write error.
    pub sent_frames: u64,
    pub sent_bytes: u64,
    /// Frames not fully sent: DAVE encrypt failed, the packetizer was gone, an RTP write failed or the pacer stopped.
    pub dropped_frames: u64,
    /// Frames dropped unstarted by `flush_video`, which the caller already knows about.
    pub flushed_frames: u64,
    /// Keyframes the pacer dropped as stale after a flush; `flush_video` already reports the ones it drains.
    pub stale_keyframes: u64,
    pub last_error: Option<String>,
}

impl VideoProgress {
    pub fn settled(&self) -> bool {
        self.sent_frames + self.dropped_frames + self.flushed_frames >= self.accepted_frames
    }
}

struct PacedFrame {
    data: Vec<u8>,
    codec: VideoCodec,
    timing: Timing,
    /// The flush generation the caller saw; a later flush makes the frame stale.
    generation: u64,
}

impl PacedFrame {
    /// Only Annex B codecs are paced (`set_packetizer`); others never report a keyframe.
    fn is_keyframe(&self) -> bool {
        let irap = |n: &&[u8]| match self.codec {
            VideoCodec::H264 => {
                H264Helpers::nal_unit_type(n) == H264NalUnitType::CodedSliceIdr as u8
            }
            VideoCodec::H265 => (16..=21).contains(&H265Helpers::nal_unit_type(n)),
            VideoCodec::Vp8 | VideoCodec::Vp9 | VideoCodec::Av1 => false,
        };
        split_nalu(&self.data).iter().any(irap)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Flushed {
    pub frames: u64,
    pub keyframe: bool,
}

/// The video path's flush and liveness, shared with `MediaConnection` so neither waits on the wrapper's lock.
pub struct VideoGate {
    pacer: StdMutex<Option<Arc<PacerShared>>>,
    generation: AtomicU64,
    progress: Arc<watch::Sender<VideoProgress>>,
}

impl VideoGate {
    fn new(progress: Arc<watch::Sender<VideoProgress>>) -> Self {
        Self {
            pacer: StdMutex::default(),
            generation: AtomicU64::default(),
            progress,
        }
    }

    pub fn video_progress(&self) -> watch::Receiver<VideoProgress> {
        self.progress.subscribe()
    }

    fn pacer(&self) -> Option<Arc<PacerShared>> {
        self.pacer.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Frames queued with an earlier generation are dropped unstarted.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Drops queued video the pacer has not started, counting it in `VideoProgress::flushed_frames`; the frame being sent finishes.
    pub fn flush(&self) -> Flushed {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.pacer().map_or_else(Flushed::default, |p| p.flush())
    }
}

/// Frames accepted by `send_video` that the pacer has not started; a started frame is no longer here.
#[derive(Default)]
struct PacerQueue {
    frames: StdMutex<VecDeque<PacedFrame>>,
    ready: Notify,
}

impl PacerQueue {
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<PacedFrame>> {
        self.frames.lock().unwrap_or_else(|e| e.into_inner())
    }

    async fn next(&self) -> PacedFrame {
        loop {
            let notified = self.ready.notified();
            if let Some(frame) = self.lock().pop_front() {
                return frame;
            }
            notified.await;
        }
    }
}

struct PacerShared {
    track: Arc<DiscordTrack>,
    packetizer: Arc<StdMutex<Option<RtpPacketizer>>>,
    dave: Arc<StdMutex<DaveHandler>>,
    send_order: Arc<Mutex<()>>,
    queue: Arc<PacerQueue>,
    queued_bytes: Arc<AtomicUsize>,
    outstanding: Arc<AtomicU64>,
    progress: Arc<watch::Sender<VideoProgress>>,
    gate: Arc<VideoGate>,
    alive: AtomicBool,
}

impl PacerShared {
    /// False when the stop already counted the frame as dropped.
    fn settle(&self) -> bool {
        self.outstanding
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .is_ok()
    }

    fn enqueue(&self, frame: PacedFrame) -> Result<(), WebRtcError> {
        let len = frame.data.len();
        let mut queue = self.queue.lock();
        if !self.alive.load(Ordering::Acquire) {
            return Err(WebRtcError::PacerStopped);
        }
        if self.queued_bytes.load(Ordering::Relaxed) + len > PACER_QUEUE_BYTES {
            return Err(WebRtcError::PacerQueueFull);
        }
        self.queued_bytes.fetch_add(len, Ordering::Relaxed);
        self.outstanding.fetch_add(1, Ordering::AcqRel);
        queue.push_back(frame);
        drop(queue);
        self.progress.send_modify(|p| p.accepted_frames += 1);
        self.queue.ready.notify_one();
        Ok(())
    }

    fn flush(&self) -> Flushed {
        let frames: Vec<PacedFrame> = self.queue.lock().drain(..).collect();
        self.flushed(&frames, false)
    }

    fn flushed(&self, frames: &[PacedFrame], stale: bool) -> Flushed {
        let bytes: usize = frames.iter().map(|f| f.data.len()).sum();
        self.queued_bytes.fetch_sub(bytes, Ordering::Relaxed);
        let counted: Vec<&PacedFrame> = frames.iter().filter(|_| self.settle()).collect();
        let keyframes = counted.iter().filter(|f| f.is_keyframe()).count() as u64;
        if !counted.is_empty() {
            debug!("video pacer flushed {} queued frames", counted.len());
            self.progress.send_modify(|p| {
                p.flushed_frames += counted.len() as u64;
                if stale {
                    p.stale_keyframes += keyframes;
                }
            });
        }
        Flushed {
            frames: counted.len() as u64,
            keyframe: keyframes > 0,
        }
    }

    /// Counts every frame not yet settled as dropped; later settles find nothing to claim.
    fn stop(&self, why: &str) {
        let mut queue = self.queue.lock();
        self.alive.store(false, Ordering::Release);
        let lost = self.outstanding.swap(0, Ordering::AcqRel);
        queue.clear();
        drop(queue);
        if lost > 0 {
            warn!("video pacer {} with {} frames unsent", why, lost);
            self.progress.send_modify(|p| {
                p.dropped_frames += lost;
                p.last_error = Some(format!("video pacer {why} with {lost} frames unsent"));
            });
        }
    }
}

struct Pacer {
    shared: Arc<PacerShared>,
    task: JoinHandle<()>,
}

impl Pacer {
    fn spawn(shared: PacerShared, bits_per_second: f64) -> Self {
        let shared = Arc::new(shared);
        *shared.gate.pacer.lock().unwrap_or_else(|e| e.into_inner()) = Some(shared.clone());
        let task = tokio::spawn(run_pacer(shared.clone(), bits_per_second / 8.0));
        Self { shared, task }
    }

    fn enqueue(&self, frame: PacedFrame) -> Result<(), WebRtcError> {
        if self.task.is_finished() {
            self.shared.stop("task ended");
            return Err(WebRtcError::PacerStopped);
        }
        self.shared.enqueue(frame)
    }

    #[cfg(test)]
    fn flush(&self) -> Flushed {
        self.shared.flush()
    }
}

impl Drop for Pacer {
    fn drop(&mut self) {
        self.task.abort();
        let mut current = self
            .shared
            .gate
            .pacer
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if current
            .as_ref()
            .is_some_and(|p| Arc::ptr_eq(p, &self.shared))
        {
            *current = None;
        }
        drop(current);
        self.shared.stop("stopped");
    }
}

/// Marks the pacer dead when its task ends for any reason, panics included.
struct PacerExit(Arc<PacerShared>);

impl Drop for PacerExit {
    fn drop(&mut self) {
        self.0.stop("task ended");
    }
}

/// DAVE-encrypts each frame when it is sent, so nonces follow wire order on both tracks.
async fn run_pacer(shared: Arc<PacerShared>, bytes_per_second: f64) {
    let _exit = PacerExit(shared.clone());
    let burst = bytes_per_second / 1000.0;
    let mut budget = burst;
    let mut last = Instant::now();
    loop {
        let frame = shared.queue.next().await;
        let order = shared.send_order.lock().await;
        if frame.generation < shared.gate.generation() {
            drop(order);
            shared.flushed(std::slice::from_ref(&frame), true);
            continue;
        }
        let encrypted =
            lock_dave(&shared.dave).encrypt(MediaType::VIDEO, dave_codec(frame.codec), &frame.data);
        let packets = match encrypted {
            Ok(payload) => lock_packetizer(&shared.packetizer)
                .as_mut()
                .map(|p| frame.timing.packetize(p, &payload))
                .ok_or_else(|| WebRtcError::PacketizerNotConfigured.to_string()),
            Err(e) => Err(WebRtcError::Dave(e.to_string()).to_string()),
        };
        let packets = match packets {
            Ok(packets) => packets,
            Err(e) => {
                drop(order);
                shared
                    .queued_bytes
                    .fetch_sub(frame.data.len(), Ordering::Relaxed);
                if shared.settle() {
                    warn!("video frame dropped in the pacer: {}", e);
                    shared.progress.send_modify(|p| {
                        p.dropped_frames += 1;
                        p.last_error = Some(e);
                    });
                }
                continue;
            }
        };
        let mut write_error = None;
        for packet in &packets {
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
            if let Err(e) = shared.track.write_rtp(packet).await {
                write_error.get_or_insert(e);
            }
        }
        drop(order);
        shared
            .queued_bytes
            .fetch_sub(frame.data.len(), Ordering::Relaxed);
        if !shared.settle() {
            continue;
        }
        match write_error {
            Some(e) => {
                warn!("video RTP write failed: {}", e);
                shared.progress.send_modify(|p| {
                    p.dropped_frames += 1;
                    p.last_error = Some(e.to_string());
                });
            }
            None => shared.progress.send_modify(|p| {
                p.sent_frames += 1;
                p.sent_bytes += frame.data.len() as u64;
            }),
        }
    }
}

pub struct WebRtcWrapper {
    peer_connection: Option<Arc<RTCPeerConnection>>,
    audio_track: Option<Arc<DiscordTrack>>,
    video_track: Option<Arc<DiscordTrack>>,
    pacer: Option<Pacer>,
    audio: Option<RtpPacketizer>,
    video: Arc<StdMutex<Option<RtpPacketizer>>>,
    // Held from DAVE encrypt until a frame's last packet is written: receivers decrypt a frame once it is complete.
    send_order: Arc<Mutex<()>>,
    video_gate: Arc<VideoGate>,
    video_codec: Option<VideoCodec>,
    ice_servers: Vec<String>,
    setting_engine: SettingEngine,
    dave: Arc<StdMutex<DaveHandler>>,
    keyframe_requests: Arc<watch::Sender<u64>>,
    video_progress: Arc<watch::Sender<VideoProgress>>,
}

impl WebRtcWrapper {
    pub fn new(dave: Arc<StdMutex<DaveHandler>>) -> Self {
        let video_progress = Arc::new(watch::channel(VideoProgress::default()).0);
        Self {
            peer_connection: None,
            audio_track: None,
            video_track: None,
            pacer: None,
            audio: None,
            video: Arc::default(),
            send_order: Arc::default(),
            video_gate: Arc::new(VideoGate::new(video_progress.clone())),
            video_codec: None,
            ice_servers: DEFAULT_ICE_SERVERS.iter().map(|s| s.to_string()).collect(),
            setting_engine: SettingEngine::default(),
            dave,
            keyframe_requests: Arc::new(watch::channel(0).0),
            video_progress,
        }
    }

    pub fn video_progress(&self) -> watch::Receiver<VideoProgress> {
        self.video_progress.subscribe()
    }

    /// Count of PLI and FIR packets received from viewers, across peer connection restarts.
    pub fn keyframe_requests(&self) -> watch::Receiver<u64> {
        self.keyframe_requests.subscribe()
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
                        sdp_fmtp_line: codec_payload::OPUS.fmtp.to_owned(),
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
                            sdp_fmtp_line: vc.fmtp.to_owned(),
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
            let keyframe_requests = self.keyframe_requests.clone();
            // RTCP must be read for the NACK responder and report interceptors to run.
            tokio::spawn(async move {
                while let Ok((packets, _)) = sender.read_rtcp().await {
                    if packets.iter().any(|p| {
                        p.as_any().is::<PictureLossIndication>()
                            || p.as_any().is::<FullIntraRequest>()
                    }) {
                        debug!("PLI/FIR received");
                        keyframe_requests.send_modify(|n| *n += 1);
                    }
                }
            });
        }

        self.pacer = Some(Pacer::spawn(
            PacerShared {
                track: video.clone(),
                packetizer: self.video.clone(),
                dave: self.dave.clone(),
                send_order: self.send_order.clone(),
                queue: Arc::default(),
                queued_bytes: Arc::default(),
                outstanding: Arc::default(),
                progress: self.video_progress.clone(),
                gate: self.video_gate.clone(),
                alive: AtomicBool::new(true),
            },
            VIDEO_PACING_BITS_PER_SECOND,
        ));
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
        if !matches!(codec, VideoCodec::H264 | VideoCodec::H265) {
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
        *lock_packetizer(&self.video) = Some(RtpPacketizer::new(
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
        self.send_video(frame, Timing::Advance(frametime_ms), None)
    }

    /// Send one Annex B access unit with an RTP timestamp derived from `pts`.
    pub async fn send_video_frame_at(
        &mut self,
        frame: &[u8],
        pts: Duration,
    ) -> Result<(), WebRtcError> {
        self.send_video(frame, Timing::Pts(pts), None)
    }

    /// Like `send_video_frame_at`; the frame is dropped unstarted if a flush comes after `generation`.
    pub async fn send_video_frame_at_generation(
        &mut self,
        frame: &[u8],
        pts: Duration,
        generation: u64,
    ) -> Result<(), WebRtcError> {
        self.send_video(frame, Timing::Pts(pts), Some(generation))
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
        let _order = self.send_order.lock().await;
        let payload = lock_dave(&self.dave)
            .encrypt_opus(frame)
            .map_err(|e| WebRtcError::Dave(e.to_string()))?;
        let packets = timing.packetize(packetizer, &payload);
        for packet in &packets {
            track.write_rtp(packet).await?;
        }
        Ok(())
    }

    fn send_video(
        &mut self,
        frame: &[u8],
        timing: Timing,
        generation: Option<u64>,
    ) -> Result<(), WebRtcError> {
        if frame.len() > MAX_VIDEO_FRAME_BYTES {
            warn!(
                "video frame of {} bytes refused: above the {}-byte sanity cap",
                frame.len(),
                MAX_VIDEO_FRAME_BYTES
            );
            return Err(WebRtcError::FrameTooLarge(frame.len()));
        }
        if !self.is_ready() {
            return Err(WebRtcError::NotConnected);
        }
        let pacer = self.pacer.as_ref().ok_or(WebRtcError::NotInitialized)?;
        let codec = self
            .video_codec
            .filter(|_| lock_packetizer(&self.video).is_some())
            .ok_or(WebRtcError::PacketizerNotConfigured)?;
        let data = if codec == VideoCodec::H264 {
            rewrite_h264_sps_vui(frame).unwrap_or_else(|| frame.to_vec())
        } else {
            frame.to_vec()
        };
        pacer.enqueue(PacedFrame {
            data,
            codec,
            timing,
            generation: generation.unwrap_or_else(|| self.video_gate.generation()),
        })
    }

    pub fn video_gate(&self) -> Arc<VideoGate> {
        self.video_gate.clone()
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
        *lock_packetizer(&self.video) = None;
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

fn lock_packetizer(
    packetizer: &StdMutex<Option<RtpPacketizer>>,
) -> std::sync::MutexGuard<'_, Option<RtpPacketizer>> {
    packetizer.lock().unwrap_or_else(|e| e.into_inner())
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
    let (opus, opus_fmtp) = (codec_payload::OPUS.payload_type, codec_payload::OPUS.fmtp);
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
             a=fmtp:{opus} {opus_fmtp}\r\n\
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
        // webrtc-rs drops partially matched codecs whenever another one matches exactly.
        if !c.fmtp.is_empty() {
            video += &format!("a=fmtp:{pt} {}\r\n", c.fmtp);
        }
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
        assert!(sdp.contains(
            "a=fmtp:101 profile-level-id=42e01f;packetization-mode=1;level-asymmetry-allowed=1\r\n"
        ));
        assert!(sdp.contains(
            "a=fmtp:120 minptime=10;maxaveragebitrate=96000;stereo=1;sprop-stereo=1;useinbandfec=1\r\n"
        ));
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

    #[derive(Debug)]
    enum TestWriter {
        Ok,
        Fails,
        Hangs,
        /// Holds every write until opened; records the RTP timestamp of each frame's marker packet.
        Gate(Arc<watch::Sender<bool>>, Arc<StdMutex<Vec<u32>>>),
    }

    #[async_trait]
    impl TrackLocalWriter for TestWriter {
        async fn write_rtp_with_attributes(
            &self,
            pkt: &Packet,
            _: &webrtc::interceptor::Attributes,
        ) -> webrtc::error::Result<usize> {
            match self {
                TestWriter::Ok => Ok(pkt.payload.len()),
                TestWriter::Fails => Err(webrtc::Error::ErrClosedPipe),
                TestWriter::Hangs => std::future::pending().await,
                TestWriter::Gate(open, written) => {
                    let _ = open.subscribe().wait_for(|o| *o).await;
                    if pkt.header.marker {
                        written.lock().unwrap().push(pkt.header.timestamp);
                    }
                    Ok(pkt.payload.len())
                }
            }
        }

        async fn write(&self, b: &[u8]) -> webrtc::error::Result<usize> {
            Ok(b.len())
        }
    }

    async fn pacer(writer: TestWriter) -> (Pacer, watch::Receiver<VideoProgress>) {
        let (pacer, rx, _) = gated_pacer(writer).await;
        (pacer, rx)
    }

    async fn gated_pacer(
        writer: TestWriter,
    ) -> (Pacer, watch::Receiver<VideoProgress>, Arc<VideoGate>) {
        let progress = Arc::new(watch::channel(VideoProgress::default()).0);
        let gate = Arc::new(VideoGate::new(progress.clone()));
        let track = Arc::new(DiscordTrack::new(RTPCodecType::Video, MIME_TYPE_H264, 2));
        *track.writer.lock().await = Some(Arc::new(writer));
        let rx = progress.subscribe();
        let packetizer = RtpPacketizer::new(
            2,
            codec_payload::H264.payload_type,
            90_000,
            PayloadFormat::Video(VideoCodec::H264),
            VIDEO_PLAYOUT_DELAY,
        );
        let shared = PacerShared {
            track,
            packetizer: Arc::new(StdMutex::new(Some(packetizer))),
            dave: Arc::new(StdMutex::new(DaveHandler::new(1, 2))),
            send_order: Arc::default(),
            queue: Arc::default(),
            queued_bytes: Arc::default(),
            outstanding: Arc::default(),
            progress,
            gate: gate.clone(),
            alive: AtomicBool::new(true),
        };
        (Pacer::spawn(shared, VIDEO_PACING_BITS_PER_SECOND), rx, gate)
    }

    fn frame(pts_ms: u64) -> PacedFrame {
        slice(pts_ms, 0x65, 0)
    }

    fn slice(pts_ms: u64, nal: u8, generation: u64) -> PacedFrame {
        PacedFrame {
            data: vec![0, 0, 0, 1, nal].into_iter().chain([7; 2995]).collect(),
            codec: VideoCodec::H264,
            timing: Timing::Pts(Duration::from_millis(pts_ms)),
            generation,
        }
    }

    fn queue(pacer: &Pacer, frames: usize) {
        for i in 0..frames {
            pacer.enqueue(frame(33 * i as u64)).unwrap();
        }
    }

    async fn settled(rx: &mut watch::Receiver<VideoProgress>, frames: u64) -> VideoProgress {
        tokio::time::timeout(
            Duration::from_secs(5),
            rx.wait_for(|p| p.sent_frames + p.dropped_frames + p.flushed_frames >= frames),
        )
        .await
        .expect("pacer progress")
        .unwrap()
        .clone()
    }

    #[tokio::test]
    async fn a_frame_counts_as_sent_once_its_last_packet_is_written() {
        let (pacer, mut rx) = pacer(TestWriter::Ok).await;
        queue(&pacer, 3);
        let p = settled(&mut rx, 3).await;
        assert_eq!(
            (p.sent_frames, p.sent_bytes, p.dropped_frames),
            (3, 9000, 0)
        );
        drop(pacer);
        assert_eq!(rx.borrow().dropped_frames, 0);
    }

    #[tokio::test]
    async fn a_frame_with_a_failed_write_counts_as_dropped() {
        let (pacer, mut rx) = pacer(TestWriter::Fails).await;
        queue(&pacer, 2);
        let p = settled(&mut rx, 2).await;
        assert_eq!((p.sent_frames, p.dropped_frames), (0, 2));
        assert!(p.last_error.is_some());
    }

    #[tokio::test]
    async fn frames_queued_or_in_flight_when_the_pacer_stops_count_as_dropped_once() {
        let (pacer, rx) = pacer(TestWriter::Hangs).await;
        queue(&pacer, 4);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!rx.borrow().settled());
        drop(pacer);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let p = rx.borrow().clone();
        assert_eq!(
            (p.accepted_frames, p.sent_frames, p.dropped_frames),
            (4, 0, 4)
        );
        assert!(p.settled());
    }

    #[tokio::test]
    async fn a_frame_finishing_after_the_stop_counted_it_is_not_published_again() {
        let (pacer, rx) = pacer(TestWriter::Hangs).await;
        queue(&pacer, 1);
        let shared = pacer.shared.clone();
        drop(pacer);
        assert!(!shared.settle());
        assert_eq!(rx.borrow().dropped_frames, 1);
    }

    #[tokio::test]
    async fn a_flush_drops_unstarted_frames_once_and_lets_the_one_in_flight_finish() {
        let open = Arc::new(watch::channel(false).0);
        let written = Arc::new(StdMutex::new(Vec::new()));
        let (pacer, mut rx) = pacer(TestWriter::Gate(open.clone(), written.clone())).await;
        queue(&pacer, 5);
        tokio::time::timeout(Duration::from_secs(5), async {
            while pacer.shared.queue.lock().len() == 5 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("first frame started");
        assert_eq!(pacer.flush().frames, 4);
        assert_eq!(pacer.flush().frames, 0);
        assert_eq!(pacer.shared.queued_bytes.load(Ordering::Relaxed), 3000);
        assert_eq!(rx.borrow().flushed_frames, 4);
        pacer.enqueue(frame(1000)).unwrap();
        open.send_replace(true);
        let p = settled(&mut rx, 6).await;
        assert_eq!(
            (p.sent_frames, p.dropped_frames, p.flushed_frames),
            (2, 0, 4)
        );
        let written = written.lock().unwrap().clone();
        assert_eq!(written.len(), 2);
        assert_eq!(
            written[1].wrapping_sub(written[0]),
            90_000,
            "the frame queued after the flush went next"
        );
        assert_eq!(pacer.shared.queued_bytes.load(Ordering::Relaxed), 0);
        assert_eq!(pacer.shared.outstanding.load(Ordering::Acquire), 0);
        drop(pacer);
        assert_eq!(rx.borrow().dropped_frames, 0);
    }

    #[tokio::test]
    async fn a_flush_after_the_stop_counted_the_frames_counts_none() {
        let (pacer, rx) = pacer(TestWriter::Hangs).await;
        queue(&pacer, 3);
        let lost = pacer.shared.outstanding.swap(0, Ordering::AcqRel);
        assert_eq!(lost, 3);
        assert_eq!(pacer.flush().frames, 0);
        assert_eq!(rx.borrow().flushed_frames, 0);
    }

    async fn started(pacer: &Pacer, queued: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while pacer.shared.queue.lock().len() == queued {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("first frame started");
    }

    #[tokio::test]
    async fn the_gate_drains_queued_frames_and_reports_a_keyframe() {
        let (pacer, rx, gate) = gated_pacer(TestWriter::Hangs).await;
        pacer.enqueue(slice(0, 0x41, gate.generation())).unwrap();
        started(&pacer, 1).await;
        pacer.enqueue(slice(33, 0x41, gate.generation())).unwrap();
        assert_eq!(
            gate.flush(),
            Flushed {
                frames: 1,
                keyframe: false
            }
        );
        pacer.enqueue(slice(66, 0x41, gate.generation())).unwrap();
        pacer.enqueue(slice(99, 0x65, gate.generation())).unwrap();
        assert_eq!(
            gate.flush(),
            Flushed {
                frames: 2,
                keyframe: true
            }
        );
        assert!(pacer.shared.queue.lock().is_empty());
        assert_eq!(
            pacer.shared.queued_bytes.load(Ordering::Relaxed),
            3000,
            "only the frame in flight"
        );
        assert_eq!(rx.borrow().flushed_frames, 3);
    }

    #[tokio::test]
    async fn a_frame_stamped_before_a_flush_is_dropped_even_if_queued_after_it() {
        let open = Arc::new(watch::channel(false).0);
        let written = Arc::new(StdMutex::new(Vec::new()));
        let (pacer, mut rx, gate) =
            gated_pacer(TestWriter::Gate(open.clone(), written.clone())).await;
        pacer.enqueue(slice(0, 0x65, gate.generation())).unwrap();
        started(&pacer, 1).await;
        let stamped = gate.generation();
        gate.flush();
        pacer.enqueue(slice(33, 0x41, stamped)).unwrap();
        pacer.enqueue(slice(1000, 0x65, gate.generation())).unwrap();
        open.send_replace(true);
        let p = settled(&mut rx, 3).await;
        assert_eq!((p.sent_frames, p.flushed_frames), (2, 1));
        let written = written.lock().unwrap().clone();
        assert_eq!(
            written[1].wrapping_sub(written[0]),
            90_000,
            "the stale frame was skipped"
        );
    }

    #[tokio::test]
    async fn a_keyframe_dropped_as_stale_is_reported() {
        let open = Arc::new(watch::channel(false).0);
        let written = Arc::new(StdMutex::new(Vec::new()));
        let (pacer, mut rx, gate) =
            gated_pacer(TestWriter::Gate(open.clone(), written.clone())).await;
        pacer.enqueue(slice(0, 0x41, gate.generation())).unwrap();
        started(&pacer, 1).await;
        let stamped = gate.generation();
        assert_eq!(gate.flush(), Flushed::default());
        pacer.enqueue(slice(33, 0x65, stamped)).unwrap();
        open.send_replace(true);
        let p = settled(&mut rx, 2).await;
        assert_eq!((p.flushed_frames, p.stale_keyframes), (1, 1));
    }

    #[test]
    fn only_annex_b_irap_units_are_keyframes() {
        let frame = |codec, nal: &[u8]| PacedFrame {
            data: [&[0, 0, 0, 1][..], nal].concat(),
            codec,
            timing: Timing::Advance(33.0),
            generation: 0,
        };
        assert!(frame(VideoCodec::H264, &[0x65]).is_keyframe());
        assert!(!frame(VideoCodec::H264, &[0x41]).is_keyframe());
        for (nal_type, irap) in [
            (15, false),
            (16, true),
            (19, true),
            (21, true),
            (22, false),
            (1, false),
        ] {
            assert_eq!(
                frame(VideoCodec::H265, &[nal_type << 1, 1]).is_keyframe(),
                irap,
                "H.265 type {nal_type}"
            );
        }
        for codec in [VideoCodec::Vp8, VideoCodec::Vp9, VideoCodec::Av1] {
            assert!(!frame(codec, &[19 << 1, 1]).is_keyframe(), "{codec:?}");
        }
    }

    fn idle_shared() -> Arc<PacerShared> {
        let progress = Arc::new(watch::channel(VideoProgress::default()).0);
        Arc::new(PacerShared {
            track: Arc::new(DiscordTrack::new(RTPCodecType::Video, MIME_TYPE_H264, 2)),
            packetizer: Arc::default(),
            dave: Arc::new(StdMutex::new(DaveHandler::new(1, 2))),
            send_order: Arc::default(),
            queue: Arc::default(),
            queued_bytes: Arc::default(),
            outstanding: Arc::default(),
            gate: Arc::new(VideoGate::new(progress.clone())),
            progress,
            alive: AtomicBool::new(true),
        })
    }

    #[test]
    fn a_frame_enqueued_while_the_pacer_stops_is_settled_either_way() {
        for _ in 0..50_000 {
            let shared = idle_shared();
            let start = Arc::new(std::sync::Barrier::new(2));
            let enqueue = {
                let (shared, start) = (shared.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    shared.enqueue(frame(0)).is_ok()
                })
            };
            start.wait();
            shared.stop("stopped");
            let accepted = enqueue.join().unwrap();
            let p = shared.progress.borrow().clone();
            assert_eq!(p.accepted_frames, u64::from(accepted));
            assert!(p.settled(), "an accepted frame was never counted: {p:?}");
        }
    }

    #[tokio::test]
    async fn a_pacer_whose_task_ended_refuses_frames_and_counts_the_unsent() {
        let (pacer, rx) = pacer(TestWriter::Hangs).await;
        queue(&pacer, 3);
        started(&pacer, 3).await;
        pacer.task.abort();
        tokio::time::timeout(Duration::from_secs(5), async {
            while pacer.shared.alive.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("pacer marked dead");
        assert!(matches!(
            pacer.enqueue(frame(99)),
            Err(WebRtcError::PacerStopped)
        ));
        let p = rx.borrow().clone();
        assert_eq!((p.accepted_frames, p.dropped_frames), (3, 3));
        assert!(p.settled());
    }

    #[test]
    fn a_frame_above_the_cap_is_refused_before_anything_else() {
        let mut w = WebRtcWrapper::new(Arc::new(StdMutex::new(DaveHandler::new(1, 2))));
        let big = vec![0; MAX_VIDEO_FRAME_BYTES + 1];
        assert!(matches!(
            w.send_video(&big, Timing::Advance(33.0), None),
            Err(WebRtcError::FrameTooLarge(n)) if n == big.len()
        ));
        assert!(matches!(
            w.send_video(&big[1..], Timing::Advance(33.0), None),
            Err(WebRtcError::NotConnected)
        ));
    }

    #[test]
    fn packetizer_only_for_h264_and_h265() {
        let params = WebRtcParams {
            address: String::new(),
            port: 0,
            audio_ssrc: 1,
            video_ssrc: 2,
            rtx_ssrc: 3,
            supported_encryption_modes: vec![],
        };
        let mut w = WebRtcWrapper::new(Arc::new(StdMutex::new(DaveHandler::new(1, 2))));
        w.set_packetizer(&params, "hevc").unwrap();
        assert_eq!(w.video_codec(), Some(VideoCodec::H265));
        for codec in ["AV1", "VP8", "VP9"] {
            assert!(matches!(
                w.set_packetizer(&params, codec),
                Err(WebRtcError::UnsupportedCodec(_))
            ));
        }
    }
}
