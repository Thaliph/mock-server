//! The fault decision of a mesh-routed request (spec §7.9 "Fault injection"): which fault of the
//! policy applies and what it does. Pure: the request parts and the two random values come in
//! as arguments.

use super::policy::{Fault, FaultAbort, FaultDelay};
use super::route::{Matches, RequestParts};

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

/// The faults of a policy made once, when the view is built: their matches deduplicated and
/// their header names looked up once per request (FLT-10). The sidecar keeps one per policy of
/// its view.
pub struct Faults {
    matches: Matches,
    effects: Vec<(Option<FaultAbort>, Option<FaultDelay>)>,
}

impl Faults {
    pub fn new(faults: &[Fault]) -> Self {
        Faults {
            matches: Matches::new(faults.iter().map(|f| f.r#match.as_ref())),
            effects: faults
                .iter()
                .map(|f| (f.abort.clone(), f.delay.clone()))
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.effects.is_empty()
    }

    /// The decision for a request: the first fault whose `match` holds (none: every request)
    /// is the one; its abort is drawn with `r_abort` and its delay, independently, with
    /// `r_delay`. An abort drawn wins over a delay drawn. `None` when no fault holds or nothing
    /// is drawn.
    pub fn decide(&self, req: &RequestParts, r_abort: u64, r_delay: u64) -> Option<Decision> {
        if self.is_empty() {
            return None;
        }
        let index = self.matches.first(req)?;
        let (abort, delay) = &self.effects[index];
        let kind = if let Some(a) = abort.as_ref().filter(|a| drawn(r_abort, a.percent)) {
            Kind::Abort { status: a.status }
        } else {
            let d = delay.as_ref().filter(|d| drawn(r_delay, d.percent))?;
            Kind::Delay { ms: d.fixed_ms }
        };
        Some(Decision { index, kind })
    }
}

/// [`Faults::decide`] over `faults` made for this one request (the tests' shorthand).
#[cfg(test)]
pub(crate) fn decide(
    faults: &[Fault],
    req: &RequestParts,
    r_abort: u64,
    r_delay: u64,
) -> Option<Decision> {
    Faults::new(faults).decide(req, r_abort, r_delay)
}
