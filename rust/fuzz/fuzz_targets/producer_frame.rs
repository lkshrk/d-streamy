#![no_main]

use dstreamy_ipc::ProducerCodec;
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
        assert_eq!(&out[..], &data[consumed - frame.encoded_len()..consumed]);
        assert_eq!(ProducerCodec.decode(&mut out).unwrap(), Some(frame));
    }
});
