//! Client-side load balancing over the endpoints of one Service port (spec §7.9, design §3.2):
//! power of two choices on the least request, weighted by slow start. Pure: the clock and the
//! random draws are passed in.

use std::net::SocketAddr;
use std::time::Duration;

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
        let _ = window;
        unimplemented!()
    }

    pub fn set_window(&mut self, window: Duration) {
        let _ = window;
        unimplemented!()
    }

    /// Replaces the endpoints by `addrs` at `now`.
    pub fn update(&mut self, addrs: &[SocketAddr], now: Duration) {
        let _ = (addrs, now);
        unimplemented!()
    }

    pub fn len(&self) -> usize {
        unimplemented!()
    }

    pub fn is_empty(&self) -> bool {
        unimplemented!()
    }

    pub fn addr(&self, index: usize) -> Option<SocketAddr> {
        let _ = index;
        unimplemented!()
    }

    pub fn weight(&self, index: usize, now: Duration) -> f64 {
        let _ = (index, now);
        unimplemented!()
    }

    /// The index of the endpoint to send a request to.
    pub fn pick(
        &self,
        now: Duration,
        in_flight: impl Fn(usize) -> u32,
        draw: &mut impl FnMut(usize) -> usize,
    ) -> Option<usize> {
        let _ = (now, in_flight, draw);
        unimplemented!()
    }
}
