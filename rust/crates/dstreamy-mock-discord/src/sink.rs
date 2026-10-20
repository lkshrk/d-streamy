//! Reassembles received RTP into frames, removes DAVE and records them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use davey::MediaType;
use tokio::sync::mpsc;
use webrtc::rtp::packet::Packet;

use crate::dave::{DaveServer, is_dave_frame};
use crate::depacketize::{Depacketizer, VideoCodec};
use crate::rtc::{H264_PAYLOAD_TYPE, H265_PAYLOAD_TYPE, OPUS_PAYLOAD_TYPE};
use crate::state::{RecordedAudioFrame, RecordedVideoFrame, Shared, VoiceKind};

const PLAYOUT_DELAY_EXT_ID: u8 = 5;

pub(crate) type SharedDave = Arc<Mutex<Option<DaveServer>>>;

// A panic while decrypting leaves the MLS state usable for later frames.
pub(crate) fn lock_dave(dave: &SharedDave) -> MutexGuard<'_, Option<DaveServer>> {
    dave.lock().unwrap_or_else(|e| e.into_inner())
}

struct Assembly {
    depacketizer: Depacketizer,
    codec: VideoCodec,
    timestamp: Option<u32>,
    next_seq: Option<u16>,
    broken: bool,
}

pub(crate) struct Sink {
    shared: Arc<Shared>,
    kind: VoiceKind,
    server_id: String,
    dave: SharedDave,
    video: HashMap<u32, Assembly>,
}

impl Sink {
    pub fn spawn(
        shared: Arc<Shared>,
        kind: VoiceKind,
        server_id: String,
        dave: SharedDave,
        mut rx: mpsc::UnboundedReceiver<Packet>,
    ) -> tokio::task::JoinHandle<()> {
        let mut sink = Sink {
            shared,
            kind,
            server_id,
            dave,
            video: HashMap::new(),
        };
        tokio::spawn(async move {
            while let Some(packet) = rx.recv().await {
                sink.push(&packet);
            }
        })
    }

    fn push(&mut self, packet: &Packet) {
        let codec = match packet.header.payload_type {
            OPUS_PAYLOAD_TYPE => return self.audio(packet),
            H264_PAYLOAD_TYPE => VideoCodec::H264,
            H265_PAYLOAD_TYPE => VideoCodec::H265,
            other => return self.drop_frame(format!("unknown payload type {other}")),
        };
        let header = &packet.header;
        let asm = self.video.entry(header.ssrc).or_insert_with(|| Assembly {
            depacketizer: Depacketizer::new(codec),
            codec,
            timestamp: None,
            next_seq: None,
            broken: false,
        });
        if asm.codec != codec {
            *asm = Assembly {
                depacketizer: Depacketizer::new(codec),
                codec,
                timestamp: None,
                next_seq: None,
                broken: false,
            };
        }
        let mut lost = Vec::new();
        if asm.timestamp.is_some_and(|ts| ts != header.timestamp) {
            lost.push(format!(
                "frame at {} ended without marker",
                asm.timestamp.unwrap_or(0)
            ));
            asm.depacketizer.reset();
            asm.broken = false;
        }
        if asm.next_seq.is_some_and(|s| s != header.sequence_number) {
            asm.broken = true;
        }
        asm.next_seq = Some(header.sequence_number.wrapping_add(1));
        asm.timestamp = Some(header.timestamp);
        if let Err(e) = asm.depacketizer.push(&packet.payload) {
            asm.broken = true;
            lost.push(format!("depacketize: {e}"));
        }
        let finished = header.marker.then(|| {
            asm.timestamp = None;
            let broken = std::mem::take(&mut asm.broken);
            (asm.depacketizer.take_frame(), broken)
        });
        for reason in lost {
            self.drop_frame(reason);
        }
        let Some((frame, broken)) = finished else {
            return;
        };
        if broken {
            return self.drop_frame(format!("sequence gap before {}", header.sequence_number));
        }
        let (data, e2ee) = match self.decrypt(MediaType::VIDEO, frame) {
            Ok(v) => v,
            Err(reason) => return self.drop_frame(reason),
        };
        let playout_delay = packet
            .header
            .get_extension(PLAYOUT_DELAY_EXT_ID)
            .and_then(|b| <[u8; 3]>::try_from(b.as_ref()).ok());
        let frame = RecordedVideoFrame {
            kind: self.kind,
            server_id: self.server_id.clone(),
            ssrc: header.ssrc,
            payload_type: header.payload_type,
            codec,
            rtp_timestamp: header.timestamp,
            data,
            e2ee,
            playout_delay,
        };
        self.shared.record(|r| r.video.push(frame));
    }

    fn audio(&mut self, packet: &Packet) {
        let (data, e2ee) = match self.decrypt(MediaType::AUDIO, packet.payload.to_vec()) {
            Ok(v) => v,
            Err(reason) => return self.drop_frame(reason),
        };
        let frame = RecordedAudioFrame {
            kind: self.kind,
            server_id: self.server_id.clone(),
            ssrc: packet.header.ssrc,
            payload_type: packet.header.payload_type,
            rtp_timestamp: packet.header.timestamp,
            data,
            e2ee,
        };
        self.shared.record(|r| r.audio.push(frame));
    }

    fn decrypt(&self, media_type: MediaType, frame: Vec<u8>) -> Result<(Vec<u8>, bool), String> {
        if !is_dave_frame(&frame) {
            return Ok((frame, false));
        }
        let mut dave = lock_dave(&self.dave);
        let server = dave
            .as_mut()
            .ok_or_else(|| "DAVE frame without a DAVE session".to_owned())?;
        server
            .decrypt(media_type, &frame)
            .map(|plain| (plain, true))
            .map_err(|e| format!("DAVE decrypt: {e}"))
    }

    fn drop_frame(&self, reason: String) {
        self.shared.record(|r| r.dropped_frames.push(reason));
    }
}
