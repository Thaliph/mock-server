//! Client-side load balancing over the endpoints of one Service port (spec §7.9, design §3.2):
//! power of two choices on the least request, weighted by slow start. Pure: the clock and the
//! random draws are passed in.

use std::net::SocketAddr;
use std::time::Duration;

/// How many times [`Endpoints::pick_unejected`] draws again for an ejected endpoint.
pub const REDRAWS: u32 = 8;

/// The lowest weight of an endpoint in slow start.
pub const MIN_WEIGHT: f64 = 0.1;

/// The endpoints of one Service port, sorted by address, with their slow-start state.
#[derive(Debug, Clone)]
pub struct Endpoints {
    window: Duration,
    members: Vec<Member>,
    seen: bool,
}

#[derive(Debug, Clone)]
struct Member {
    addr: SocketAddr,
    /// When the endpoint was first seen; `None`: seen in the first update, full weight.
    born: Option<Duration>,
}

impl Endpoints {
    /// No endpoint yet; the first [`Endpoints::update`] is the first view.
    pub fn new(window: Duration) -> Endpoints {
        Endpoints {
            window,
            members: Vec::new(),
            seen: false,
        }
    }

    /// Changes the slow-start window; zero disables the slow start.
    pub fn set_window(&mut self, window: Duration) {
        self.window = window;
    }

    /// Replaces the endpoints by `addrs` at `now` (any order, duplicates ignored). An endpoint
    /// already known keeps its first-seen time; the endpoints of the first update have full
    /// weight, the ones that join later start in slow start.
    pub fn update(&mut self, addrs: &[SocketAddr], now: Duration) {
        let mut sorted = addrs.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let first = !self.seen;
        let members = sorted
            .into_iter()
            .map(|addr| {
                let born = match self.members.binary_search_by_key(&addr, |m| m.addr) {
                    Ok(known) => self.members[known].born,
                    Err(_) if first => None,
                    Err(_) => Some(now),
                };
                Member { addr, born }
            })
            .collect();
        self.members = members;
        self.seen = true;
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub fn addr(&self, index: usize) -> Option<SocketAddr> {
        self.members.get(index).map(|m| m.addr)
    }

    /// `max(0.1, min(1, age / window))` for an endpoint in slow start, 1 otherwise.
    pub fn weight(&self, index: usize, now: Duration) -> f64 {
        let Some(born) = self.members.get(index).and_then(|m| m.born) else {
            return 1.0;
        };
        if self.window.is_zero() {
            return 1.0;
        }
        let age = now.saturating_sub(born);
        (age.as_secs_f64() / self.window.as_secs_f64()).clamp(MIN_WEIGHT, 1.0)
    }

    /// The index of the endpoint to send a request to: two distinct endpoints are drawn
    /// (`draw(n)` returns a number below `n`) and the one with the lower
    /// `(in_flight + 1) / weight` wins, the first draw on a tie. One endpoint is taken without
    /// a draw, none gives `None`. O(1): the draws index the set directly (MSH-46).
    pub fn pick(
        &self,
        now: Duration,
        in_flight: impl Fn(usize) -> u32,
        draw: &mut impl FnMut(usize) -> usize,
    ) -> Option<usize> {
        let n = self.members.len();
        match n {
            0 => return None,
            1 => return Some(0),
            _ => {}
        }
        let first = draw(n).min(n - 1);
        let mut second = draw(n - 1).min(n - 2);
        if second >= first {
            second += 1;
        }
        Some(self.better(first, second, now, &in_flight))
    }

    /// [`Endpoints::pick`] that never returns an endpoint `ejected` accepts (spec §7.9 "Outlier
    /// detection"): the same two draws, so with nothing ejected the choice is `pick`'s, draw
    /// for draw. A drawn endpoint that is ejected is drawn again (at most [`REDRAWS`] times
    /// for the pair, which keeps the pick O(1) while few are ejected); past that the choice is
    /// made among the live ones as [`Endpoints::pick_where`] does (O(n)). `None` with a
    /// non-empty set means every endpoint is ejected.
    pub fn pick_unejected(
        &self,
        now: Duration,
        in_flight: impl Fn(usize) -> u32,
        draw: &mut impl FnMut(usize) -> usize,
        ejected: impl Fn(SocketAddr) -> bool,
    ) -> Option<usize> {
        let n = self.members.len();
        let live = |i: usize| self.members.get(i).is_some_and(|m| !ejected(m.addr));
        match n {
            0 => return None,
            1 => return live(0).then_some(0),
            _ => {}
        }
        let mut redraws = 0;
        let mut live_draw = |m: usize, past: Option<usize>| loop {
            let mut i = draw(m).min(m - 1);
            if past.is_some_and(|first| i >= first) {
                i += 1;
            }
            if live(i) {
                return Some(i);
            }
            redraws += 1;
            if redraws > REDRAWS {
                return None;
            }
        };
        let pair = live_draw(n, None)
            .and_then(|first| live_draw(n - 1, Some(first)).map(|second| (first, second)));
        match pair {
            Some((first, second)) => Some(self.better(first, second, now, &in_flight)),
            None => self.pick_where(now, in_flight, draw, |a| !ejected(a)),
        }
    }

    /// Of two endpoints, the one with the lower `(in_flight + 1) / weight`, `first` on a tie.
    fn better(
        &self,
        first: usize,
        second: usize,
        now: Duration,
        in_flight: &impl Fn(usize) -> u32,
    ) -> usize {
        let score = |i: usize| (f64::from(in_flight(i)) + 1.0) / self.weight(i, now);
        if score(second) < score(first) {
            second
        } else {
            first
        }
    }

    /// How many endpoints `allowed` accepts. O(n).
    pub fn count_where(&self, allowed: impl Fn(SocketAddr) -> bool) -> usize {
        self.members.iter().filter(|m| allowed(m.addr)).count()
    }

    /// [`Endpoints::pick`] among the endpoints `allowed` accepts: the draws are over their
    /// number, the index returned is in the whole set. Nothing is allocated. O(n), so it is
    /// used for retries only; a first try uses [`Endpoints::pick`].
    pub fn pick_where(
        &self,
        now: Duration,
        in_flight: impl Fn(usize) -> u32,
        draw: &mut impl FnMut(usize) -> usize,
        allowed: impl Fn(SocketAddr) -> bool,
    ) -> Option<usize> {
        let n = self.count_where(&allowed);
        // The index in the whole set of the `k`th allowed endpoint.
        let nth = |k: usize| {
            self.members
                .iter()
                .enumerate()
                .filter(|(_, m)| allowed(m.addr))
                .nth(k)
                .map(|(i, _)| i)
        };
        match n {
            0 => return None,
            1 => return nth(0),
            _ => {}
        }
        let first = draw(n).min(n - 1);
        let mut second = draw(n - 1).min(n - 2);
        if second >= first {
            second += 1;
        }
        let (first, second) = (nth(first)?, nth(second)?);
        Some(self.better(first, second, now, &in_flight))
    }
}
