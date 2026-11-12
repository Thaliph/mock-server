//! Fuzz target for the gRPC codec (spec GRP-10, §6.2).
//!
//! The input is read two ways. As a stream: its first four bytes pick where to cut the rest
//! (each as a fraction of its length), which is fed to the frame parser whole and in pieces:
//! the same frames and the same end whatever the cuts, and never more than a header and a
//! maximal payload held. Every frame is decoded under each encoding (the gzip inflation stays
//! within the cap). As a payload: the whole input and every decoded payload go to the wire
//! reader; what it reads writes JSON that reads back, and the encoding of that is a fixed
//! point (decode, encode and decode again give the same bytes). What it reads is also redacted
//! (GRP-18): a fixed point whose encoding decodes.

#![no_main]

use libfuzzer_sys::fuzz_target;
use mimic_core::grpc::record::{Mark, bucketed, first_message, single_message, steps};
use mimic_core::grpc::frame::{Encoding, Frame, FrameError, HEADER, MAX_MESSAGE, Parser, Payload};
use mimic_core::discovery::redact::Redactor;
use mimic_core::grpc::redact::{Direction, FieldNames, FieldRule, GrpcRedactor};
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

/// A decoded payload redacted (GRP-18): redaction is a fixed point and its output encodes.
fn redaction(message: &Message) {
    let rule = FieldRule::new("p.S/M", Direction::Request, "1.2").expect("a rule");
    let redactor = GrpcRedactor::new(Redactor::default(), vec![rule]);
    let mut names = FieldNames::new();
    names.insert(vec![2], "password".to_string());
    names.insert(vec![1, 1], "refreshToken".to_string());
    let once = redactor
        .message("p.S/M", Direction::Request, message, &names)
        .expect("a decoded payload is within the bounds");
    let twice = redactor
        .message("p.S/M", Direction::Request, &once, &names)
        .expect("a redacted payload is within the bounds");
    assert_eq!(once, twice);
    let encoded = once.encode().expect("a redacted payload encodes");
    assert!(encoded.len() <= MAX_PAYLOAD);
    Message::decode(&encoded).expect("an encoding decodes");
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
    redaction(&message);
    let encoded = read.encode().expect("a decoded payload encodes");
    assert!(encoded.len() <= bytes.len());
    let again = Message::decode(&encoded).expect("an encoding decodes");
    assert_eq!(again.encode().expect("encodes"), encoded);
}

/// The first message of a captured body (GRP-19): whatever the bytes and the encoding, no panic.
fn first(data: &[u8]) {
    for encoding in [None, Some("gzip"), Some("br")] {
        let _ = first_message(encoding, data);
    }
}

/// The steps of a streamed body (GRP-47): at most 63, every delay a bucket bound, whatever the
/// bytes, the encoding and the order of the marks.
fn steps_of(data: &[u8]) {
    let (head, body) = data.split_at(data.len().min(16));
    let marks: Vec<Mark> = head
        .chunks(2)
        .map(|c| Mark {
            end: usize::from(c[0]) * body.len() / 128,
            ms: u64::from(*c.get(1).unwrap_or(&0)) * 997,
        })
        .collect();
    let _ = single_message(body);
    for encoding in [None, Some("gzip"), Some("br")] {
        let found = steps(encoding, body, &marks);
        assert!(found.len() <= 63);
        for step in &found {
            assert_eq!(bucketed(step.delay_ms), step.delay_ms);
        }
        // Without marks there is no delay.
        assert!(steps(encoding, body, &[]).iter().all(|s| s.delay_ms == 0));
    }
}

fuzz_target!(|data: &[u8]| {
    payload(data);
    first(data);
    steps_of(data);

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
