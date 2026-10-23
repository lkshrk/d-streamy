#![no_main]

use dstreamy_ipc::{Kind, ProducerCodec};
use libfuzzer_sys::fuzz_target;
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder};

fuzz_target!(|data: &[u8]| {
    let mut buf = BytesMut::from(data);
    let mut consumed = 0;
    while let Ok(Some(frame)) = ProducerCodec.decode_eof(&mut buf) {
        consumed += frame.encoded_len();
        let mut out = BytesMut::new();
        ProducerCodec.encode(&frame, &mut out).unwrap();
        let mut original = data[consumed - frame.encoded_len()..consumed].to_vec();
        if frame.kind != Kind::Video {
            original[13] &= !1;
        }
        assert_eq!(&out[..], &original[..]);
        assert_eq!(ProducerCodec.decode(&mut out).unwrap(), Some(frame));
    }
});
