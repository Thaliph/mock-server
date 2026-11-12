//! Length-prefixed gRPC messages (spec §6.2, GRP-01 to GRP-03).

use std::borrow::Cow;

/// Largest payload, announced or inflated: 256 KiB.
pub const MAX_MESSAGE: usize = 256 * 1024;
/// Flag byte plus 4-byte length.
pub const HEADER: usize = 5;

/// Why a stream or a payload is no longer readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// A flag byte other than 0 or 1.
    BadFlag(u8),
    /// A frame announcing more than `MAX_MESSAGE` bytes.
    Oversize(usize),
    /// The stream ended inside a frame.
    Truncated,
    /// A compressed frame whose encoding is `identity`.
    CompressedIdentity,
    /// A gzip payload that does not inflate.
    Corrupt,
    /// A gzip payload that inflates to more than `MAX_MESSAGE` bytes.
    Inflated,
}

/// One message of the stream, as sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub compressed: bool,
    pub payload: Vec<u8>,
}

/// The `grpc-encoding` of a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Identity,
    Gzip,
    /// Any other name: compressed payloads stay opaque.
    Other,
}

impl Encoding {
    /// An absent header is `identity`; names compare case-insensitively.
    pub fn from_header(_value: Option<&str>) -> Encoding {
        todo!()
    }
}

/// A payload after the encoding is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload<'a> {
    Bytes(Cow<'a, [u8]>),
    /// Compressed with an encoding mimic does not decode.
    Opaque,
}

impl Frame {
    pub fn decode(&self, _encoding: Encoding) -> Result<Payload<'_>, FrameError> {
        todo!()
    }
}

/// What a `push` produced: the frames it completed, and the error that stopped the stream.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pushed {
    pub frames: Vec<Frame>,
    pub error: Option<FrameError>,
}

/// Incremental parser: bytes in as they arrive, frames out.
#[derive(Debug, Default)]
pub struct Parser {
    buf: Vec<u8>,
    error: Option<FrameError>,
}

impl Parser {
    pub fn new() -> Parser {
        Parser::default()
    }

    pub fn push(&mut self, _bytes: &[u8]) -> Pushed {
        todo!()
    }

    /// The stream ended: an error when a frame is partial, or when one failed before.
    pub fn finish(&self) -> Result<(), FrameError> {
        todo!()
    }

    /// Bytes of a partial frame held.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
}

/// The wire form of a message.
pub fn encode_frame(_compressed: bool, _payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    todo!()
}
