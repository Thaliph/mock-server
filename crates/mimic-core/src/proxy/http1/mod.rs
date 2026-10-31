//! Byte-exact HTTP/1 framer (spec §4.1, "HTTP/1.x" and "Unframed HTTP/1").
//!
//! The relay forwards every byte unchanged; the [`Codec`] only watches the two directions to
//! find message boundaries and to report what the access log and the metrics need. It never
//! answers and never rejects: input it cannot frame unambiguously yields
//! [`Event::Unframed`] and the codec goes silent (the rest of the connection is a tunnel).
//! State is bounded: a head or trailer section is buffered up to 64 KiB, a chunk-size line is
//! never buffered.

#[cfg(test)]
mod tests;

/// Largest head or trailer section, in bytes (the blank line included).
pub const HEAD_LIMIT: usize = 64 * 1024;
/// Most lines in a head (start line included) or in a trailer section.
pub const LINE_LIMIT: usize = 256;
/// Longest chunk-size line, line ending excluded.
pub const CHUNK_LINE_LIMIT: usize = 4 * 1024;
/// Longest `path` reported, in bytes.
pub const PATH_LIMIT: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    Http10,
    Http11,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    pub method: String,
    /// Path without query, at most [`PATH_LIMIT`] bytes; empty for `CONNECT`.
    pub path: String,
    /// Authority of an absolute-form target, or the target of a `CONNECT` (no userinfo).
    pub uri_authority: Option<String>,
    /// First `Host` header value.
    pub host: Option<String>,
    pub version: Version,
    /// `Connection: upgrade` with an `Upgrade` header.
    pub upgrade: bool,
    pub connect: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    pub version: Version,
    pub status: u16,
    /// The request this response answers; `None` when no request was pending.
    pub request: Option<RequestHead>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Client,
    Server,
}

/// Why a message could not be framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    HeadTooLarge,
    TooManyHeaderLines,
    TrailersTooLarge,
    TooManyTrailerLines,
    BareCr,
    BadStartLine,
    BadHeaderLine,
    ObsFold,
    /// Both `content-length` and `transfer-encoding`.
    ConflictingLength,
    BadContentLength,
    BadTransferEncoding,
    BadChunkSize,
    ChunkLineTooLong,
    BadChunkTerminator,
    /// Client bytes held back behind an upgrade request outgrew the head limit.
    HeldOverflow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unframed {
    pub side: Side,
    pub reason: Reason,
    /// The request known so far (for a response: the pending one).
    pub request: Option<RequestHead>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Request(RequestHead),
    /// The request, body included, is complete. `body_bytes` counts payload bytes (chunk
    /// data for `chunked`).
    RequestEnd {
        body_bytes: u64,
    },
    /// A `1xx` other than `101`: not the final response, the request stays pending.
    Interim(ResponseHead),
    /// The final response head (also `101`).
    Response(ResponseHead),
    ResponseEnd {
        body_bytes: u64,
    },
    /// The HTTP part is over (`101`, or 2xx to `CONNECT`): the rest is opaque.
    Tunnel,
    /// Framing gave up: the rest is opaque, no further event.
    Unframed(Unframed),
}

#[derive(Debug, Default)]
pub struct Codec {}

impl Codec {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes read from the client.
    pub fn client_bytes(&mut self, _data: &[u8]) -> Vec<Event> {
        Vec::new()
    }

    /// Bytes read from the destination.
    pub fn server_bytes(&mut self, _data: &[u8]) -> Vec<Event> {
        Vec::new()
    }

    /// The destination closed (FIN): ends a close-delimited response.
    pub fn server_eof(&mut self) -> Vec<Event> {
        Vec::new()
    }

    /// Requests read and not yet answered by a final response.
    pub fn pending(&self) -> usize {
        0
    }
}
