use dstreamy_ipc::control::MAX_LINE;
use dstreamy_ipc::producer::{HEADER_LEN, MAX_FRAME_LEN};
use dstreamy_ipc::{ControlCodec, Frame, Kind, Malformed, ProducerCodec, ProducerError};
use proptest::prelude::*;
use serde_json::Value;
use tokio_util::bytes::{BufMut, Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

fn frame() -> impl Strategy<Value = Frame> {
    let media = (
        prop_oneof![Just(Kind::Video), Just(Kind::Opus)],
        any::<i64>(),
        any::<bool>(),
        proptest::collection::vec(any::<u8>(), 0..2048),
    )
        .prop_map(|(kind, pts_us, keyframe, payload)| Frame {
            kind,
            pts_us,
            keyframe: keyframe && kind == Kind::Video,
            payload: Bytes::from(payload),
        });
    prop_oneof![9 => media, 1 => any::<i64>().prop_map(Frame::end_of_stream)]
}

fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<u64>().prop_map(Value::from),
        ".*".prop_map(Value::from),
    ];
    leaf.prop_recursive(4, 64, 8, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..8).prop_map(Value::from),
            proptest::collection::btree_map(".*", inner, 0..8)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

fn decode_all_frames(buf: &mut BytesMut) -> Result<Vec<Frame>, ProducerError> {
    let mut out = Vec::new();
    while let Some(frame) = ProducerCodec.decode_eof(buf)? {
        out.push(frame);
    }
    Ok(out)
}

fn feed_in_chunks<D: Decoder>(
    codec: &mut D,
    bytes: &[u8],
    chunk: usize,
) -> Result<Vec<D::Item>, D::Error> {
    let mut buf = BytesMut::new();
    let mut out = Vec::new();
    for piece in bytes.chunks(chunk.max(1)) {
        buf.extend_from_slice(piece);
        while let Some(item) = codec.decode(&mut buf)? {
            out.push(item);
        }
    }
    while let Some(item) = codec.decode_eof(&mut buf)? {
        out.push(item);
    }
    Ok(out)
}

proptest! {
    #[test]
    fn producer_frames_round_trip(frames in proptest::collection::vec(frame(), 0..16), chunk in 1usize..64) {
        let mut buf = BytesMut::new();
        for frame in &frames {
            ProducerCodec.encode(frame, &mut buf).unwrap();
        }
        let decoded = feed_in_chunks(&mut ProducerCodec, &buf, chunk).unwrap();
        prop_assert_eq!(decoded, frames);
    }

    #[test]
    fn truncated_producer_stream_is_an_error(frame in frame(), cut in any::<prop::sample::Index>()) {
        let mut buf = BytesMut::new();
        ProducerCodec.encode(&frame, &mut buf).unwrap();
        let cut = 1 + cut.index(buf.len() - 1);
        let mut short = BytesMut::from(&buf[..cut]);
        prop_assert!(matches!(decode_all_frames(&mut short), Err(ProducerError::Truncated)));
    }

    #[test]
    fn oversized_length_prefix_is_refused(len in (MAX_FRAME_LEN + 1)..=u32::MAX) {
        let mut buf = BytesMut::new();
        buf.put_u32(len);
        prop_assert!(matches!(ProducerCodec.decode(&mut buf), Err(ProducerError::Oversized(_))));
    }

    #[test]
    fn undersized_length_prefix_is_refused(len in 0..HEADER_LEN) {
        let mut buf = BytesMut::new();
        buf.put_u32(len);
        prop_assert!(matches!(ProducerCodec.decode(&mut buf), Err(ProducerError::TooShort(_))));
    }

    #[test]
    fn arbitrary_bytes_never_panic_the_producer_decoder(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let mut buf = BytesMut::from(&bytes[..]);
        let _ = decode_all_frames(&mut buf);
    }

    #[test]
    fn control_values_round_trip(values in proptest::collection::vec(json_value(), 0..8), chunk in 1usize..64) {
        let mut buf = BytesMut::new();
        for value in &values {
            ControlCodec::new().encode(value, &mut buf).unwrap();
        }
        let decoded = feed_in_chunks(&mut ControlCodec::new(), &buf, chunk).unwrap();
        let expected: Vec<Result<Value, Malformed>> = values.into_iter().map(Ok).collect();
        prop_assert_eq!(decoded, expected);
    }

    #[test]
    fn arbitrary_bytes_never_panic_the_control_decoder(bytes in proptest::collection::vec(any::<u8>(), 0..4096), chunk in 1usize..512) {
        let items = feed_in_chunks(&mut ControlCodec::new(), &bytes, chunk).unwrap();
        let lines = bytes.split(|&b| b == b'\n').count();
        prop_assert!(items.len() <= lines);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(8))]

    #[test]
    fn oversized_control_line_resyncs(extra in 1usize..(3 * MAX_LINE), chunk in 4096usize..(1 << 18)) {
        let mut bytes = vec![b'x'; MAX_LINE + extra];
        bytes.extend_from_slice(b"\n{\"id\":\"next\"}\n");
        let items = feed_in_chunks(&mut ControlCodec::new(), &bytes, chunk).unwrap();
        prop_assert_eq!(items, vec![Err(Malformed::Oversized), Ok(serde_json::json!({"id": "next"}))]);
    }
}
