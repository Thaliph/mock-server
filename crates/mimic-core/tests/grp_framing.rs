//! GRP-01 to GRP-03 (#459): gRPC message framing, size bounds and encodings.

use std::io::Write;

use flate2::Compression;
use flate2::write::GzEncoder;
use mimic_core::grpc::frame::{
    Encoding, Frame, FrameError, HEADER, MAX_MESSAGE, Parser, Payload, encode_frame,
};
use proptest::prelude::*;

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut e = GzEncoder::new(Vec::new(), Compression::default());
    e.write_all(data).expect("in memory");
    e.finish().expect("in memory")
}

fn frame(compressed: bool, payload: &[u8]) -> Frame {
    Frame {
        compressed,
        payload: payload.to_vec(),
    }
}

fn stream(frames: &[Frame]) -> Vec<u8> {
    frames
        .iter()
        .flat_map(|f| encode_frame(f.compressed, &f.payload).expect("encodes"))
        .collect()
}

fn read_split(bytes: &[u8], cuts: &[usize]) -> (Vec<Frame>, Option<FrameError>) {
    let mut parser = Parser::new();
    let mut frames = Vec::new();
    let mut last = 0;
    let mut points: Vec<usize> = cuts.iter().map(|c| c % (bytes.len() + 1)).collect();
    points.push(bytes.len());
    points.sort_unstable();
    for p in points {
        let pushed = parser.push(&bytes[last..p]);
        frames.extend(pushed.frames);
        if pushed.error.is_some() {
            return (frames, pushed.error);
        }
        last = p;
    }
    (frames, parser.finish().err())
}

#[test]
fn grp_01_frames_are_read_as_sent() {
    let sent = [frame(false, b"abc"), frame(true, b"zz"), frame(false, b"")];
    let bytes = stream(&sent);
    assert_eq!(&bytes[..HEADER], &[0, 0, 0, 0, 3]);
    let mut parser = Parser::new();
    let pushed = parser.push(&bytes);
    assert_eq!(pushed.error, None);
    assert_eq!(pushed.frames, sent);
    assert_eq!(parser.finish(), Ok(()));
    assert_eq!(parser.buffered(), 0);
    // One frame alone, and an empty stream.
    assert_eq!(
        Parser::new().push(&bytes[..8]).frames,
        [frame(false, b"abc")]
    );
    assert_eq!(Parser::new().finish(), Ok(()));
}

#[test]
fn grp_01_a_stream_cut_at_every_offset_gives_the_same_frames() {
    let sent = [frame(false, b"abc"), frame(true, b"zz"), frame(false, b"")];
    let bytes = stream(&sent);
    for cut in 0..=bytes.len() {
        let (frames, error) = read_split(&bytes, &[cut]);
        assert_eq!((frames, error), (sent.to_vec(), None), "cut at {cut}");
    }
    // Byte by byte.
    let mut parser = Parser::new();
    let mut frames = Vec::new();
    for b in &bytes {
        frames.extend(parser.push(std::slice::from_ref(b)).frames);
    }
    assert_eq!(frames, sent);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]
    #[test]
    fn grp_01_any_split_gives_the_same_frames(
        payloads in proptest::collection::vec(
            (any::<bool>(), proptest::collection::vec(any::<u8>(), 0..64)), 0..8),
        cuts in proptest::collection::vec(any::<usize>(), 0..12),
    ) {
        let sent: Vec<Frame> = payloads.iter().map(|(c, p)| frame(*c, p)).collect();
        let bytes = stream(&sent);
        prop_assert_eq!(read_split(&bytes, &cuts), (sent, None));
    }
}

#[test]
fn grp_01_a_flag_other_than_0_or_1_is_an_error_and_sticks() {
    let mut parser = Parser::new();
    let pushed = parser.push(&[0, 0, 0, 0, 1, b'x', 2, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    assert_eq!(pushed.frames, [frame(false, b"x")]);
    assert_eq!(pushed.error, Some(FrameError::BadFlag(2)));
    let again = parser.push(&[0, 0, 0, 0, 0]);
    assert_eq!(again.error, Some(FrameError::BadFlag(2)));
    assert!(again.frames.is_empty());
    assert_eq!(parser.finish(), Err(FrameError::BadFlag(2)));
}

#[test]
fn grp_01_finish_after_a_partial_frame_is_an_error() {
    for partial in [&[0u8][..], &[0, 0, 0, 0, 3, b'a', b'b'][..]] {
        let mut parser = Parser::new();
        let pushed = parser.push(partial);
        assert!(pushed.frames.is_empty() && pushed.error.is_none());
        assert_eq!(parser.finish(), Err(FrameError::Truncated));
    }
}

#[test]
fn grp_02_a_frame_of_256_kib_is_read() {
    let payload = vec![7u8; MAX_MESSAGE];
    let bytes = encode_frame(false, &payload).expect("encodes");
    let pushed = Parser::new().push(&bytes);
    assert_eq!(pushed.error, None);
    assert_eq!(pushed.frames, [frame(false, &payload)]);
}

#[test]
fn grp_02_an_oversize_frame_is_refused_from_its_header() {
    let announced = u32::try_from(MAX_MESSAGE + 1).expect("fits");
    let mut header = vec![0u8];
    header.extend_from_slice(&announced.to_be_bytes());
    let mut parser = Parser::new();
    let pushed = parser.push(&header);
    assert_eq!(pushed.error, Some(FrameError::Oversize(MAX_MESSAGE + 1)));
    assert!(parser.buffered() <= HEADER);
    // Fed a byte at a time, the same, and nothing beyond the header is kept.
    let mut parser = Parser::new();
    let mut error = None;
    for b in &header {
        error = parser.push(std::slice::from_ref(b)).error;
        assert!(parser.buffered() <= HEADER);
    }
    assert_eq!(error, Some(FrameError::Oversize(MAX_MESSAGE + 1)));
    // A header with the body behind it, in one push.
    let mut with_body = header.clone();
    with_body.extend(std::iter::repeat_n(1u8, MAX_MESSAGE + 1));
    let mut parser = Parser::new();
    assert_eq!(
        parser.push(&with_body).error,
        Some(FrameError::Oversize(MAX_MESSAGE + 1))
    );
    assert!(parser.buffered() <= HEADER);
    assert_eq!(
        encode_frame(false, &vec![0u8; MAX_MESSAGE + 1]),
        Err(FrameError::Oversize(MAX_MESSAGE + 1))
    );
}

#[test]
fn grp_03_identity_absent_and_gzip_are_decoded() {
    assert_eq!(Encoding::from_header(None), Encoding::Identity);
    assert_eq!(Encoding::from_header(Some("identity")), Encoding::Identity);
    assert_eq!(Encoding::from_header(Some("gzip")), Encoding::Gzip);
    assert_eq!(Encoding::from_header(Some("GZip")), Encoding::Gzip);
    for other in ["br", "deflate", "zstd", "snappy", "x-unknown", ""] {
        assert_eq!(
            Encoding::from_header(Some(other)),
            Encoding::Other,
            "encoding {other:?}"
        );
    }
    let plain = frame(false, b"hello");
    for encoding in [Encoding::Identity, Encoding::Gzip, Encoding::Other] {
        assert_eq!(
            plain.decode(encoding),
            Ok(Payload::Bytes(b"hello"[..].into()))
        );
    }
    let packed = frame(true, &gzip(b"hello world"));
    assert_eq!(
        packed.decode(Encoding::Gzip),
        Ok(Payload::Bytes(b"hello world"[..].into()))
    );
}

#[test]
fn grp_03_a_compressed_frame_under_identity_is_an_error() {
    assert_eq!(
        frame(true, b"x").decode(Encoding::Identity),
        Err(FrameError::CompressedIdentity)
    );
}

#[test]
fn grp_03_other_encodings_leave_a_compressed_frame_opaque() {
    assert_eq!(
        frame(true, b"\x28\xb5\x2f\xfd").decode(Encoding::Other),
        Ok(Payload::Opaque)
    );
}

#[test]
fn grp_03_a_corrupt_gzip_is_an_error() {
    assert_eq!(
        frame(true, b"not gzip at all").decode(Encoding::Gzip),
        Err(FrameError::Corrupt)
    );
    let mut cut = gzip(&vec![9u8; 1000]);
    cut.truncate(cut.len() - 12);
    assert_eq!(
        frame(true, &cut).decode(Encoding::Gzip),
        Err(FrameError::Corrupt)
    );
}

#[test]
fn grp_03_a_gzip_bomb_is_refused_and_the_limit_is_accepted() {
    let bomb = gzip(&vec![0u8; 64 * 1024 * 1024]);
    assert!(bomb.len() < 100 * 1024, "the bomb is small: {}", bomb.len());
    assert_eq!(
        frame(true, &bomb).decode(Encoding::Gzip),
        Err(FrameError::Inflated)
    );
    let exact = vec![5u8; MAX_MESSAGE];
    assert_eq!(
        frame(true, &gzip(&exact)).decode(Encoding::Gzip),
        Ok(Payload::Bytes(exact.into()))
    );
    assert_eq!(
        frame(true, &gzip(&vec![5u8; MAX_MESSAGE + 1])).decode(Encoding::Gzip),
        Err(FrameError::Inflated)
    );
}
