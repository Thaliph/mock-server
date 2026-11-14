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
        todo!()
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
        todo!()
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
pub fn classify(_failure: Failure, _method: &str, _body_held: bool, _written: bool) -> Verdict {
    todo!()
}

/// Whether an answer is a trusted refusal of the pod of this try: `503`, exactly one
/// `x-mimic-error` (`overloaded` or `queue-timeout`) and exactly one `x-mimic-endpoint` equal
/// to `pod`. Field names are compared without case; `pod` is `None` on the original destination.
pub fn trusted(
    _status: u16,
    _fields: &[(&[u8], &[u8])],
    _pod: Option<SocketAddr>,
) -> Option<Refusal> {
    todo!()
}

/// The delay before the next try: none after a connect failure, a refused stream or a reset,
/// 25 ms ± 50 % after a refusal; `unit` is a draw in `[0, 1]`.
pub fn delay(_outcome: Outcome, _unit: f64) -> Duration {
    todo!()
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
    pub fn new(_retries: u32) -> Planner {
        todo!()
    }

    pub fn tries(&self) -> u32 {
        self.tries
    }

    /// The next try over the current `endpoints`.
    pub fn next(
        &mut self,
        _endpoints: &Endpoints,
        _now: Duration,
        _in_flight: impl Fn(usize) -> u32,
        _draw: &mut impl FnMut(usize) -> usize,
    ) -> Next {
        todo!()
    }

    /// The outcome of the try [`Planner::next`] returned.
    pub fn failed(&mut self, _outcome: Outcome) {
        todo!()
    }
}
