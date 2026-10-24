//! Capture en tee des bodies : le flux est relayé tel quel, une copie bornée est conservée.

use std::io::Read;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use http_body::{Body as HttpBody, Frame, SizeHint};
use mimic_core::model::{Body, Header};
use tokio::sync::oneshot;

/// Budget global des octets de bodies copiés en vol par les tee (spec §5.2). Une
/// réservation n'attend jamais : faute de place, la copie est sautée, pas le relais.
#[derive(Debug)]
pub struct InflightBudget {
    available: AtomicUsize,
}

impl InflightBudget {
    pub fn new(bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            available: AtomicUsize::new(bytes),
        })
    }

    /// Octets encore disponibles.
    pub fn available(&self) -> usize {
        self.available.load(Ordering::Relaxed)
    }

    fn try_reserve(&self, n: usize) -> bool {
        self.available
            .try_update(Ordering::AcqRel, Ordering::Acquire, |a| a.checked_sub(n))
            .is_ok()
    }

    fn release(&self, n: usize) {
        self.available.fetch_add(n, Ordering::AcqRel);
    }
}

/// Octets du budget tenus par la copie d'un body, rendus quand elle est consommée ou jetée.
#[derive(Debug, Default)]
pub struct Lease {
    budget: Option<Arc<InflightBudget>>,
    bytes: usize,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(b) = &self.budget
            && self.bytes > 0
        {
            b.release(self.bytes);
        }
    }
}

/// Ce que la capture a vu d'un body.
#[derive(Debug)]
pub struct Captured {
    pub prefix: Vec<u8>,
    pub size: u64,
    /// Faux si le flux a été interrompu (erreur, client parti).
    pub complete: bool,
    pub ended: Instant,
    /// Vrai si une partie du body n'a pas été copiée, faute de budget en vol.
    pub over_budget: bool,
    /// Part du budget en vol tenue par `prefix`, rendue avec lui.
    pub lease: Lease,
    /// Trailers du body (HTTP/2 : `grpc-status`), vides s'il n'y en a pas.
    pub trailers: Vec<Header>,
}

impl Captured {
    /// Conversion vers le modèle, en décodant `gzip`, `deflate`, `br` ou `zstd` si nécessaire.
    pub fn into_body(mut self, keep: bool, limit: usize, content_encoding: Option<&str>) -> Body {
        if self.size == 0 {
            return Body::Empty;
        }
        if !keep {
            return Body::Skipped { size: self.size };
        }
        if !self.complete || self.over_budget || self.size > self.prefix.len() as u64 {
            return Body::Truncated {
                size: self.size,
                prefix: std::mem::take(&mut self.prefix),
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

/// Body qui relaie `inner` et en garde une copie des `limit` premiers octets, dans la
/// limite du budget en vol s'il y en a un.
pub struct TeeBody<B> {
    inner: B,
    limit: usize,
    prefix: Vec<u8>,
    size: u64,
    lease: Lease,
    over_budget: bool,
    trailers: Vec<Header>,
    tx: Option<oneshot::Sender<Captured>>,
}

impl<B: HttpBody<Data = Bytes> + Unpin> TeeBody<B> {
    pub fn new(inner: B, limit: usize) -> (Self, oneshot::Receiver<Captured>) {
        Self::build(inner, limit, None)
    }

    /// Comme [`TeeBody::new`], les octets copiés étant pris sur `budget` (spec §5.2).
    pub fn with_budget(
        inner: B,
        limit: usize,
        budget: Arc<InflightBudget>,
    ) -> (Self, oneshot::Receiver<Captured>) {
        Self::build(inner, limit, Some(budget))
    }

    fn build(
        inner: B,
        limit: usize,
        budget: Option<Arc<InflightBudget>>,
    ) -> (Self, oneshot::Receiver<Captured>) {
        let (tx, rx) = oneshot::channel();
        let mut tee = Self {
            inner,
            limit,
            prefix: vec![],
            size: 0,
            lease: Lease { budget, bytes: 0 },
            over_budget: false,
            trailers: vec![],
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
            let _ = tx.send(self.captured(complete));
        }
    }
}

impl<B> TeeBody<B> {
    fn captured(&mut self, complete: bool) -> Captured {
        Captured {
            prefix: std::mem::take(&mut self.prefix),
            size: self.size,
            complete,
            ended: Instant::now(),
            over_budget: self.over_budget,
            lease: std::mem::take(&mut self.lease),
            trailers: std::mem::take(&mut self.trailers),
        }
    }

    /// Copie le fragment dans la limite de `limit` et du budget ; un fragment refusé par le
    /// budget arrête la copie de ce body, sans jamais retarder le relais.
    fn copy(&mut self, data: &[u8]) {
        let room = self.limit.saturating_sub(self.prefix.len());
        let take = room.min(data.len());
        if take == 0 || self.over_budget {
            return;
        }
        if let Some(budget) = &self.lease.budget {
            if !budget.try_reserve(take) {
                self.over_budget = true;
                return;
            }
            self.lease.bytes += take;
        }
        self.prefix.extend_from_slice(&data[..take]);
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
                    this.copy(data);
                }
                if let Some(trailers) = frame.trailers_ref() {
                    this.trailers = trailers
                        .iter()
                        .map(|(n, v)| {
                            Header::new(n.as_str(), String::from_utf8_lossy(v.as_bytes()))
                        })
                        .collect();
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
            let _ = tx.send(self.captured(false));
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

    /// Budget en vol (CAP-11) : au-delà, le body n'est plus copié mais reste relayé en
    /// entier ; le budget est rendu quand la copie est consommée.
    #[tokio::test]
    async fn cap_11_budget_bounds_copies_and_is_released() {
        let budget = InflightBudget::new(150);
        let (a, ra) =
            TeeBody::with_budget(Full::new(Bytes::from(vec![1u8; 100])), 1000, budget.clone());
        let (b, rb) =
            TeeBody::with_budget(Full::new(Bytes::from(vec![2u8; 100])), 1000, budget.clone());
        assert_eq!(a.collect().await.unwrap().to_bytes().len(), 100);
        assert_eq!(b.collect().await.unwrap().to_bytes().len(), 100);
        let (ca, cb) = (ra.await.unwrap(), rb.await.unwrap());
        assert!(!ca.over_budget && ca.prefix.len() == 100);
        assert!(cb.over_budget && cb.prefix.is_empty());
        assert_eq!(budget.available(), 50);
        assert!(matches!(
            cb.into_body(true, 1000, None),
            Body::Truncated { size: 100, .. }
        ));
        assert_eq!(budget.available(), 50);
        assert_eq!(
            ca.into_body(true, 1000, None),
            Body::Captured {
                data: vec![1u8; 100]
            }
        );
        assert_eq!(budget.available(), 150);
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
            over_budget: false,
            lease: Lease::default(),
            trailers: vec![],
        };
        assert_eq!(
            c.into_body(true, 100, Some("gzip")),
            Body::captured("hello")
        );
    }
}
