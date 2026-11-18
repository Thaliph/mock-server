//! The fault decision of a mesh-routed request (spec §7.9 "Fault injection"): which fault of the
//! policy applies and what it does. Pure: the request parts and the two random values come in
//! as arguments.

use super::policy::Fault;
use super::route::{RequestParts, match_holds};

/// What the sidecar does to the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Answers `status` at once, sending nothing to a pod.
    Abort { status: u16 },
    /// Waits `ms` milliseconds before the first try.
    Delay { ms: u64 },
}

/// The fault decided: its index in the policy's `faults` and what it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub index: usize,
    pub kind: Kind,
}

/// Whether `r`, a uniform 64-bit value, is drawn by `percent`: `⌊r × 100 / 2⁶⁴⌋ < percent`
/// (0 never, 100 always).
pub fn drawn(r: u64, percent: u32) -> bool {
    (u128::from(r) * 100) >> 64 < u128::from(percent)
}

/// The decision for a request: the first fault whose `match` holds (none: every request) is
/// the one; its abort is drawn with `r_abort` and its delay, independently, with `r_delay`. An
/// abort drawn wins over a delay drawn. `None` when no fault holds or nothing is drawn.
pub fn decide(
    faults: &[Fault],
    req: &RequestParts,
    r_abort: u64,
    r_delay: u64,
) -> Option<Decision> {
    let (index, fault) = faults
        .iter()
        .enumerate()
        .find(|(_, f)| f.r#match.as_ref().is_none_or(|m| match_holds(m, req)))?;
    let kind = if let Some(a) = fault.abort.as_ref().filter(|a| drawn(r_abort, a.percent)) {
        Kind::Abort { status: a.status }
    } else {
        let d = fault.delay.as_ref().filter(|d| drawn(r_delay, d.percent))?;
        Kind::Delay { ms: d.fixed_ms }
    };
    Some(Decision { index, kind })
}
