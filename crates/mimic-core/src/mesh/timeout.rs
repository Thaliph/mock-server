//! Route timeouts of the mesh (spec §7.9 "Timeouts"): the deadline of a call and of each try,
//! as millisecond offsets from the request's start. Pure: the times are passed in, there is no
//! clock, and every addition saturates (a deadline past `u64::MAX` is none).

use super::route::TimeoutsView;

/// Which timeout a deadline is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `timeouts.request`: the whole call.
    Request,
    /// `timeouts.backendRequest`: one try.
    BackendRequest,
}

impl Kind {
    /// The `timeout` field of the access line.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Request => "request",
            Kind::BackendRequest => "backendRequest",
        }
    }
}

/// A deadline: `at_ms` after the request's start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline {
    pub at_ms: u64,
    pub kind: Kind,
}

/// A timeout of 0 is none, as an absent one.
fn set(ms: Option<u64>) -> Option<u64> {
    ms.filter(|ms| *ms > 0)
}

/// The deadline of the whole call: `requestMs`.
pub fn call_deadline(timeouts: Option<&TimeoutsView>) -> Option<Deadline> {
    let at_ms = set(timeouts?.request_ms)?;
    Some(Deadline {
        at_ms,
        kind: Kind::Request,
    })
}

/// The deadline of a try started at `start_ms`: `min(start + backendRequestMs, requestMs)`,
/// the call's when it comes first or at the same time. A call deadline at or before the start
/// leaves a try that is already expired.
pub fn try_deadline(timeouts: Option<&TimeoutsView>, start_ms: u64) -> Option<Deadline> {
    let call = call_deadline(timeouts);
    let backend = timeouts
        .and_then(|t| set(t.backend_request_ms))
        .and_then(|ms| start_ms.checked_add(ms))
        .map(|at_ms| Deadline {
            at_ms,
            kind: Kind::BackendRequest,
        });
    match (backend, call) {
        (Some(b), Some(c)) if b.at_ms < c.at_ms => Some(b),
        (b, c) => c.or(b),
    }
}
