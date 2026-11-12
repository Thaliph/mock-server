//! Fuzz target for the gRPC codec (spec GRP-10, §6.2).
//!
//! The input is read two ways. As a stream: its first four bytes pick where to cut the rest
//! (each as a fraction of its length), which is fed to the frame parser whole and in pieces:
//! the same frames and the same end whatever the cuts, and never more than a header and a
//! maximal payload held. Every frame is decoded under each encoding (the gzip inflation stays
//! within the cap). As a payload: the whole input and every decoded payload go to the wire
//! reader; what it reads writes JSON that reads back, and the encoding of that is a fixed
//! point (decode, encode and decode again give the same bytes).

#![no_main]

use libfuzzer_sys::fuzz_target;
use mimic_core::grpc::frame::{Encoding, Frame, FrameError, HEADER, MAX_MESSAGE, Parser, Payload};
use mimic_core::grpc::wire::{MAX_PAYLOAD, Message};

fn stream(parser: &mut Parser, pieces: &[&[u8]]) -> (Vec<Frame>, Option<FrameError>) {
    let mut frames = Vec::new();
    for piece in pieces {
        let pushed = parser.push(piece);
        frames.extend(pushed.frames);
        assert!(parser.buffered() <= HEADER + MAX_MESSAGE);
        if pushed.error.is_some() {
            return (frames, pushed.error);
        }
    }
    (frames, parser.finish().err())
}

fn payload(bytes: &[u8]) {
    assert!(bytes.len() <= MAX_PAYLOAD || Message::decode(bytes).is_err());
    let Ok(message) = Message::decode(bytes) else {
        return;
    };
    let json = message.to_json();
    let Ok(read) = Message::from_json(&json) else {
        panic!("the JSON of a decoded payload does not read back");
    };
    let encoded = read.encode().expect("a decoded payload encodes");
    assert!(encoded.len() <= bytes.len());
    let again = Message::decode(&encoded).expect("an encoding decodes");
    assert_eq!(again.encode().expect("encodes"), encoded);
}

fuzz_target!(|data: &[u8]| {
    payload(data);

    let (cuts, bytes) = data.split_at(data.len().min(4));
    let (whole, whole_end) = stream(&mut Parser::new(), &[bytes]);
    let mut points: Vec<usize> = cuts
        .iter()
        .map(|c| usize::from(*c) * bytes.len() / 256)
        .collect();
    points.sort_unstable();
    let mut pieces = Vec::new();
    let mut last = 0;
    for p in points {
        pieces.push(&bytes[last..p]);
        last = p;
    }
    pieces.push(&bytes[last..]);
    let (split, split_end) = stream(&mut Parser::new(), &pieces);
    assert_eq!((&whole, whole_end), (&split, split_end));

    for frame in &whole {
        assert!(frame.payload.len() <= MAX_MESSAGE);
        for encoding in [Encoding::Identity, Encoding::Gzip, Encoding::Other] {
            if let Ok(Payload::Bytes(inner)) = frame.decode(encoding) {
                assert!(inner.len() <= MAX_MESSAGE);
                payload(&inner);
            }
        }
    }
});
