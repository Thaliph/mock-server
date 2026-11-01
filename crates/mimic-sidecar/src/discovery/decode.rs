//! Content decoding of the captured bodies (spec §6), on the recorder thread. The output is
//! bounded: a body that would decode to more than the capture limit is refused, so a small
//! compressed body cannot grow into memory (decompression bomb).

use std::borrow::Cow;
use std::io::Read;

use flate2::read::{DeflateDecoder, GzDecoder, ZlibDecoder};
use mimic_core::proxy::http1::CAPTURE_BODY_LIMIT;

/// Why a body could not be decoded.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Failure {
    /// A `content-encoding` that is not decoded (`zstd`, a list, an unknown coding).
    Encoding,
    /// The decoded body is over the limit.
    TooLarge,
    /// The compressed stream is corrupt.
    Invalid,
}

impl Failure {
    /// The `result` of `mimic_discovery_bodies_total`.
    pub(super) fn result(&self) -> &'static str {
        match self {
            Failure::Encoding => "encoding",
            Failure::TooLarge => "too_large",
            Failure::Invalid => "invalid",
        }
    }
}

/// A zlib stream starts with a header whose two bytes are a multiple of 31 (RFC 1950); some
/// servers send a raw deflate stream under the same coding name.
fn is_zlib(data: &[u8]) -> bool {
    matches!(data, [cmf, flg, ..] if cmf & 0x0f == 8 && (u16::from(*cmf) << 8 | u16::from(*flg)) % 31 == 0)
}

fn bounded(reader: impl Read) -> Result<Vec<u8>, Failure> {
    let mut out = Vec::new();
    reader
        .take(CAPTURE_BODY_LIMIT as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|_| Failure::Invalid)?;
    if out.len() > CAPTURE_BODY_LIMIT {
        return Err(Failure::TooLarge);
    }
    Ok(out)
}

/// `data` decoded according to the value of `content-encoding` (`None` when absent).
pub(super) fn decode<'a>(data: &'a [u8], encoding: Option<&str>) -> Result<Cow<'a, [u8]>, Failure> {
    let encoding = encoding.unwrap_or_default().trim().to_ascii_lowercase();
    let decoded = match encoding.as_str() {
        "" | "identity" => return Ok(Cow::Borrowed(data)),
        "gzip" | "x-gzip" => bounded(GzDecoder::new(data))?,
        "deflate" if is_zlib(data) => bounded(ZlibDecoder::new(data))?,
        "deflate" => bounded(DeflateDecoder::new(data))?,
        "br" => bounded(brotli_decompressor::Decompressor::new(data, 4096))?,
        _ => return Err(Failure::Encoding),
    };
    Ok(Cow::Owned(decoded))
}
