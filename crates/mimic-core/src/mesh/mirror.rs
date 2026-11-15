//! Request mirrors of the mesh (spec §7.9 "Routing", "Mirrors"): which mirrors of a rule get a
//! copy, from the random values, the free slots and the body's state passed in, and the
//! deadline of a copy. Pure: there is no random source, no clock, no counter of slots here.

use std::collections::BTreeMap;

use super::route::TimeoutsView;

/// Copies in flight per mirror Service port (default of `mirror_max_in_flight`).
pub const MAX_IN_FLIGHT: usize = 256;
/// Copies in flight for the sidecar (default of `mirror_max_in_flight_total`).
pub const MAX_IN_FLIGHT_TOTAL: usize = 1024;
/// Deadline of a copy when the rule has no shorter timeout (default of `mirror_timeout`).
pub const TIMEOUT_MS: u64 = 10_000;

/// Whether the mirror `numerator / denominator` is drawn for the uniform 64-bit value `r`:
/// `⌊r × denominator / 2⁶⁴⌋ < numerator`.
pub fn drawn(numerator: u32, denominator: u32, r: u64) -> bool {
    ((u128::from(r) * u128::from(denominator)) >> 64) < u128::from(numerator)
}

/// The request body as known at the head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
    /// No body.
    None,
    /// A body of the declared `content-length` (not 0).
    Declared(u64),
    /// A body of unknown length (chunked, HTTP/2 without a length).
    Unknown,
}

/// A mirror of the rule: its mirror Service port (the key of its slots) and its draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate<'a> {
    pub port: &'a str,
    pub drawn: bool,
}

/// What a drawn mirror does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Copy now; the mirror holds a slot.
    Send,
    /// Copy once the body's end was read; the mirror holds a slot.
    AwaitBody,
    /// Skipped, the body is over the request's share; no slot is held.
    SkipBody,
    /// Skipped, no free slot; nothing is held.
    SkipLimit,
}

impl Decision {
    /// Whether the mirror holds a slot.
    pub fn holds_slot(self) -> bool {
        matches!(self, Decision::Send | Decision::AwaitBody)
    }
}

/// One decision per candidate, in order; `None` for a mirror not drawn. `free_port` is the number
/// of free slots of a mirror Service port, `free_total` those of the sidecar; `share` is the
/// bytes the request may hold. Two drawn mirrors of one port take two slots.
pub fn decide(
    candidates: &[Candidate<'_>],
    free_total: usize,
    free_port: impl Fn(&str) -> usize,
    body: Body,
    share: u64,
) -> Vec<Option<Decision>> {
    let mut total = 0usize;
    let mut per_port: BTreeMap<&str, usize> = BTreeMap::new();
    candidates
        .iter()
        .map(|c| {
            if !c.drawn {
                return None;
            }
            let taken = per_port.entry(c.port).or_insert(0);
            if total >= free_total || *taken >= free_port(c.port) {
                return Some(Decision::SkipLimit);
            }
            let decision = match body {
                Body::None => Decision::Send,
                Body::Declared(len) if len > share => return Some(Decision::SkipBody),
                Body::Declared(_) | Body::Unknown => Decision::AwaitBody,
            };
            total += 1;
            *taken += 1;
            Some(decision)
        })
        .collect()
}

/// The deadline of a copy, in milliseconds from the request head: the earliest of
/// `mirror_timeout_ms` and the rule's non-zero timeouts.
pub fn deadline_ms(mirror_timeout_ms: u64, timeouts: Option<&TimeoutsView>) -> u64 {
    let rule = timeouts
        .into_iter()
        .flat_map(|t| [t.request_ms, t.backend_request_ms])
        .flatten()
        .filter(|ms| *ms > 0);
    rule.fold(mirror_timeout_ms, u64::min)
}
