#![no_main]

use dstreamy_ipc::{Kind, ProducerCodec};
use dstreamy_media::{Ingest, OPUS_SILENCE_FRAME};
use libfuzzer_sys::fuzz_target;
use tokio_util::bytes::BytesMut;
use tokio_util::codec::Decoder;

const MAX_INPUT: usize = 64 * 1024;
const MAX_FRAMES: usize = 64;
const KEYFRAME: &[u8] = &[
    0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88,
];

fn recover(ingest: &mut Ingest, last: &mut [Option<i64>; 2]) {
    for track in 0..2 {
        let next = match last[track] {
            Some(pts) => pts.checked_add(1),
            None => Some(last[1 - track].unwrap_or(0)),
        };
        let Some(pts) = next else { continue };
        let result = if track == 0 {
            ingest.video(KEYFRAME, pts, true)
        } else {
            ingest.audio(&OPUS_SILENCE_FRAME, pts)
        };
        assert!(
            result.is_ok(),
            "rejected media poisoned track {track} at {pts}: {result:?}"
        );
        last[track] = Some(pts);
    }
}

fuzz_target!(|data: &[u8]| {
    let mut input = BytesMut::from(&data[..data.len().min(MAX_INPUT)]);
    let mut ingest = Ingest::default();
    let mut last = [None; 2];
    for _ in 0..MAX_FRAMES {
        let frame = match ProducerCodec.decode_eof(&mut input) {
            Ok(Some(frame)) => frame,
            _ => break,
        };
        let (track, result) = match frame.kind {
            Kind::Video => (
                0,
                ingest.video(&frame.payload, frame.pts_us, frame.keyframe),
            ),
            Kind::Opus => (1, ingest.audio(&frame.payload, frame.pts_us)),
            Kind::EndOfStream => {
                ingest = Ingest::default();
                last = [None; 2];
                continue;
            }
        };
        if result.is_ok() {
            last[track] = Some(frame.pts_us);
        } else {
            recover(&mut ingest, &mut last);
        }
    }
});
