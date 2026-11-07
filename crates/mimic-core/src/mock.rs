//! Mock mode (spec §8): the pure part. The request signature, the lookup in the egress
//! snapshot, the tiers, the answer and the latency draw. No I/O, no clock: the sidecar owns
//! the sockets and the timers, and hands the draw its random numbers.

mod effective;
mod index;
mod latency;
mod signature;

#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::sync::Arc;

use crate::discovery::histogram::Histogram;

pub use effective::{DEFAULT_FACTOR, Effective, MockSettings, effective};
pub use index::Index;
pub use latency::{
    MAX_DELAY_MS, MAX_FACTOR, Random, draw, factor_number, format_factor, parse_factor,
    parse_factor_header, parse_mode_header, scale,
};
pub use signature::{Signature, signature};

/// A request as a mocked connection reads it. Bodies are already content-decoded and cut to
/// the capture limit of spec §8.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub method: &'a str,
    /// `Host`, or `:authority` for h2, as received.
    pub target: Option<&'a str>,
    /// An intercepted TLS connection (default port 443), else cleartext (80).
    pub tls: bool,
    pub sni: Option<&'a str>,
    /// The original destination.
    pub dst: SocketAddr,
    /// HTTP/2: a gRPC request needs it.
    pub h2: bool,
    /// Path without query, as received.
    pub path: &'a str,
    /// The raw query, without `?`.
    pub query: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    /// The request body when it is available for matching.
    pub body: Option<&'a [u8]>,
}

/// How an answer was chosen (spec §8 "Tiers").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Exact,
    Endpoint,
    Fallback,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Exact => "exact",
            Tier::Endpoint => "endpoint",
            Tier::Fallback => "fallback",
        }
    }
}

/// Why a request is not answered from the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Miss {
    Authority,
    Endpoint,
    Connect,
    Upgrade,
    Overload,
}

impl Miss {
    pub fn as_str(self) -> &'static str {
        match self {
            Miss::Authority => "authority",
            Miss::Endpoint => "endpoint",
            Miss::Connect => "connect",
            Miss::Upgrade => "upgrade",
            Miss::Overload => "overload",
        }
    }

    /// The `599` of spec §8 "Miss": the same for every reason, gRPC included.
    pub fn response(self) -> Response {
        Response {
            status: 599,
            headers: vec![
                ("x-mimic-miss".to_string(), "1".to_string()),
                ("content-length".to_string(), "0".to_string()),
            ],
            trailers: Vec::new(),
            body: None,
        }
    }
}

/// A sample's answer, serialised once: status, headers and body (spec §8 "Bounds").
#[derive(Debug, PartialEq, Eq)]
pub struct Answer {
    pub(crate) status: u16,
    /// The sample's headers in order, without the ones mimic sets itself.
    pub(crate) headers: Vec<(Box<str>, Box<str>)>,
    pub(crate) body: Box<[u8]>,
    pub(crate) grpc: bool,
}

impl Answer {
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The size of the answer in memory, for the bound of PUL-18.
    pub fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.body.len()
            + self
                .headers
                .iter()
                .map(|(n, v)| n.len() + v.len() + 2 * std::mem::size_of::<Box<str>>())
                .sum::<usize>()
    }
}

/// An answer chosen from the snapshot, with the histogram the delay is drawn from.
#[derive(Debug)]
pub struct Hit<'a> {
    pub tier: Tier,
    pub answer: Arc<Answer>,
    pub latency: &'a Histogram,
    /// The method and template of the endpoint the answer comes from.
    pub method: &'a str,
    pub template: &'a str,
}

/// What a mocked exchange sends: status, headers, trailers and a body shared with the index.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub trailers: Vec<(String, String)>,
    body: Option<Arc<Answer>>,
}

impl Response {
    pub fn body(&self) -> &[u8] {
        self.body.as_deref().map_or(&[], |a| &a.body)
    }
}

impl Hit<'_> {
    /// The response to a request of `method`: `HEAD`, `204` and `304` have no body.
    pub fn response(&self, method: &str) -> Response {
        index::respond(&self.answer, self.tier, method)
    }
}
