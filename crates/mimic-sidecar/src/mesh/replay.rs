//! The request bodies held for replay (spec §7.9 "Retries", "Bodies"): at most
//! `MIMIC_MESH_RETRY_BUFFER` per request and `MIMIC_MESH_RETRY_BUFFER_TOTAL` for all the
//! requests of the sidecar together.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use bytes::Bytes;
use hyper::Request;
use hyper::body::{Body, Frame, SizeHint};
use hyper::header::HeaderMap;
use hyper::http::request::Parts;
use prometheus_client::metrics::gauge::Gauge;

use crate::exchange::ReqBody;

/// The bytes held by all the requests of the sidecar.
pub struct ReplayBudget {
    total: usize,
    used: AtomicUsize,
    gauge: Gauge,
}

impl ReplayBudget {
    /// `gauge` is `mimic_mesh_retry_buffer_bytes`.
    pub fn new(total: usize, gauge: Gauge) -> Arc<ReplayBudget> {
        Arc::new(ReplayBudget {
            total,
            used: AtomicUsize::new(0),
            gauge,
        })
    }

    /// `mimic_mesh_retry_buffer_bytes`.
    pub fn gauge(&self) -> &Gauge {
        &self.gauge
    }

    /// The bytes held now.
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    /// A buffer for one request, at most `cap` bytes.
    pub fn replay(self: &Arc<Self>, cap: usize) -> Replay {
        Replay {
            budget: Arc::clone(self),
            cap,
            held: 0,
            chunks: Vec::new(),
            state: State::Held,
        }
    }

    /// Takes `n` bytes of the total, or none.
    fn reserve(&self, n: usize) -> bool {
        let total = self.total;
        let taken = self
            .used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(n).filter(|sum| *sum <= total)
            })
            .is_ok();
        if taken {
            self.gauge.inc_by(i64::try_from(n).unwrap_or(i64::MAX));
        }
        taken
    }

    fn give_back(&self, n: usize) {
        self.used.fetch_sub(n, Ordering::AcqRel);
        self.gauge.dec_by(i64::try_from(n).unwrap_or(i64::MAX));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Held,
    /// A chunk did not fit: streamed from now on.
    Streamed,
    /// A response head came back.
    Released,
}

/// The body of one request, held while it can still be sent again. Dropping it releases its
/// bytes.
pub struct Replay {
    budget: Arc<ReplayBudget>,
    cap: usize,
    held: usize,
    chunks: Vec<Bytes>,
    state: State,
}

impl Replay {
    /// Holds a chunk read from the app. `false`: it does not fit (the request's share or the
    /// total), the chunks held are released and the body is streamed from now on.
    pub fn push(&mut self, chunk: &Bytes) -> bool {
        if self.state != State::Held {
            return false;
        }
        let len = chunk.len();
        let fits = len <= self.cap - self.held && self.budget.reserve(len);
        if !fits {
            self.drop_chunks();
            self.state = State::Streamed;
            return false;
        }
        self.held += len;
        if len > 0 {
            self.chunks.push(chunk.clone());
        }
        true
    }

    /// Every byte read so far is held (and the request was not released).
    pub fn is_held(&self) -> bool {
        self.state == State::Held
    }

    /// The chunks to send again, in order.
    pub fn chunks(&self) -> &[Bytes] {
        &self.chunks
    }

    /// The request can no longer be retried (a response head came back): releases the bytes.
    pub fn release(&mut self) {
        self.drop_chunks();
        self.state = State::Released;
    }

    fn drop_chunks(&mut self) {
        self.chunks.clear();
        if self.held > 0 {
            self.budget.give_back(self.held);
            self.held = 0;
        }
    }
}

impl Drop for Replay {
    fn drop(&mut self) {
        self.drop_chunks();
    }
}

struct Shared {
    body: ReqBody,
    replay: Replay,
    ended: bool,
    trailers: Option<HeaderMap>,
    /// A frame was read from the app.
    read: bool,
    /// The number of tries that took the body: only the last one reads it.
    generation: u64,
    /// The request ended before an answer was delivered: no try reads any more.
    closed: bool,
    /// An answer was delivered: the request is not retried and its last try goes on.
    delivered: bool,
}

/// The body of a request over its tries: what the app sends is read once, held for replay while
/// it fits, and each try gets an [`Attempt`] that starts with what was held.
pub(crate) struct Source {
    shared: Arc<Mutex<Shared>>,
    hint: SizeHint,
}

fn lock(shared: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Source {
    pub(crate) fn new(body: ReqBody, replay: Replay) -> Source {
        let hint = body.size_hint();
        Source {
            shared: Arc::new(Mutex::new(Shared {
                body,
                replay,
                ended: false,
                trailers: None,
                read: false,
                generation: 0,
                closed: false,
                delivered: false,
            })),
            hint,
        }
    }

    /// Every byte read from the app is still held (spec §7.9 "Bodies").
    pub(crate) fn held(&self) -> bool {
        lock(&self.shared).replay.is_held()
    }

    /// The request can be sent again whole: nothing was read, or all of it is held.
    pub(crate) fn intact(&self) -> bool {
        let shared = lock(&self.shared);
        shared.replay.is_held() || !shared.read
    }

    /// A response head came back: the bytes held are released.
    pub(crate) fn release(&self) {
        let mut shared = lock(&self.shared);
        shared.delivered = true;
        shared.replay.release();
    }

    /// The request for one try: `head` and a body that starts with the bytes held.
    pub(crate) fn request(&self, head: &Parts) -> Request<Attempt> {
        // The try before this one reads no more: the bytes it took are in the snapshot.
        let (queue, generation) = {
            let mut shared = lock(&self.shared);
            shared.generation += 1;
            (
                shared.replay.chunks().iter().cloned().collect(),
                shared.generation,
            )
        };
        let mut request = Request::new(Attempt {
            shared: self.shared.clone(),
            generation,
            queue,
            hint: self.hint,
            trailed: false,
        });
        *request.method_mut() = head.method.clone();
        *request.uri_mut() = head.uri.clone();
        *request.version_mut() = head.version;
        *request.headers_mut() = head.headers.clone();
        *request.extensions_mut() = head.extensions.clone();
        request
    }
}

impl Drop for Source {
    /// A request that ends before its answer (the app left) frees the bytes held at once, and
    /// the tries still pending fail their bodies.
    fn drop(&mut self) {
        let mut shared = lock(&self.shared);
        if !shared.delivered {
            shared.closed = true;
            shared.replay.release();
        }
    }
}

/// The body of one try.
pub(crate) struct Attempt {
    shared: Arc<Mutex<Shared>>,
    generation: u64,
    /// What was held when the try began, still to send.
    queue: VecDeque<Bytes>,
    hint: SizeHint,
    /// The trailers were handed over.
    trailed: bool,
}

impl Attempt {
    /// A later try took the body, or the request is over: this one fails and reads nothing.
    fn stale(&self, shared: &Shared) -> bool {
        shared.closed || shared.generation != self.generation
    }
}

impl Body for Attempt {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        let this = self.get_mut();
        let mut shared = lock(&this.shared);
        if this.stale(&shared) {
            // An error, never the end of the body: the connection of this try must not be able
            // to complete a request that is cut (`hyper` closes an HTTP/1 connection, resets an
            // HTTP/2 stream).
            this.queue.clear();
            return Poll::Ready(Some(Err(std::io::Error::other(
                "the request went to another pod",
            ))));
        }
        if let Some(chunk) = this.queue.pop_front() {
            return Poll::Ready(Some(Ok(Frame::data(chunk))));
        }
        if shared.ended {
            if !std::mem::replace(&mut this.trailed, true)
                && let Some(trailers) = shared.trailers.clone()
            {
                return Poll::Ready(Some(Ok(Frame::trailers(trailers))));
            }
            return Poll::Ready(None);
        }
        let polled = Pin::new(&mut shared.body).poll_frame(cx);
        match polled {
            Poll::Ready(Some(Ok(frame))) => {
                shared.read = true;
                match frame.into_data() {
                    Ok(data) => {
                        shared.replay.push(&data);
                        Poll::Ready(Some(Ok(Frame::data(data))))
                    }
                    Err(frame) => {
                        shared.ended = true;
                        this.trailed = true;
                        if let Ok(trailers) = frame.into_trailers() {
                            shared.trailers = Some(trailers.clone());
                            return Poll::Ready(Some(Ok(Frame::trailers(trailers))));
                        }
                        Poll::Ready(None)
                    }
                }
            }
            Poll::Ready(None) => {
                shared.ended = true;
                this.trailed = true;
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(std::io::Error::other(e)))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        let shared = lock(&self.shared);
        if self.stale(&shared) {
            return false;
        }
        if !self.queue.is_empty() {
            return false;
        }
        if shared.ended {
            shared.trailers.is_none() || self.trailed
        } else {
            shared.body.is_end_stream()
        }
    }

    fn size_hint(&self) -> SizeHint {
        self.hint
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(total: usize) -> (Arc<ReplayBudget>, Gauge) {
        let gauge = Gauge::default();
        (ReplayBudget::new(total, gauge.clone()), gauge)
    }

    fn chunk(n: usize) -> Bytes {
        Bytes::from(vec![7u8; n])
    }

    #[test]
    fn rty_14_a_body_within_its_share_is_held_and_replayed() {
        let (budget, gauge) = budget(1 << 20);
        let mut replay = budget.replay(64 * 1024);
        assert!(replay.push(&chunk(30 * 1024)));
        assert!(replay.push(&chunk(30 * 1024)));
        assert!(replay.is_held());
        assert_eq!(replay.chunks().len(), 2);
        assert_eq!(budget.used(), 60 * 1024);
        assert_eq!(gauge.get(), 60 * 1024);
        replay.release();
        assert!(!replay.is_held(), "released: no longer retryable");
        assert_eq!(budget.used(), 0);
        assert_eq!(gauge.get(), 0);
    }

    #[test]
    fn rty_14_a_body_past_its_share_is_streamed_and_its_bytes_released() {
        let (budget, gauge) = budget(1 << 20);
        let mut replay = budget.replay(64 * 1024);
        assert!(replay.push(&chunk(60 * 1024)));
        assert!(
            !replay.push(&chunk(10 * 1024)),
            "70 KiB does not fit 64 KiB"
        );
        assert!(!replay.is_held());
        assert!(replay.chunks().is_empty());
        assert_eq!(budget.used(), 0);
        assert!(!replay.push(&chunk(1)), "streamed for good");
        assert_eq!(gauge.get(), 0);
    }

    #[test]
    fn rty_14_the_total_is_never_exceeded() {
        let (budget, gauge) = budget(1 << 20);
        let mut held: Vec<Replay> = (0..16)
            .map(|_| {
                let mut r = budget.replay(64 * 1024);
                assert!(r.push(&chunk(64 * 1024)));
                r
            })
            .collect();
        assert_eq!(gauge.get(), 1 << 20);
        let mut late = budget.replay(64 * 1024);
        assert!(!late.push(&chunk(1)), "the 17th finds the total in use");
        assert!(!late.is_held());
        assert_eq!(budget.used(), 1 << 20);
        // A request answered frees its share for the next one.
        held.pop().unwrap().release();
        let mut next = budget.replay(64 * 1024);
        assert!(next.push(&chunk(64 * 1024)));
        assert_eq!(gauge.get(), 1 << 20);
        drop(held);
        drop(next);
        assert_eq!(budget.used(), 0, "dropping a request releases its bytes");
        assert_eq!(gauge.get(), 0);
    }

    #[test]
    fn rty_14_a_zero_buffer_holds_only_an_empty_body() {
        let (budget, _) = budget(1 << 20);
        let mut empty = budget.replay(0);
        assert!(empty.push(&Bytes::new()));
        assert!(empty.is_held());
        let mut body = budget.replay(0);
        assert!(!body.push(&chunk(1)));
        assert!(!body.is_held());
    }

    #[test]
    fn rty_14_a_body_with_no_chunk_is_held() {
        let (budget, _) = budget(1 << 20);
        assert!(budget.replay(64 * 1024).is_held());
    }

    /// A request body arriving from a connection: the request `hyper` serves for it.
    async fn incoming(
        client: &mut (impl tokio::io::AsyncWrite + Unpin),
        server: tokio::io::DuplexStream,
    ) -> hyper::body::Incoming {
        use tokio::io::AsyncWriteExt;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let tx = Mutex::new(Some(tx));
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req| {
                let tx = tx.lock().unwrap().take();
                async move {
                    if let Some(tx) = tx {
                        let _ = tx.send(req);
                    }
                    std::future::pending::<
                        Result<
                            hyper::Response<http_body_util::Empty<Bytes>>,
                            std::convert::Infallible,
                        >,
                    >()
                    .await
                }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(server), service)
                .await;
        });
        client
            .write_all(
                b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n",
            )
            .await
            .unwrap();
        rx.await.unwrap().into_body()
    }

    async fn next(body: &mut Attempt) -> Option<Result<Frame<Bytes>, std::io::Error>> {
        std::future::poll_fn(|cx| Pin::new(&mut *body).poll_frame(cx)).await
    }

    #[tokio::test]
    async fn rty_14_a_superseded_try_reads_no_more_of_the_body() {
        use tokio::io::AsyncWriteExt;
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let incoming = incoming(&mut client, server).await;
        let (budget, _) = budget(1 << 20);
        let source = Source::new(ReqBody::plain(incoming), budget.replay(64 * 1024));
        let head = Request::new(()).into_parts().0;
        let mut first = source.request(&head).into_body();
        let got = next(&mut first).await.unwrap().unwrap();
        assert_eq!(got.into_data().ok(), Some(Bytes::from_static(b"abc")));
        // The first try waits for more, and a second try takes over the body.
        assert!(
            futures::poll!(std::pin::pin!(next(&mut first))).is_pending(),
            "waiting for the app"
        );
        let mut second = source.request(&head).into_body();
        client.write_all(b"3\r\ndef\r\n").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let old = next(&mut first).await;
        assert!(
            matches!(old, Some(Err(_))),
            "the first try fails, it does not read"
        );
        let mut seen = Vec::new();
        for _ in 0..2 {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(2), next(&mut second))
                .await
                .expect("a chunk, not a hole")
                .unwrap()
                .unwrap();
            seen.extend_from_slice(&frame.into_data().unwrap());
        }
        assert_eq!(seen, b"abcdef");
    }
}
