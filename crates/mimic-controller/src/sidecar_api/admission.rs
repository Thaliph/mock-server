//! What a sidecar API request must pass before its body is read (spec §7.2): the per-source
//! rate limit, the cache of successful `TokenReview`s and the budget of bytes being read.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::time::Instant;

use super::{Bucket, Review};

/// Sources tracked by the rate limit; the least recently seen is forgotten first.
pub const MAX_RATE_SOURCES: usize = 65_536;
/// How long a successful `TokenReview` is reused.
pub const REVIEW_TTL: Duration = Duration::from_secs(60);
/// Reviews cached at most.
pub const MAX_REVIEWS: usize = 10_000;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Default)]
struct RateState {
    /// Bucket of each source and the tick of its last request.
    sources: BTreeMap<IpAddr, (Bucket, u64)>,
    /// Sources by tick of their last request, oldest first.
    recency: BTreeMap<u64, IpAddr>,
    tick: u64,
}

/// A token bucket per peer address: `per_second` requests per second, a burst of twice that.
pub struct RateLimiter {
    per_second: u32,
    burst: u32,
    max_sources: usize,
    state: Mutex<RateState>,
}

impl RateLimiter {
    pub fn new(per_second: u32) -> RateLimiter {
        RateLimiter::with_sources(per_second, MAX_RATE_SOURCES)
    }

    pub fn with_sources(per_second: u32, max_sources: usize) -> RateLimiter {
        RateLimiter {
            per_second,
            burst: per_second.saturating_mul(2),
            max_sources,
            state: Mutex::default(),
        }
    }

    /// Takes one request from the budget of `source`; false when it is spent.
    pub fn allow(&self, source: Option<IpAddr>) -> bool {
        let source = source.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let mut state = lock(&self.state);
        let state = &mut *state;
        state.tick += 1;
        let tick = state.tick;
        let seen = match state.sources.get_mut(&source) {
            Some((_, last)) => Some(std::mem::replace(last, tick)),
            None => None,
        };
        match seen {
            Some(last) => {
                state.recency.remove(&last);
            }
            None => {
                if state.sources.len() >= self.max_sources
                    && let Some((_, oldest)) = state.recency.pop_first()
                {
                    state.sources.remove(&oldest);
                }
                state
                    .sources
                    .insert(source, (Bucket::full(self.burst), tick));
            }
        }
        state.recency.insert(tick, source);
        let Some((bucket, _)) = state.sources.get_mut(&source) else {
            return true;
        };
        if bucket.refill(self.burst, self.per_second) {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    pub fn sources(&self) -> usize {
        lock(&self.state).sources.len()
    }
}

/// Successful reviews, by the SHA-256 of the token: the token itself is never kept.
pub struct ReviewCache {
    ttl: Duration,
    max: usize,
    entries: Mutex<BTreeMap<[u8; 32], (Instant, Review)>>,
}

impl Default for ReviewCache {
    fn default() -> ReviewCache {
        ReviewCache::new(REVIEW_TTL, MAX_REVIEWS)
    }
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

impl ReviewCache {
    pub fn new(ttl: Duration, max: usize) -> ReviewCache {
        ReviewCache {
            ttl,
            max,
            entries: Mutex::default(),
        }
    }

    /// The review of `token`, if one was made less than the TTL ago.
    pub fn get(&self, token: &str) -> Option<Review> {
        let key = digest(token);
        let mut entries = lock(&self.entries);
        match entries.get(&key) {
            Some((expires, review)) if *expires > Instant::now() => Some(review.clone()),
            Some(_) => {
                entries.remove(&key);
                None
            }
            None => None,
        }
    }

    /// Remembers a successful review. When the cache is full the expired entries go, then
    /// the one closest to expiry.
    pub fn put(&self, token: &str, review: Review) {
        let now = Instant::now();
        let mut entries = lock(&self.entries);
        if entries.len() >= self.max {
            entries.retain(|_, (expires, _)| *expires > now);
        }
        while entries.len() >= self.max.max(1) {
            let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, (expires, _))| *expires)
                .map(|(key, _)| *key)
            else {
                break;
            };
            entries.remove(&oldest);
        }
        entries.insert(digest(token), (now + self.ttl, review));
    }
}

/// Bytes of push bodies being read at once, reserved from the declared `Content-Length`.
pub struct BodyBudget {
    capacity: u64,
    used: AtomicU64,
}

/// A reservation; released when dropped.
pub struct Reservation<'a> {
    budget: &'a BodyBudget,
    bytes: u64,
}

impl BodyBudget {
    pub fn new(capacity: u64) -> BodyBudget {
        BodyBudget {
            capacity,
            used: AtomicU64::new(0),
        }
    }

    /// Reserves `bytes`, or None when that would exceed the capacity.
    pub fn reserve(&self, bytes: u64) -> Option<Reservation<'_>> {
        self.used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= self.capacity)
            })
            .ok()
            .map(|_| Reservation {
                budget: self,
                bytes,
            })
    }

    /// Bytes reserved now.
    #[cfg(test)]
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn ctl_08_the_table_of_sources_forgets_the_least_recently_seen() {
        let limiter = RateLimiter::with_sources(1, 2);
        let ip = |n: u8| Some(IpAddr::from([10, 0, 0, n]));
        // Burst 2: source 1 spends its budget.
        assert!(limiter.allow(ip(1)) && limiter.allow(ip(1)) && !limiter.allow(ip(1)));
        assert!(limiter.allow(ip(2)));
        // Source 1 was seen last before 2: a third source pushes out 2, not 1.
        assert!(!limiter.allow(ip(1)));
        assert!(limiter.allow(ip(3)));
        assert_eq!(limiter.sources(), 2);
        assert!(!limiter.allow(ip(1)), "its budget was kept");
        assert!(limiter.allow(ip(2)), "forgotten sources start full");
    }

    #[test]
    fn ctl_07_the_budget_reserves_and_releases() {
        let budget = BodyBudget::new(10);
        let first = budget.reserve(6).expect("fits");
        assert!(budget.reserve(5).is_none());
        assert_eq!(budget.used(), 6);
        drop(first);
        assert!(budget.reserve(10).is_some());
        assert!(budget.reserve(u64::MAX).is_none());
    }
}
