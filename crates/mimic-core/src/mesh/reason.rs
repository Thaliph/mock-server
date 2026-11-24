//! The `x-mimic-error` reasons of spec §9: the one enumeration every writer of a reason takes,
//! so that the header, the access line's `reason` and the metric labels hold the same string.

use super::retry::Outcome;

/// A reason of the §9 `x-mimic-error` table. No `Ord`: the order of a serialised map of
/// reasons is chosen where it is written (§7.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reason {
    Overloaded,
    QueueTimeout,
    ConnectFailed,
    NoEndpoint,
    NoRoute,
    NoBackend,
    InvalidBackend,
    Timeout,
    BreakerOpen,
    FaultInjected,
    InvalidResponse,
    InvalidRequest,
    Unsupported,
}

impl Reason {
    /// The §9 table in its row order.
    pub const ALL: [Reason; 13] = [
        Reason::Overloaded,
        Reason::QueueTimeout,
        Reason::ConnectFailed,
        Reason::NoEndpoint,
        Reason::NoRoute,
        Reason::NoBackend,
        Reason::InvalidBackend,
        Reason::Timeout,
        Reason::BreakerOpen,
        Reason::FaultInjected,
        Reason::InvalidResponse,
        Reason::InvalidRequest,
        Reason::Unsupported,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Reason::Overloaded => "overloaded",
            Reason::QueueTimeout => "queue-timeout",
            Reason::ConnectFailed => "connect-failed",
            Reason::NoEndpoint => "no-endpoint",
            Reason::NoRoute => "no-route",
            Reason::NoBackend => "no-backend",
            Reason::InvalidBackend => "invalid-backend",
            Reason::Timeout => "timeout",
            Reason::BreakerOpen => "breaker-open",
            Reason::FaultInjected => "fault-injected",
            Reason::InvalidResponse => "invalid-response",
            Reason::InvalidRequest => "invalid-request",
            Reason::Unsupported => "unsupported",
        }
    }

    /// The status of the table; none for `fault-injected`, whose status is configured
    /// (`faults[].abort.status`).
    pub const fn status(self) -> Option<u16> {
        match self {
            Reason::Overloaded
            | Reason::QueueTimeout
            | Reason::ConnectFailed
            | Reason::NoEndpoint
            | Reason::BreakerOpen => Some(503),
            Reason::NoRoute => Some(404),
            Reason::NoBackend | Reason::InvalidBackend => Some(500),
            Reason::Timeout => Some(504),
            Reason::FaultInjected => None,
            Reason::InvalidResponse => Some(502),
            Reason::InvalidRequest => Some(400),
            Reason::Unsupported => Some(501),
        }
    }

    /// The reason whose name is exactly `value` (case-sensitive, no trim). The length is
    /// compared first, so a long untrusted value costs nothing.
    pub fn parse(value: &[u8]) -> Option<Reason> {
        if !(7..=16).contains(&value.len()) {
            return None;
        }
        Reason::ALL
            .into_iter()
            .find(|r| r.as_str().as_bytes() == value)
    }

    /// The refusals an inbound sidecar writes and a caller's sidecar trusts from the pod of
    /// its try, in the §7.8 `mimicErrors` key order.
    pub const fn trusted_inbound() -> [Reason; 3] {
        [
            Reason::ConnectFailed,
            Reason::Overloaded,
            Reason::QueueTimeout,
        ]
    }

    /// The reason of a received trusted refusal (§9 "Attempt outcomes"); none for the other
    /// outcomes of a try, the caller's own connect failure included.
    pub const fn of_outcome(outcome: Outcome) -> Option<Reason> {
        match outcome {
            Outcome::Overloaded => Some(Reason::Overloaded),
            Outcome::QueueTimeout => Some(Reason::QueueTimeout),
            Outcome::AppRefused => Some(Reason::ConnectFailed),
            Outcome::Ok
            | Outcome::ConnectFailed
            | Outcome::RefusedStream
            | Outcome::Reset
            | Outcome::Timeout => None,
        }
    }
}
