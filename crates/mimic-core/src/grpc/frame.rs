//! Length-prefixed gRPC messages (spec §6.2, GRP-01 to GRP-03).

use std::borrow::Cow;
use std::io::Read;

use flate2::read::GzDecoder;

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
    pub fn from_header(value: Option<&str>) -> Encoding {
        match value {
            None => Encoding::Identity,
            Some(v) if v.eq_ignore_ascii_case("identity") => Encoding::Identity,
            Some(v) if v.eq_ignore_ascii_case("gzip") => Encoding::Gzip,
            Some(_) => Encoding::Other,
        }
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
    /// The payload with the stream's encoding undone. An inflated payload is capped at
    /// `MAX_MESSAGE`: the decoder is read one byte past the cap and refused beyond it.
    pub fn decode(&self, encoding: Encoding) -> Result<Payload<'_>, FrameError> {
        if !self.compressed {
            return Ok(Payload::Bytes(Cow::Borrowed(&self.payload)));
        }
        match encoding {
            Encoding::Identity => Err(FrameError::CompressedIdentity),
            Encoding::Other => Ok(Payload::Opaque),
            Encoding::Gzip => {
                let mut out = Vec::new();
                let cap = MAX_MESSAGE as u64 + 1;
                GzDecoder::new(self.payload.as_slice())
                    .take(cap)
                    .read_to_end(&mut out)
                    .map_err(|_| FrameError::Corrupt)?;
                if out.len() > MAX_MESSAGE {
                    return Err(FrameError::Inflated);
                }
                Ok(Payload::Bytes(Cow::Owned(out)))
            }
        }
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

    /// Reads what arrived. Frames completed before an error are returned with it; after an
    /// error the parser reads nothing more and reports the same error. At most `HEADER`
    /// bytes of a refused frame are held: the length is checked before the body is.
    pub fn push(&mut self, mut bytes: &[u8]) -> Pushed {
        let mut pushed = Pushed {
            frames: Vec::new(),
            error: self.error,
        };
        while pushed.error.is_none() {
            // The header first, then the body it announces.
            let want = if self.buf.len() < HEADER {
                HEADER - self.buf.len()
            } else {
                HEADER + self.announced() - self.buf.len()
            };
            let take = want.min(bytes.len());
            self.buf.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buf.len() < HEADER {
                break;
            }
            if let Err(error) = self.check_header() {
                self.fail(error, &mut pushed);
                break;
            }
            if self.buf.len() < HEADER + self.announced() {
                // The body is not complete: read on while there is input.
                if bytes.is_empty() {
                    break;
                }
                continue;
            }
            let payload = self.buf.split_off(HEADER);
            let compressed = self.buf[0] == 1;
            self.buf.clear();
            pushed.frames.push(Frame {
                compressed,
                payload,
            });
        }
        pushed
    }

    fn announced(&self) -> usize {
        let b = &self.buf;
        u32::from_be_bytes([b[1], b[2], b[3], b[4]]) as usize
    }

    fn check_header(&self) -> Result<(), FrameError> {
        match self.buf[0] {
            0 | 1 => {}
            flag => return Err(FrameError::BadFlag(flag)),
        }
        match self.announced() {
            n if n > MAX_MESSAGE => Err(FrameError::Oversize(n)),
            _ => Ok(()),
        }
    }

    fn fail(&mut self, error: FrameError, pushed: &mut Pushed) {
        self.error = Some(error);
        self.buf.clear();
        pushed.error = Some(error);
    }

    /// The stream ended: an error when a frame is partial, or when one failed before.
    pub fn finish(&self) -> Result<(), FrameError> {
        match (self.error, self.buf.is_empty()) {
            (Some(error), _) => Err(error),
            (None, true) => Ok(()),
            (None, false) => Err(FrameError::Truncated),
        }
    }

    /// Bytes of a partial frame held.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
}

/// The wire form of a message.
pub fn encode_frame(compressed: bool, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.len() > MAX_MESSAGE {
        return Err(FrameError::Oversize(payload.len()));
    }
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.push(u8::from(compressed));
    // Cannot truncate: bounded by MAX_MESSAGE above.
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}
