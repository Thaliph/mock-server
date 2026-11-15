//! Outlier detection of the mesh (spec §7.9 "Outlier detection", design §3.2, §6): per Service
//! port, the failures of each endpoint in a row, its ejection and its restoration. Pure: the
//! sidecar's monotonic clock is passed in; there is no I/O and no random source.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use super::policy::{MAX_BASE_EJECTION_SECS, OutlierDetection};

/// What one try to a pod came to, as far as outlier detection is concerned (the table of
/// spec §7.9 "Outcomes").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Connect refused or timed out, failed TLS handshake or verification.
    ConnectFailed,
    /// A trusted `connect-failed` of the pod's own inbound sidecar (#518).
    AppRefused,
    /// Closed or reset after the request was written and before any byte of a response head.
    Reset,
    /// A response head `hyper` rejects.
    InvalidResponse,
    /// A final response head of the pod that is not a trusted refusal: a `5xx` counts as such,
    /// a `1xx` is not an outcome, any other status ends the failures in a row.
    Head(u16),
    /// An L4 connect that succeeded.
    Connected,
    /// A trusted refusal `overloaded`: a busy pod is not a failing one.
    Overloaded,
    /// A trusted refusal `queue-timeout`.
    QueueTimeout,
    RefusedStream,
    /// A route timeout: the author's bound.
    Timeout,
    /// The app left before the response head.
    AppLeft,
    /// A mirror copy's deadline.
    CopyDeadline,
    /// Anything after the response head.
    AfterHead,
}

/// Which counter ejected an endpoint (the `reason` label).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    ConsecutiveFailures,
    Consecutive5xx,
}

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Reason::ConsecutiveFailures => "consecutive_failures",
            Reason::Consecutive5xx => "consecutive_5xx",
        }
    }
}

/// An ejection that [`Outlier::record`] made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ejection {
    pub reason: Reason,
    /// How long the endpoint is ejected.
    pub seconds: u32,
    /// The ejections of its streak, this one included.
    pub ejections: u32,
}

/// Why an endpoint was restored (the `cause` of the log line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// Its ejection time ended.
    Elapsed,
    /// A view or a policy left more endpoints ejected than `maxEjectionPercent` allows.
    MaxPercent,
}

impl Cause {
    pub fn label(self) -> &'static str {
        match self {
            Cause::Elapsed => "elapsed",
            Cause::MaxPercent => "max_percent",
        }
    }
}

/// An endpoint that went back to the candidates, reported once by [`Outlier::reconcile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Restoration {
    pub addr: SocketAddr,
    pub cause: Cause,
}

/// The streak ends once an endpoint has been restored for this long without a new ejection.
const STREAK_END: Duration = Duration::from_secs(MAX_BASE_EJECTION_SECS as u64);

/// Fixed-size state of one endpoint.
#[derive(Debug, Clone, Default)]
struct State {
    failures: u32,
    fives: u32,
    /// The end of the ejection while it lasts, or that ended and was not settled yet.
    ejected_until: Option<Duration>,
    /// Ejections of the current streak.
    streak: u32,
    /// When the last ejection ended (by its time or at once).
    restored_at: Option<Duration>,
    /// A restoration that [`Outlier::reconcile`] has not reported yet.
    unreported: Option<Cause>,
}

impl State {
    fn ejected(&self, now: Duration) -> bool {
        self.ejected_until.is_some_and(|until| until > now)
    }

    /// An ejection whose time ended becomes a restoration, to report.
    fn settle(&mut self, now: Duration) {
        if let Some(until) = self.ejected_until
            && until <= now
        {
            self.ejected_until = None;
            self.restored_at = Some(until);
            self.unreported = Some(Cause::Elapsed);
        }
    }
}

/// The endpoints of one Service port under one `outlierDetection` policy.
#[derive(Debug, Clone)]
pub struct Outlier {
    consecutive_failures: u32,
    consecutive_5xx: u32,
    base_ejection_seconds: u32,
    max_ejection_percent: u32,
    endpoints: BTreeMap<SocketAddr, State>,
}

impl Outlier {
    /// No endpoint yet; [`Outlier::reconcile`] gives the first view.
    pub fn new(policy: &OutlierDetection) -> Outlier {
        Outlier {
            consecutive_failures: policy.consecutive_failures,
            consecutive_5xx: policy.consecutive_5xx,
            base_ejection_seconds: policy.base_ejection_seconds,
            max_ejection_percent: policy.max_ejection_percent,
            endpoints: BTreeMap::new(),
        }
    }

    /// `⌊n × maxEjectionPercent / 100⌋`, `n` the endpoints of the view.
    fn allowed(&self) -> usize {
        let n = self.endpoints.len() as u64;
        usize::try_from(n * u64::from(self.max_ejection_percent) / 100).unwrap_or(usize::MAX)
    }

    /// Whether `addr` is ejected at `now` (left out of every choice). The end time compared
    /// with the clock decides, so the answer is exact whatever the sweep did.
    pub fn is_ejected(&self, addr: SocketAddr, now: Duration) -> bool {
        self.endpoints.get(&addr).is_some_and(|s| s.ejected(now))
    }

    /// The endpoints ejected at `now`.
    pub fn ejected_count(&self, now: Duration) -> usize {
        self.endpoints.values().filter(|s| s.ejected(now)).count()
    }

    /// The `(failures, 5xx)` counters of `addr`.
    pub fn counters(&self, addr: SocketAddr) -> Option<(u32, u32)> {
        self.endpoints.get(&addr).map(|s| (s.failures, s.fives))
    }

    #[cfg(test)]
    pub(super) fn set_counters_for_test(&mut self, addr: SocketAddr, failures: u32, fives: u32) {
        if let Some(s) = self.endpoints.get_mut(&addr) {
            s.failures = failures;
            s.fives = fives;
        }
    }

    /// Applies the outcome of one try to `addr` at `now`; the ejection it causes, if any. An
    /// endpoint the view does not hold, and one that is ejected (a try that was in flight),
    /// count nothing.
    pub fn record(
        &mut self,
        addr: SocketAddr,
        outcome: Outcome,
        now: Duration,
    ) -> Option<Ejection> {
        let state = self.endpoints.get_mut(&addr)?;
        state.settle(now);
        if state.ejected(now) {
            return None;
        }
        match outcome {
            Outcome::ConnectFailed
            | Outcome::AppRefused
            | Outcome::Reset
            | Outcome::InvalidResponse => state.failures = state.failures.saturating_add(1),
            Outcome::Head(status) if (500..=599).contains(&status) => {
                state.failures = 0;
                state.fives = state.fives.saturating_add(1);
            }
            Outcome::Head(status) if status >= 200 => {
                state.failures = 0;
                state.fives = 0;
            }
            Outcome::Connected => {
                state.failures = 0;
                state.fives = 0;
            }
            Outcome::Head(_)
            | Outcome::Overloaded
            | Outcome::QueueTimeout
            | Outcome::RefusedStream
            | Outcome::Timeout
            | Outcome::AppLeft
            | Outcome::CopyDeadline
            | Outcome::AfterHead => return None,
        }
        let reason = if state.failures >= self.consecutive_failures {
            Reason::ConsecutiveFailures
        } else if self.consecutive_5xx != 0 && state.fives >= self.consecutive_5xx {
            Reason::Consecutive5xx
        } else {
            return None;
        };
        // The room is counted only for an endpoint at its threshold.
        if self.ejected_count(now) >= self.allowed() {
            return None;
        }
        let base = self.base_ejection_seconds;
        let state = self.endpoints.get_mut(&addr)?;
        if state
            .restored_at
            .is_some_and(|r| now >= r.saturating_add(STREAK_END))
        {
            state.streak = 0;
        }
        let seconds = ejection_seconds(base, state.streak);
        state.streak = state.streak.saturating_add(1);
        state.failures = 0;
        state.fives = 0;
        state.ejected_until = Some(now.saturating_add(Duration::from_secs(u64::from(seconds))));
        Some(Ejection {
            reason,
            seconds,
            ejections: state.streak,
        })
    }

    /// Follows a view: the policy and the endpoints of the port at `now`. An endpoint that
    /// left drops its state without a report, a new one is a candidate. The ejections that
    /// ended are settled, and when more endpoints are ejected than the policy allows the ones
    /// ending first (ties: the lower address) are restored at once. The restorations not yet
    /// reported are returned, in address order.
    pub fn reconcile(
        &mut self,
        policy: &OutlierDetection,
        addrs: &[SocketAddr],
        now: Duration,
    ) -> Vec<Restoration> {
        self.consecutive_failures = policy.consecutive_failures;
        self.consecutive_5xx = policy.consecutive_5xx;
        self.base_ejection_seconds = policy.base_ejection_seconds;
        self.max_ejection_percent = policy.max_ejection_percent;
        self.endpoints.retain(|a, _| addrs.contains(a));
        for addr in addrs {
            self.endpoints.entry(*addr).or_default();
        }
        for state in self.endpoints.values_mut() {
            state.settle(now);
        }
        let allowed = self.allowed();
        let mut ending: Vec<(Duration, SocketAddr)> = self
            .endpoints
            .iter()
            .filter(|(_, s)| s.ejected(now))
            .filter_map(|(a, s)| s.ejected_until.map(|until| (until, *a)))
            .collect();
        ending.sort_unstable();
        let excess = ending.len().saturating_sub(allowed);
        for (_, addr) in ending.into_iter().take(excess) {
            if let Some(state) = self.endpoints.get_mut(&addr) {
                state.ejected_until = None;
                state.restored_at = Some(now);
                state.unreported = Some(Cause::MaxPercent);
            }
        }
        self.endpoints
            .iter_mut()
            .filter_map(|(addr, s)| {
                s.unreported
                    .take()
                    .map(|cause| Restoration { addr: *addr, cause })
            })
            .collect()
    }
}

/// `base × 2^streak` seconds, at most 300; the arithmetic saturates.
fn ejection_seconds(base: u32, streak: u32) -> u32 {
    let factor = 1u64.checked_shl(streak).unwrap_or(u64::MAX);
    let secs = u64::from(base).saturating_mul(factor);
    u32::try_from(secs.min(u64::from(MAX_BASE_EJECTION_SECS))).unwrap_or(MAX_BASE_EJECTION_SECS)
}
