//! Retries of the mesh (spec §7.9 "Retries", design §7): which failures are retried, when the
//! refusal of a pod can be trusted, and where the next try goes. Pure: the random draws are
//! passed in, there is no clock.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::time::Duration;

use super::lb::Endpoints;

/// What one try came to; the `result` label of `mimic_mesh_attempts_total` and, for a retry,
/// the `reason` label of `mimic_mesh_retries_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A response head came back (and it is not a trusted refusal).
    Ok,
    ConnectFailed,
    RefusedStream,
    Reset,
    Overloaded,
    QueueTimeout,
    /// The callee's sidecar answered that its app refused the connection (#518).
    AppRefused,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::ConnectFailed => "connect_failed",
            Outcome::RefusedStream => "refused_stream",
            Outcome::Reset => "reset",
            Outcome::Overloaded => "overloaded",
            Outcome::QueueTimeout => "queue_timeout",
            Outcome::AppRefused => "app_refused",
        }
    }
}

/// A trusted refusal of a pod's own sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Overloaded,
    QueueTimeout,
    /// `connect-failed` written by the inbound sidecar of the pod (`x-mimic-origin`).
    AppRefused,
}

/// A failure of one try, as the sidecar observed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Connect refused or timed out, failed handshake or verification: nothing was written.
    Connect,
    /// `RST_STREAM(REFUSED_STREAM)`, or a `GOAWAY` below the stream.
    RefusedStream,
    /// A response that [`trusted`] accepted.
    Refusal(Refusal),
    /// The connection closed or the stream reset (another code) before any response byte.
    Reset,
    /// A response that is not a trusted refusal.
    Response,
    /// A failure after the response head.
    AfterHead,
}

/// Why a failure of a retried kind is not retried (`mimic_mesh_retries_skipped_total`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// A reset of a method that is not idempotent.
    Method,
    /// Something was written and the body is not held for replay.
    Buffer,
}

impl Skip {
    pub fn label(self) -> &'static str {
        match self {
            Skip::Method => "method",
            Skip::Buffer => "buffer",
        }
    }
}

/// The verdict of [`classify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub outcome: Outcome,
    pub retry: bool,
    /// Set when the failure is of a retried kind and was not retried for a reason of the
    /// request (the planner adds `exhausted`).
    pub skip: Option<Skip>,
}

/// Classifies a failure (spec §7.9 table). `body_held`: every body byte read from the app is
/// still held for replay; `written`: a byte of the request went on the wire.
pub fn classify(failure: Failure, method: &str, body_held: bool, written: bool) -> Verdict {
    let outcome = match failure {
        Failure::Connect => {
            return Verdict {
                outcome: Outcome::ConnectFailed,
                retry: true,
                skip: None,
            };
        }
        Failure::Response | Failure::AfterHead => {
            return Verdict {
                outcome: Outcome::Ok,
                retry: false,
                skip: None,
            };
        }
        Failure::RefusedStream => Outcome::RefusedStream,
        Failure::Refusal(Refusal::Overloaded) => Outcome::Overloaded,
        Failure::Refusal(Refusal::QueueTimeout) => Outcome::QueueTimeout,
        Failure::Refusal(Refusal::AppRefused) => Outcome::AppRefused,
        Failure::Reset => Outcome::Reset,
    };
    let skip = if failure == Failure::Reset && written && !IDEMPOTENT.contains(&method) {
        Some(Skip::Method)
    } else if written && !body_held {
        Some(Skip::Buffer)
    } else {
        None
    };
    Verdict {
        outcome,
        retry: skip.is_none(),
        skip,
    }
}

/// The methods whose request a server may see twice (RFC 9110 §9.2.2, compared exactly).
const IDEMPOTENT: [&str; 6] = ["GET", "HEAD", "OPTIONS", "TRACE", "PUT", "DELETE"];

/// Whether an answer is a trusted refusal of the pod of this try: `503`, exactly one
/// `x-mimic-error` (`overloaded` or `queue-timeout`) and exactly one `x-mimic-endpoint` equal
/// to `pod`; `connect-failed` also needs exactly one valid `x-mimic-origin` (see [`origin`]).
/// Field names are compared without case; `pod` is `None` on the original destination.
pub fn trusted(status: u16, fields: &[(&[u8], &[u8])], pod: Option<SocketAddr>) -> Option<Refusal> {
    let pod = pod?;
    if status != 503 {
        return None;
    }
    let mut error = None;
    let mut endpoint = None;
    let mut origins = Vec::new();
    for (name, value) in fields {
        let slot = if name.eq_ignore_ascii_case(b"x-mimic-error") {
            &mut error
        } else if name.eq_ignore_ascii_case(b"x-mimic-endpoint") {
            &mut endpoint
        } else {
            if name.eq_ignore_ascii_case(b"x-mimic-origin") {
                origins.push(*value);
            }
            continue;
        };
        if slot.replace(*value).is_some() {
            return None;
        }
    }
    let bound = std::str::from_utf8(endpoint?)
        .ok()?
        .parse::<SocketAddr>()
        .ok()?;
    if bound != pod {
        return None;
    }
    match error? {
        b"overloaded" => Some(Refusal::Overloaded),
        b"queue-timeout" => Some(Refusal::QueueTimeout),
        b"connect-failed" => match origins[..] {
            [one] if origin(one) => Some(Refusal::AppRefused),
            _ => None,
        },
        _ => None,
    }
}

/// Whether `value` is an `x-mimic-origin` of an inbound sidecar (spec §4.1 "Refused app
/// connections"): `inbound`, or `inbound; pod=<name>` with a name of 1 to 253 characters
/// `a-z 0-9 . -`.
pub fn origin(value: &[u8]) -> bool {
    match value.strip_prefix(b"inbound") {
        Some(b"") => true,
        Some(rest) => rest.strip_prefix(b"; pod=").is_some_and(|name| {
            (1..=253).contains(&name.len())
                && name.iter().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-')
                })
        }),
        None => false,
    }
}

/// The delay before the next try: none after a connect failure, a refused stream or a reset,
/// 25 ms ± 50 % after a refusal; `unit` is a draw in `[0, 1]`.
pub fn delay(outcome: Outcome, unit: f64) -> Duration {
    match outcome {
        Outcome::Overloaded | Outcome::QueueTimeout => {
            let unit = if unit.is_nan() {
                0.5
            } else {
                unit.clamp(0.0, 1.0)
            };
            Duration::from_secs_f64(0.0125 + 0.025 * unit)
        }
        _ => Duration::ZERO,
    }
}

/// Where the next try goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    Pod(SocketAddr),
    /// The original destination (the ClusterIP); it ends the tries.
    Original,
    Stop,
}

/// The tries of one request.
#[derive(Debug, Clone)]
pub struct Planner {
    retries: u32,
    tries: u32,
    tried: BTreeSet<SocketAddr>,
    only_connect: bool,
    original: bool,
}

impl Planner {
    pub fn new(retries: u32) -> Planner {
        Planner {
            retries,
            tries: 0,
            tried: BTreeSet::new(),
            only_connect: true,
            original: false,
        }
    }

    pub fn tries(&self) -> u32 {
        self.tries
    }

    /// Whether a further try is allowed: the planner is neither out of tries nor past the
    /// original destination.
    pub fn has_try(&self) -> bool {
        unimplemented!()
    }

    /// The next try over the current `endpoints`: a pod not tried yet (P2C among them), else
    /// any pod. When every failure so far was a connect failure and either no untried pod is
    /// left or this is the last try, the original destination, once, which ends the tries.
    /// With no endpoint at the first try: [`Next::Stop`] (the caller answers `no-endpoint`).
    pub fn next(
        &mut self,
        endpoints: &Endpoints,
        now: Duration,
        in_flight: impl Fn(usize) -> u32,
        draw: &mut impl FnMut(usize) -> usize,
    ) -> Next {
        if self.original || self.tries > self.retries {
            return Next::Stop;
        }
        // First try: nothing tried yet, so the O(1) pick over the whole set (MSH-46).
        let untried = if self.tried.is_empty() {
            endpoints.len()
        } else {
            endpoints.count_where(|a| !self.tried.contains(&a))
        };
        let last = self.tries == self.retries;
        if self.tries > 0 && self.only_connect && (untried == 0 || last) {
            self.original = true;
            self.tries += 1;
            return Next::Original;
        }
        let picked = if self.tried.is_empty() {
            endpoints.pick(now, &in_flight, draw)
        } else if untried > 0 {
            endpoints.pick_where(now, &in_flight, draw, |a| !self.tried.contains(&a))
        } else {
            endpoints.pick(now, &in_flight, draw)
        };
        match picked.and_then(|i| endpoints.addr(i)) {
            Some(pod) => {
                self.tried.insert(pod);
                self.tries += 1;
                Next::Pod(pod)
            }
            None => Next::Stop,
        }
    }

    /// The outcome of the try [`Planner::next`] returned.
    pub fn failed(&mut self, outcome: Outcome) {
        self.only_connect &= outcome == Outcome::ConnectFailed;
    }
}

/// The slots of a [`Budget`]'s ring.
pub const BUDGET_SLOTS: usize = 10;

/// The retry budget of one Service (spec §7.9 "Retry budget").
#[derive(Debug, Clone)]
pub struct Budget {}

impl Budget {
    pub fn new(_percent: u32, _min_per_second: u32, _window: Duration) -> Budget {
        unimplemented!()
    }

    pub fn deposit(&mut self, _now: Duration) {
        unimplemented!()
    }

    pub fn withdraw(&mut self, _now: Duration) -> bool {
        unimplemented!()
    }

    /// `n` requests at once (tests: saturation without 2^32 calls).
    #[cfg(test)]
    pub(super) fn deposit_many_for_test(&mut self, _now: Duration, _n: u32) {
        unimplemented!()
    }

    /// The requests and retries held at `now`.
    pub fn counts(&mut self, _now: Duration) -> (u64, u64) {
        unimplemented!()
    }
}

/// What to do with a failed try (spec §7.9 "Tries", "Retry budget").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Not a failure of a retried kind: the result stands.
    Done,
    /// Of a retried kind, not retried for a reason of the request.
    Skipped(Skip),
    /// Of a retried kind, no try left.
    Exhausted,
    /// Of a retried kind, a try left, the budget refused: the tries end, the failure stands.
    Budget,
    /// The next try may go.
    Retry,
}

/// Decides after a failed try; the budget is asked (and a grant counted) only for a failure
/// that `verdict` retries and for which `planner` has a try left.
pub fn decide(
    _verdict: Verdict,
    _planner: &Planner,
    _budget: &mut Budget,
    _now: Duration,
) -> Decision {
    unimplemented!()
}
