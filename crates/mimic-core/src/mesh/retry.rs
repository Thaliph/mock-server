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
        }
    }
}

/// A trusted refusal of a pod's own sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Overloaded,
    QueueTimeout,
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
/// to `pod`. Field names are compared without case; `pod` is `None` on the original destination.
pub fn trusted(status: u16, fields: &[(&[u8], &[u8])], pod: Option<SocketAddr>) -> Option<Refusal> {
    let pod = pod?;
    if status != 503 {
        return None;
    }
    let mut error = None;
    let mut endpoint = None;
    for (name, value) in fields {
        let slot = if name.eq_ignore_ascii_case(b"x-mimic-error") {
            &mut error
        } else if name.eq_ignore_ascii_case(b"x-mimic-endpoint") {
            &mut endpoint
        } else {
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
        _ => None,
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
        let untried = endpoints.count_where(|a| !self.tried.contains(&a));
        let last = self.tries == self.retries;
        if self.tries > 0 && self.only_connect && (untried == 0 || last) {
            self.original = true;
            self.tries += 1;
            return Next::Original;
        }
        let picked = if untried > 0 {
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
