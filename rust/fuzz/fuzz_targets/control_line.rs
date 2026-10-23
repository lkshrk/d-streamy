#![no_main]

use dstreamy_ipc::ControlCodec;
use dstreamy_ipc::control::MAX_LINE;
use libfuzzer_sys::fuzz_target;
use serde_json::Value;
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder};

// serde_json without `float_roundtrip` may parse a printed f64 one ulp off.
fn has_float(value: &Value) -> bool {
    match value {
        Value::Number(n) => n.is_f64(),
        Value::Array(items) => items.iter().any(has_float),
        Value::Object(fields) => fields.values().any(has_float),
        _ => false,
    }
}

fuzz_target!(|data: &[u8]| {
    let (split, data) = match data.split_first() {
        Some((&split, rest)) => (usize::from(split).max(1), rest),
        None => return,
    };
    let mut codec = ControlCodec::new();
    let mut buf = BytesMut::new();
    let mut items = Vec::new();
    for chunk in data.chunks(split) {
        buf.extend_from_slice(chunk);
        while let Some(item) = codec.decode(&mut buf).unwrap() {
            items.push(item);
        }
        assert!(buf.len() <= MAX_LINE + chunk.len());
    }
    while let Some(item) = codec.decode_eof(&mut buf).unwrap() {
        items.push(item);
    }
    assert!(items.len() <= data.split(|&b| b == b'\n').count());
    for value in items.into_iter().flatten().filter(|v| !has_float(v)) {
        let mut out = BytesMut::new();
        if ControlCodec::new().encode(&value, &mut out).is_ok() {
            let again = ControlCodec::new().decode(&mut out).unwrap();
            assert_eq!(again, Some(Ok(value)));
        }
    }
});
