use dstreamy_media::{Ingest, IngestError, OPUS_SILENCE_FRAME};

const KEYFRAME: &[u8] = &[
    0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88,
];
const DELTA: &[u8] = &[0, 0, 0, 1, 0x41, 0x9a];

#[test]
fn a_rejected_audio_packet_cannot_poison_either_tracks_next_timestamp() {
    for bad in [&[][..], &[0xf9, 0xff][..], &[0x10][..]] {
        let mut ingest = Ingest::default();
        ingest.video(KEYFRAME, 0, true).unwrap();
        ingest.audio(&OPUS_SILENCE_FRAME, 0).unwrap();
        assert!(ingest.audio(bad, 100_000).is_err(), "{bad:?}");
        ingest.audio(&OPUS_SILENCE_FRAME, 20_000).unwrap();
        ingest.video(DELTA, 33_333, false).unwrap();
    }
}

#[test]
fn the_first_packet_on_the_other_track_checks_the_exact_jump_boundary() {
    for origin in [i64::MIN, -20_000_000, 0, i64::MAX - 20_000_000] {
        let mut ingest = Ingest::default();
        ingest.video(KEYFRAME, origin, true).unwrap();
        assert!(matches!(
            ingest.audio(&OPUS_SILENCE_FRAME, origin + 10_000_001),
            Err(IngestError::PtsJump { track: "audio", .. })
        ));
        ingest
            .audio(&OPUS_SILENCE_FRAME, origin + 10_000_000)
            .unwrap();

        let mut ingest = Ingest::default();
        ingest.audio(&OPUS_SILENCE_FRAME, origin).unwrap();
        assert!(matches!(
            ingest.video(KEYFRAME, origin + 10_000_001, true),
            Err(IngestError::PtsJump { track: "video", .. })
        ));
        ingest.video(KEYFRAME, origin + 10_000_000, true).unwrap();
    }
}
