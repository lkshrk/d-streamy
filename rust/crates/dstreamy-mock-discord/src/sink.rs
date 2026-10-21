//! Reassembles received RTP into frames, removes DAVE and records them.

use std::sync::{Arc, Mutex, MutexGuard};

use davey::{MediaType, OPUS_SILENCE_PACKET};
use tokio::sync::mpsc;
use webrtc::rtp::packet::Packet;

use crate::dave::{DaveServer, is_dave_frame};
use crate::depacketize::{Depacketizer, VideoCodec};
use crate::state::{RecordedAudioFrame, RecordedVideoFrame, Shared, VoiceKind};

const PLAYOUT_DELAY_EXT_ID: u8 = 5;

pub(crate) type SharedDave = Arc<Mutex<Option<DaveServer>>>;

// A panic while decrypting leaves the MLS state usable for later frames.
pub(crate) fn lock_dave(dave: &SharedDave) -> MutexGuard<'_, Option<DaveServer>> {
    dave.lock().unwrap_or_else(|e| e.into_inner())
}

/// The SSRCs from READY and payload types from SELECT_PROTOCOL; other RTP is dropped.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Negotiated {
    pub audio_ssrc: u32,
    pub video_ssrc: u32,
    pub audio_payload_type: u8,
    pub video_payload_type: u8,
    pub video_codec: VideoCodec,
}

struct Assembly {
    depacketizer: Depacketizer,
    timestamp: Option<u32>,
    next_seq: Option<u16>,
    broken: bool,
}

pub(crate) struct Sink {
    shared: Arc<Shared>,
    kind: VoiceKind,
    server_id: String,
    dave: SharedDave,
    negotiated: Negotiated,
    video: Assembly,
}

impl Sink {
    pub fn spawn(
        shared: Arc<Shared>,
        kind: VoiceKind,
        server_id: String,
        dave: SharedDave,
        negotiated: Negotiated,
        mut rx: mpsc::UnboundedReceiver<Packet>,
    ) -> tokio::task::JoinHandle<()> {
        let mut sink = Sink {
            shared,
            kind,
            server_id,
            dave,
            negotiated,
            video: Assembly {
                depacketizer: Depacketizer::new(negotiated.video_codec),
                timestamp: None,
                next_seq: None,
                broken: false,
            },
        };
        tokio::spawn(async move {
            while let Some(packet) = rx.recv().await {
                sink.push(&packet);
            }
        })
    }

    fn push(&mut self, packet: &Packet) {
        let header = &packet.header;
        let n = self.negotiated;
        match (header.ssrc, header.payload_type) {
            (ssrc, pt) if ssrc == n.audio_ssrc && pt == n.audio_payload_type => {
                return self.audio(packet);
            }
            (ssrc, pt) if ssrc == n.video_ssrc && pt == n.video_payload_type => {}
            (ssrc, pt) => {
                return self.drop_frame(format!("RTP with ssrc {ssrc} and payload type {pt}"));
            }
        }
        let asm = &mut self.video;
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
            codec: n.video_codec,
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
        let mut dave = lock_dave(&self.dave);
        if !is_dave_frame(&frame) {
            let rejected = dave
                .as_ref()
                .is_some_and(|s| s.is_active() && !s.accepts_plaintext());
            if rejected && frame != OPUS_SILENCE_PACKET {
                return Err("plaintext frame after the DAVE transition grace period".to_owned());
            }
            return Ok((frame, false));
        }
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

#[cfg(test)]
mod tests {
    use std::num::NonZeroU16;
    use std::time::Instant;

    use davey::{Codec, DaveSession, ProposalsOperationType};
    use webrtc::rtp::header::Header;

    use super::*;
    use crate::config::MockConfig;
    use crate::state::Recorded;

    const CLIENT: u64 = 42;
    const NEGOTIATED: Negotiated = Negotiated {
        audio_ssrc: 1000,
        video_ssrc: 1001,
        audio_payload_type: 120,
        video_payload_type: 101,
        video_codec: VideoCodec::H264,
    };

    fn sink(dave: Option<DaveServer>) -> Sink {
        Sink {
            shared: Arc::new(Shared::new(
                MockConfig::default(),
                String::new(),
                String::new(),
                0,
            )),
            kind: VoiceKind::Stream,
            server_id: "1".to_owned(),
            dave: Arc::new(Mutex::new(dave)),
            negotiated: NEGOTIATED,
            video: Assembly {
                depacketizer: Depacketizer::new(VideoCodec::H264),
                timestamp: None,
                next_seq: None,
                broken: false,
            },
        }
    }

    fn rtp(ssrc: u32, payload_type: u8, payload: &[u8]) -> Packet {
        Packet {
            header: Header {
                ssrc,
                payload_type,
                marker: true,
                ..Header::default()
            },
            payload: payload.to_vec().into(),
        }
    }

    fn dave_pair() -> (DaveServer, DaveSession) {
        let v = NonZeroU16::new(1).unwrap();
        let mut server = DaveServer::new(v, CLIENT, 7, 9999).unwrap();
        let mut client = DaveSession::new(v, CLIENT, 7, None).unwrap();
        let kp = client.create_key_package().unwrap();
        client
            .set_external_sender(server.external_sender_package())
            .unwrap();
        let proposals = server.on_key_package(&kp).unwrap();
        let cw = client
            .process_proposals(
                ProposalsOperationType::APPEND,
                &proposals[1..],
                Some(&[9999]),
            )
            .unwrap()
            .unwrap();
        let mut payload = cw.commit.clone();
        payload.extend(cw.welcome.unwrap());
        let (_, announce) = server.on_commit_welcome(&payload).unwrap();
        client.process_commit(&announce[2..]).unwrap();
        (server, client)
    }

    fn recorded(sink: &Sink) -> Recorded {
        sink.shared.state().recorded.clone()
    }

    #[test]
    fn rtp_outside_the_negotiated_ssrcs_and_payload_types_is_dropped() {
        let mut sink = sink(None);
        sink.push(&rtp(NEGOTIATED.audio_ssrc, 120, &[1, 2, 3]));
        sink.push(&rtp(5, 120, &[1, 2, 3]));
        sink.push(&rtp(NEGOTIATED.audio_ssrc, 101, &[1, 2, 3]));
        sink.push(&rtp(NEGOTIATED.video_ssrc, 103, &[0x65, 1]));
        let r = recorded(&sink);
        assert_eq!(r.audio.len(), 1);
        assert_eq!(r.dropped_frames.len(), 3, "{:?}", r.dropped_frames);
    }

    #[test]
    fn plaintext_is_dropped_only_after_the_transition_grace_period() {
        let (server, mut client) = dave_pair();
        let mut sink = sink(Some(server));
        let opus = [0xFC, 0xFF, 0xFE, 0x01];
        let audio = |sink: &mut Sink, payload: &[u8]| {
            sink.push(&rtp(NEGOTIATED.audio_ssrc, 120, payload));
        };
        audio(&mut sink, &opus);
        lock_dave(&sink.dave).as_mut().unwrap().execute_transition();
        audio(&mut sink, &opus);
        let encrypted = client
            .encrypt(MediaType::AUDIO, Codec::OPUS, &opus)
            .unwrap();
        audio(&mut sink, &encrypted);
        audio(&mut sink, &opus);
        lock_dave(&sink.dave).as_mut().unwrap().plaintext_until = Some(Instant::now());
        audio(&mut sink, &OPUS_SILENCE_PACKET);
        audio(&mut sink, &opus);
        let r = recorded(&sink);
        let e2ee: Vec<bool> = r.audio.iter().map(|a| a.e2ee).collect();
        assert_eq!(e2ee, [false, false, true, false, false]);
        assert_eq!(r.dropped_frames.len(), 1, "{:?}", r.dropped_frames);
    }
}
