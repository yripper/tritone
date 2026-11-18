use flate2::{Compress, Compression, FlushCompress};

use super::*;

/// Compresses one payload the way the gateway does: one shared stream, each
/// message ending in a sync flush (`00 00 ff ff`).
fn deflate(c: &mut Compress, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 64);
    c.compress_vec(payload, &mut out, FlushCompress::Sync).unwrap();
    while out.len() == out.capacity() {
        out.reserve(64);
        c.compress_vec(&[], &mut out, FlushCompress::Sync).unwrap();
    }
    out
}

#[test]
fn zlib_stream_multi_frame() {
    let mut c = Compress::new(Compression::default(), true);
    let first = deflate(&mut c, br#"{"op":10,"d":{"heartbeat_interval":41250}}"#);
    let second = deflate(&mut c, br#"{"op":11,"d":null}"#);
    assert!(first.ends_with(&SUFFIX) && second.ends_with(&SUFFIX));

    let mut z = Inflater::new();
    assert_eq!(z.push(&first).unwrap().unwrap(), br#"{"op":10,"d":{"heartbeat_interval":41250}}"#);
    // The second message arrives in three pieces: only the last completes it.
    let (a, rest) = second.split_at(3);
    let (b, c2) = rest.split_at(rest.len() - 2);
    assert!(z.push(a).unwrap().is_none());
    assert!(z.push(b).unwrap().is_none());
    assert_eq!(z.push(c2).unwrap().unwrap(), br#"{"op":11,"d":null}"#);
}

#[test]
fn zlib_large_message_inflates_fully() {
    let mut c = Compress::new(Compression::default(), true);
    let big = format!(r#"{{"op":0,"d":"{}"}}"#, "x".repeat(3 << 20));
    let mut z = Inflater::new();
    assert_eq!(z.push(&deflate(&mut c, big.as_bytes())).unwrap().unwrap(), big.as_bytes());
}

#[test]
fn zlib_garbage_is_an_error() {
    let mut z = Inflater::new();
    assert!(z.push(&[1, 2, 3, 0, 0, 0xff, 0xff]).is_err());
}

#[test]
fn zlib_caps_the_buffered_frame() {
    let mut z = Inflater::with_limit(16);
    assert!(z.push(&[7u8; 20]).is_err());
}
