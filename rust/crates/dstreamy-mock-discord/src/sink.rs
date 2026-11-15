//! Reassembles received RTP into frames, removes DAVE and records them.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use davey::errors::{DecryptError, DecryptorDecryptError};
use davey::{MediaType, OPUS_SILENCE_PACKET};
use tokio::sync::mpsc;
use webrtc::rtp::packet::Packet;

use crate::dave::{DaveServer, DaveServerError, is_dave_frame};
use crate::dave_frame;
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

/// Prefix of a drop explained by receive-side reordering: its nonce is below the other track's frame that set davey's baseline for the current key.
pub const REORDERED_DAVE_DROP: &str = "DAVE frame overtaken by the other track at a key's start: ";

/// What davey's decryptor has seen under the current client key, per track (audio 0, video 1).
#[derive(Default)]
struct KeyOrder {
    /// The track and nonce of the first frame davey decrypted under this key, which set its nonce baseline.
    baseline: Option<(usize, u32)>,
    newest: [Option<u32>; 2],
    decrypted: [Option<u32>; 2],
}

pub(crate) struct Sink {
    shared: Arc<Shared>,
    kind: VoiceKind,
    server_id: String,
    dave: SharedDave,
    negotiated: Negotiated,
    video: Assembly,
    key_order: KeyOrder,
}

/// davey reports a nonce below its baseline and a failed authentication alike (`cryptor/decryptor.rs:130,159`).
fn could_be_reordered(e: &DaveServerError) -> bool {
    matches!(
        e,
        DaveServerError::Decrypt(DecryptError::DecryptionFailed(
            DecryptorDecryptError::NoValidCryptorFound { .. }
        ))
    )
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
            key_order: KeyOrder::default(),
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
            received_at: Instant::now(),
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
            received_at: Instant::now(),
        };
        self.shared.record(|r| r.audio.push(frame));
    }

    fn decrypt(
        &mut self,
        media_type: MediaType,
        frame: Vec<u8>,
    ) -> Result<(Vec<u8>, bool), String> {
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
        let (name, track) = if media_type == MediaType::VIDEO {
            ("video", 1)
        } else {
            ("audio", 0)
        };
        let nonce = dave_frame::nonce(&frame);
        let result = server.decrypt(media_type, &frame);
        let Some(nonce) = nonce else {
            return result
                .map(|plain| (plain, true))
                .map_err(|e| format!("DAVE decrypt ({name}): {e}"));
        };
        let order = &mut self.key_order;
        // A track decrypts in rising nonce order under one key, so a decrypt that isn't rising means a new key started.
        if result.is_ok() && order.decrypted[track].is_some_and(|d| nonce <= d) {
            *order = KeyOrder::default();
        }
        let in_track_order = order.newest[track].is_none_or(|n| nonce > n);
        order.newest[track] = order.newest[track].max(Some(nonce));
        match result {
            Ok(plain) => {
                order.baseline.get_or_insert((track, nonce));
                order.decrypted[track] = order.decrypted[track].max(Some(nonce));
                Ok((plain, true))
            }
            Err(e)
                if could_be_reordered(&e)
                    && in_track_order
                    && order.baseline.is_some_and(|(t, b)| t != track && nonce < b) =>
            {
                Err(format!("{REORDERED_DAVE_DROP}{name}: {e}"))
            }
            Err(e) => Err(format!("DAVE decrypt ({name}): {e}")),
        }
    }

    fn drop_frame(&self, reason: String) {
        self.shared.record(|r| r.dropped_frames.push(reason));
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU16;

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
            key_order: KeyOrder::default(),
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
        let mut server = DaveServer::new(NonZeroU16::new(1).unwrap(), CLIENT, 7, 9999).unwrap();
        let client = join(&mut server);
        (server, client)
    }

    fn join(server: &mut DaveServer) -> DaveSession {
        let v = NonZeroU16::new(1).unwrap();
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
        client
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

    const OPUS: [u8; 4] = [0xFC, 0xFF, 0xFE, 0x01];
    const IDR: [u8; 10] = [0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x33, 0xFF];

    fn seal(client: &mut DaveSession, media: MediaType) -> Vec<u8> {
        let (codec, frame) = if media == MediaType::VIDEO {
            (Codec::H264, &IDR[..])
        } else {
            (Codec::OPUS, &OPUS[..])
        };
        client.encrypt(media, codec, frame).unwrap().to_vec()
    }

    fn excused(e: &str, track: &str) -> bool {
        e.starts_with(&format!("{REORDERED_DAVE_DROP}{track}"))
    }

    fn rekey(sink: &Sink, client: &mut DaveSession) -> Vec<u8> {
        let mut dave = lock_dave(&sink.dave);
        let server = dave.as_mut().unwrap();
        let proposals = server.add_guest_proposal(8888).unwrap();
        let cw = client
            .process_proposals(
                ProposalsOperationType::APPEND,
                &proposals[1..],
                Some(&[9999, 8888]),
            )
            .unwrap()
            .unwrap();
        let mut payload = cw.commit.clone();
        payload.extend(cw.welcome.unwrap());
        server.on_commit_welcome(&payload).unwrap().1
    }

    #[test]
    fn every_frame_a_new_key_frame_on_the_other_track_overtook_is_excused() {
        let (server, mut client) = dave_pair();
        let mut sink = sink(Some(server));
        let audio = [
            seal(&mut client, MediaType::AUDIO),
            seal(&mut client, MediaType::AUDIO),
        ];
        let video = seal(&mut client, MediaType::VIDEO);
        let late_video = seal(&mut client, MediaType::VIDEO);
        let later_audio = seal(&mut client, MediaType::AUDIO);
        assert!(sink.decrypt(MediaType::VIDEO, video).is_ok());
        for packet in audio.clone() {
            let e = sink.decrypt(MediaType::AUDIO, packet).unwrap_err();
            assert!(excused(&e, "audio"), "{e}");
        }
        assert!(sink.decrypt(MediaType::AUDIO, later_audio).is_ok());
        assert!(sink.decrypt(MediaType::VIDEO, late_video).is_ok());
        let e = sink
            .decrypt(MediaType::AUDIO, audio[0].clone())
            .unwrap_err();
        assert!(!excused(&e, ""), "a replay: {e}");
    }

    #[test]
    fn a_video_frame_behind_decrypted_audio_is_excused_too() {
        let (server, mut client) = dave_pair();
        let mut sink = sink(Some(server));
        let video = seal(&mut client, MediaType::VIDEO);
        assert!(
            sink.decrypt(MediaType::AUDIO, seal(&mut client, MediaType::AUDIO))
                .is_ok()
        );
        let e = sink.decrypt(MediaType::VIDEO, video).unwrap_err();
        assert!(excused(&e, "video"), "{e}");
    }

    #[test]
    fn a_frame_overtaken_on_its_own_track_is_a_real_loss() {
        let (server, mut client) = dave_pair();
        let mut sink = sink(Some(server));
        let first = seal(&mut client, MediaType::AUDIO);
        assert!(
            sink.decrypt(MediaType::AUDIO, seal(&mut client, MediaType::AUDIO))
                .is_ok()
        );
        assert!(
            sink.decrypt(MediaType::VIDEO, seal(&mut client, MediaType::VIDEO))
                .is_ok()
        );
        let e = sink.decrypt(MediaType::AUDIO, first).unwrap_err();
        assert!(!excused(&e, ""), "{e}");
    }

    #[test]
    fn a_new_participant_session_starts_its_own_baseline() {
        let (server, mut client) = dave_pair();
        let mut sink = sink(Some(server));
        for media in [MediaType::AUDIO, MediaType::VIDEO] {
            assert!(sink.decrypt(media, seal(&mut client, media)).is_ok());
        }
        let mut client = join(lock_dave(&sink.dave).as_mut().unwrap());
        let audio = seal(&mut client, MediaType::AUDIO);
        assert!(
            sink.decrypt(MediaType::VIDEO, seal(&mut client, MediaType::VIDEO))
                .is_ok()
        );
        let e = sink.decrypt(MediaType::AUDIO, audio).unwrap_err();
        assert!(excused(&e, "audio"), "{e}");
    }

    #[test]
    fn old_key_frames_after_a_rekey_count_toward_the_old_key_only() {
        let (server, mut client) = dave_pair();
        let mut sink = sink(Some(server));
        let replayed = seal(&mut client, MediaType::AUDIO);
        assert!(sink.decrypt(MediaType::AUDIO, replayed.clone()).is_ok());
        assert!(
            sink.decrypt(MediaType::VIDEO, seal(&mut client, MediaType::VIDEO))
                .is_ok()
        );
        let announce = rekey(&sink, &mut client);
        let old_key = seal(&mut client, MediaType::AUDIO);
        assert!(sink.decrypt(MediaType::AUDIO, old_key).is_ok());
        client.process_commit(&announce[2..]).unwrap();
        let audio = seal(&mut client, MediaType::AUDIO);
        assert!(
            sink.decrypt(MediaType::VIDEO, seal(&mut client, MediaType::VIDEO))
                .is_ok()
        );
        let e = sink.decrypt(MediaType::AUDIO, audio).unwrap_err();
        assert!(excused(&e, "audio"), "{e}");
        let e = sink.decrypt(MediaType::AUDIO, replayed).unwrap_err();
        assert!(!excused(&e, ""), "an old-key replay: {e}");
    }

    #[test]
    fn a_drop_while_neither_track_decrypted_is_a_real_loss() {
        let (server, mut client) = dave_pair();
        let mut sink = sink(Some(server));
        let mut forged = seal(&mut client, MediaType::VIDEO);
        forged[5] ^= 0xff;
        for _ in 0..2 {
            let e = sink.decrypt(MediaType::VIDEO, forged.clone()).unwrap_err();
            assert!(!excused(&e, ""), "{e}");
        }
    }

    #[test]
    fn only_a_rejected_nonce_or_tag_can_be_receive_reordering() {
        let no_cryptor = DecryptorDecryptError::NoValidCryptorFound {
            media_type: MediaType::AUDIO,
            encrypted_size: 0,
            plaintext_size: 0,
            manager_count: 1,
        };
        assert!(could_be_reordered(
            &DecryptError::DecryptionFailed(no_cryptor).into()
        ));
        for e in [
            DecryptError::NoDecryptorForUser,
            DecryptError::DecryptionFailed(
                DecryptorDecryptError::UnencryptedWhenPassthroughDisabled,
            ),
        ] {
            assert!(!could_be_reordered(&e.into()));
        }
        assert!(!could_be_reordered(&DaveServerError::NoParticipant));
    }

    #[test]
    fn a_dave_frame_without_a_participant_is_not_tagged() {
        let (_, mut client) = dave_pair();
        let server = DaveServer::new(NonZeroU16::new(1).unwrap(), CLIENT, 7, 9999).unwrap();
        let mut sink = sink(Some(server));
        let video = client
            .encrypt(
                MediaType::VIDEO,
                Codec::H264,
                &[0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00],
            )
            .unwrap()
            .to_vec();
        let e = sink.decrypt(MediaType::VIDEO, video).unwrap_err();
        assert!(!excused(&e, ""), "{e}");
    }
}
