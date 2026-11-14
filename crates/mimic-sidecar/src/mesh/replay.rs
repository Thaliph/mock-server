//! The request bodies held for replay (spec §7.9 "Retries", "Bodies"): at most
//! `MIMIC_MESH_RETRY_BUFFER` per request and `MIMIC_MESH_RETRY_BUFFER_TOTAL` for all the
//! requests of the sidecar together.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use prometheus_client::metrics::gauge::Gauge;

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
}
