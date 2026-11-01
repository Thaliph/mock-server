//! Message body framing (RFC 9112 §6, §7): `content-length`, `chunked`, close-delimited.

use super::scan::{HeadScanner, Kind, Scan};
use super::{CHUNK_LINE_LIMIT, Reason};

/// Payload bytes handed to the discovery tap: at most `room` of them, per message.
#[derive(Debug, Default)]
pub(super) struct Cap {
    pub(super) room: usize,
    pub(super) out: Vec<u8>,
}

impl Cap {
    fn put(&mut self, payload: &[u8]) {
        let n = payload.len().min(self.room);
        self.out.extend_from_slice(&payload[..n]);
        self.room -= n;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum BodyScan {
    More,
    /// The body ended; the payload byte count.
    Done(u64),
    Unframed(Reason),
}

#[derive(Debug)]
pub(super) enum Body {
    Length { left: u64, total: u64 },
    Chunked { state: Chunk, total: u64 },
    UntilClose { total: u64 },
}

#[derive(Debug)]
pub(super) enum Chunk {
    Size {
        size: u64,
        digits: u32,
        ext: bool,
        len: usize,
        cr: bool,
    },
    Data {
        left: u64,
    },
    DataEnd {
        cr: bool,
    },
    Trailers(HeadScanner),
}

impl Chunk {
    fn size_line() -> Self {
        Chunk::Size {
            size: 0,
            digits: 0,
            ext: false,
            len: 0,
            cr: false,
        }
    }
}

impl Body {
    pub(super) fn length(n: u64) -> Self {
        Body::Length { left: n, total: n }
    }

    pub(super) fn chunked() -> Self {
        Body::Chunked {
            state: Chunk::size_line(),
            total: 0,
        }
    }

    pub(super) fn until_close() -> Self {
        Body::UntilClose { total: 0 }
    }

    /// Bytes buffered for the framing (a trailer section being read).
    pub(super) fn retained(&self) -> usize {
        match self {
            Body::Chunked {
                state: Chunk::Trailers(scanner),
                ..
            } => scanner.retained(),
            _ => 0,
        }
    }

    /// Payload bytes read so far.
    pub(super) fn seen(&self) -> u64 {
        match self {
            Body::Length { left, total } => total - left,
            Body::Chunked { total, .. } | Body::UntilClose { total } => *total,
        }
    }

    /// Byte count of a close-delimited body when the peer closes.
    pub(super) fn at_eof(&self) -> Option<u64> {
        match self {
            Body::UntilClose { total } => Some(*total),
            _ => None,
        }
    }

    /// Consume bytes; returns how many belong to this body.
    pub(super) fn feed(&mut self, data: &[u8], cap: &mut Cap) -> (usize, BodyScan) {
        match self {
            Body::Length { left, total } => {
                let n = u64::try_from(data.len()).unwrap_or(u64::MAX).min(*left);
                *left -= n;
                let used = usize::try_from(n).unwrap_or(data.len());
                cap.put(&data[..used]);
                if *left == 0 {
                    (used, BodyScan::Done(*total))
                } else {
                    (used, BodyScan::More)
                }
            }
            Body::UntilClose { total } => {
                *total = total.saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
                cap.put(data);
                (data.len(), BodyScan::More)
            }
            Body::Chunked { state, total } => feed_chunked(state, total, data, cap),
        }
    }
}

fn feed_chunked(
    state: &mut Chunk,
    total: &mut u64,
    data: &[u8],
    cap: &mut Cap,
) -> (usize, BodyScan) {
    let mut pos = 0;
    while pos < data.len() {
        match state {
            Chunk::Data { left } => {
                let avail = u64::try_from(data.len() - pos).unwrap_or(u64::MAX);
                let n = avail.min(*left);
                *left -= n;
                *total = total.saturating_add(n);
                let used = usize::try_from(n).unwrap_or(data.len() - pos);
                cap.put(&data[pos..pos + used]);
                pos += used;
                if *left == 0 {
                    *state = Chunk::DataEnd { cr: false };
                }
            }
            Chunk::DataEnd { cr } => {
                let b = data[pos];
                pos += 1;
                match (*cr, b) {
                    (false, b'\r') => *cr = true,
                    (_, b'\n') => *state = Chunk::size_line(),
                    _ => return (pos, BodyScan::Unframed(Reason::BadChunkTerminator)),
                }
            }
            Chunk::Size {
                size,
                digits,
                ext,
                len,
                cr,
            } => {
                let b = data[pos];
                pos += 1;
                if *cr && b != b'\n' {
                    return (pos, BodyScan::Unframed(Reason::BadChunkSize));
                }
                if b == b'\r' {
                    *cr = true;
                } else if b == b'\n' {
                    if *digits == 0 {
                        return (pos, BodyScan::Unframed(Reason::BadChunkSize));
                    }
                    *state = if *size == 0 {
                        Chunk::Trailers(HeadScanner::new(Kind::Trailer))
                    } else {
                        Chunk::Data { left: *size }
                    };
                } else {
                    *len += 1;
                    if *len > CHUNK_LINE_LIMIT {
                        return (pos, BodyScan::Unframed(Reason::ChunkLineTooLong));
                    }
                    if !*ext {
                        if let Some(d) = (b as char).to_digit(16) {
                            match size.checked_mul(16) {
                                Some(s) => *size = s + u64::from(d),
                                None => return (pos, BodyScan::Unframed(Reason::BadChunkSize)),
                            }
                            *digits += 1;
                        } else if *digits > 0 && matches!(b, b';' | b' ' | b'\t') {
                            *ext = true;
                        } else {
                            return (pos, BodyScan::Unframed(Reason::BadChunkSize));
                        }
                    }
                }
            }
            Chunk::Trailers(scanner) => {
                let (n, scan) = scanner.feed(&data[pos..]);
                pos += n;
                match scan {
                    Scan::More => {}
                    Scan::Done => return (pos, BodyScan::Done(*total)),
                    Scan::Unframed(r) => return (pos, BodyScan::Unframed(r)),
                }
            }
        }
    }
    (pos, BodyScan::More)
}
