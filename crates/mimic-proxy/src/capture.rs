//! Capture en tee des bodies : le flux est relayé tel quel, une copie bornée est conservée.

use std::io::Read;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use http_body::{Body as HttpBody, Frame, SizeHint};
use mimic_core::model::Body;
use tokio::sync::oneshot;

/// Ce que la capture a vu d'un body.
#[derive(Debug)]
pub struct Captured {
    pub prefix: Vec<u8>,
    pub size: u64,
    /// Faux si le flux a été interrompu (erreur, client parti).
    pub complete: bool,
    pub ended: Instant,
}

impl Captured {
    /// Conversion vers le modèle, en décodant `gzip`, `deflate`, `br` ou `zstd` si nécessaire.
    pub fn into_body(self, keep: bool, limit: usize, content_encoding: Option<&str>) -> Body {
        if self.size == 0 {
            return Body::Empty;
        }
        if !keep {
            return Body::Skipped { size: self.size };
        }
        if !self.complete || self.size > self.prefix.len() as u64 {
            return Body::Truncated {
                size: self.size,
                prefix: self.prefix,
            };
        }
        match content_encoding.map(|c| c.trim().to_ascii_lowercase()) {
            None => Body::Captured { data: self.prefix },
            Some(c) if c == "identity" => Body::Captured { data: self.prefix },
            Some(c) => match decode(&c, &self.prefix, limit) {
                Some(data) => Body::Captured { data },
                // Encodage non géré ou corrompu : rien d'exploitable pour l'inférence ni le rejeu.
                None => Body::Skipped { size: self.size },
            },
        }
    }
}

/// Décode un body compressé, borné à `limit` octets décodés.
pub(crate) fn decode(encoding: &str, data: &[u8], limit: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let read = match encoding {
        "gzip" | "x-gzip" => flate2::read::GzDecoder::new(data)
            .take(limit as u64 + 1)
            .read_to_end(&mut out),
        "deflate" => flate2::read::ZlibDecoder::new(data)
            .take(limit as u64 + 1)
            .read_to_end(&mut out),
        "br" => brotli::Decompressor::new(data, 4096)
            .take(limit as u64 + 1)
            .read_to_end(&mut out),
        "zstd" => zstd::stream::read::Decoder::new(data)
            .and_then(|d| d.take(limit as u64 + 1).read_to_end(&mut out)),
        _ => return None,
    };
    (read.is_ok() && out.len() <= limit).then_some(out)
}

/// Encode un body pour le rejeu ; `None` si l'encodage n'est pas géré.
pub fn encode(encoding: &str, data: &[u8]) -> Option<Vec<u8>> {
    use std::io::Write;
    match encoding.trim().to_ascii_lowercase().as_str() {
        "gzip" | "x-gzip" => {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            e.write_all(data).ok()?;
            e.finish().ok()
        }
        "deflate" => {
            let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            e.write_all(data).ok()?;
            e.finish().ok()
        }
        "br" => {
            let mut out = Vec::new();
            let mut e = brotli::CompressorWriter::new(&mut out, 4096, 1, 22);
            e.write_all(data).ok()?;
            e.flush().ok()?;
            drop(e);
            Some(out)
        }
        "zstd" => zstd::encode_all(data, 1).ok(),
        "identity" => Some(data.to_vec()),
        _ => None,
    }
}

/// Body qui relaie `inner` et en garde une copie des `limit` premiers octets.
pub struct TeeBody<B> {
    inner: B,
    limit: usize,
    prefix: Vec<u8>,
    size: u64,
    tx: Option<oneshot::Sender<Captured>>,
}

impl<B: HttpBody<Data = Bytes> + Unpin> TeeBody<B> {
    pub fn new(inner: B, limit: usize) -> (Self, oneshot::Receiver<Captured>) {
        let (tx, rx) = oneshot::channel();
        let mut tee = Self {
            inner,
            limit,
            prefix: vec![],
            size: 0,
            tx: Some(tx),
        };
        // Un body vide peut ne jamais être lu : on le déclare terminé tout de suite.
        if tee.inner.is_end_stream() {
            tee.finish(true);
        }
        (tee, rx)
    }

    fn finish(&mut self, complete: bool) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Captured {
                prefix: std::mem::take(&mut self.prefix),
                size: self.size,
                complete,
                ended: Instant::now(),
            });
        }
    }
}

impl<B: HttpBody<Data = Bytes> + Unpin> HttpBody for TeeBody<B> {
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, B::Error>>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.size += data.len() as u64;
                    let room = this.limit.saturating_sub(this.prefix.len());
                    this.prefix.extend_from_slice(&data[..room.min(data.len())]);
                }
                if this.inner.is_end_stream() {
                    this.finish(true);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.finish(false);
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.finish(true);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl<B> Drop for TeeBody<B> {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Captured {
                prefix: std::mem::take(&mut self.prefix),
                size: self.size,
                complete: false,
                ended: Instant::now(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Full};

    #[tokio::test]
    async fn tee_keeps_bounded_prefix_and_relays_everything() {
        let (tee, rx) = TeeBody::new(Full::new(Bytes::from(vec![7u8; 100])), 10);
        let relayed = tee.collect().await.unwrap().to_bytes();
        assert_eq!(relayed.len(), 100);
        let c = rx.await.unwrap();
        assert_eq!(c.size, 100);
        assert_eq!(c.prefix.len(), 10);
        assert!(c.complete);
        assert!(matches!(
            c.into_body(true, 10, None),
            Body::Truncated { size: 100, .. }
        ));
    }

    #[tokio::test]
    async fn empty_body_finishes_immediately() {
        let (_tee, rx) = TeeBody::new(http_body_util::Empty::<Bytes>::new(), 10);
        let c = rx.await.unwrap();
        assert_eq!(c.size, 0);
        assert!(matches!(c.into_body(true, 10, None), Body::Empty));
    }

    #[test]
    fn gzip_roundtrip() {
        let gz = encode("gzip", b"hello").unwrap();
        let c = Captured {
            size: gz.len() as u64,
            prefix: gz,
            complete: true,
            ended: Instant::now(),
        };
        assert_eq!(
            c.into_body(true, 100, Some("gzip")),
            Body::captured("hello")
        );
    }
}
