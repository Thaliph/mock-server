//! The shipping client (spec §7.2): freezes the local report into a pending report every
//! `MIMIC_DISCOVERY_PUSH_INTERVAL` and posts it to the sidecar API until it is settled.

use bytes::Bytes;
use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mimic_core::discovery::report::{Limits, Report};
use mimic_core::proxy::tls::Backoff;
use mimic_runtime::config::{ConfigError, Env};
use serde::Serialize;
use tokio::time::{Instant, sleep, sleep_until, timeout_at};

use super::Handoff;
use super::capture::Budget;
use super::metrics::DiscoveryMetrics;
use super::recorder;
use crate::tls::ca::{self, api_client, read_capped};

const INTERVAL_VAR: &str = "MIMIC_DISCOVERY_PUSH_INTERVAL";
const URL_VAR: &str = "MIMIC_CONTROLLER_URL";
const CA_VAR: &str = "MIMIC_CONTROLLER_CA";
const TOKEN_VAR: &str = "MIMIC_TLS_TOKEN_FILE";
const WORKLOAD_VAR: &str = "MIMIC_WORKLOAD";

/// Time allowed for one post.
const POST_TIMEOUT: Duration = Duration::from_secs(30);
/// A `Retry-After` longer than this is capped: a pending report must not wait for ever.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(300);
/// The pause between two posts of the shutdown flush, whatever the backoff or `Retry-After`.
const FLUSH_RETRY: Duration = Duration::from_millis(200);
/// How long the shutdown flush waits for the recorder to see the last exchanges.
const MAX_SETTLE: Duration = Duration::from_secs(2);

/// Where and how often the report is shipped.
#[derive(Debug, Clone)]
pub struct ShipConfig {
    pub interval: Duration,
    /// Time allowed for one post.
    pub timeout: Duration,
    /// `None` without `MIMIC_CONTROLLER_URL`: nothing is shipped.
    pub target: Option<Target>,
}

#[derive(Debug, Clone)]
pub struct Target {
    /// Sidecar API base URL, without trailing slash.
    pub url: String,
    pub ca: String,
    pub token_file: PathBuf,
    /// Deployment name (`MIMIC_WORKLOAD`).
    pub workload: String,
}

impl ShipConfig {
    pub fn from_env(env: &Env) -> Result<ShipConfig, ConfigError> {
        let interval = env.duration_or(INTERVAL_VAR, Duration::from_secs(30))?;
        if !(Duration::from_secs(1)..=Duration::from_secs(3600)).contains(&interval) {
            return Err(ConfigError::invalid(
                INTERVAL_VAR,
                env.optional(INTERVAL_VAR).unwrap_or_default(),
                "expected a duration from 1s to 1h",
            ));
        }
        Ok(ShipConfig {
            interval,
            timeout: POST_TIMEOUT,
            target: Target::from_env(env)?,
        })
    }
}

impl Target {
    /// The controller the sidecar talks to (`MIMIC_CONTROLLER_URL` and its companions), or
    /// `None` when the URL is unset.
    pub fn from_env(env: &Env) -> Result<Option<Target>, ConfigError> {
        env.optional(URL_VAR)
            .map(|url| target(env, url))
            .transpose()
    }
}

fn target(env: &Env, url: &str) -> Result<Target, ConfigError> {
    if !url.starts_with("https://") {
        return Err(ConfigError::invalid(
            URL_VAR,
            url,
            "expected an https:// URL",
        ));
    }
    let ca = env.required(CA_VAR)?;
    if ca::first_certificate(ca).is_none() {
        return Err(ConfigError::invalid(
            CA_VAR,
            "<pem>",
            "not a PEM certificate",
        ));
    }
    Ok(Target {
        url: url.trim_end_matches('/').to_string(),
        ca: ca.to_string(),
        token_file: PathBuf::from(env.optional(TOKEN_VAR).unwrap_or(crate::tls::DEFAULT_TOKEN)),
        workload: env.required(WORKLOAD_VAR)?.to_string(),
    })
}

/// What the shipper takes from its [`super::Discovery`].
pub(super) struct Parts {
    pub(super) report: Arc<Mutex<Report>>,
    /// The outbound exchanges (spec §6.1), frozen with the report.
    pub(super) egress: Arc<Mutex<Report>>,
    pub(super) limits: Limits,
    pub(super) pod: Option<String>,
    pub(super) handoff: Arc<Handoff>,
    pub(super) budget: Arc<Budget>,
    pub(super) metrics: DiscoveryMetrics,
}

/// A frozen report, posted unchanged until it is settled.
struct Pending {
    sequence: u64,
    report: Report,
    body: Bytes,
    exchanges: u64,
    /// The samples were removed after a `413`.
    stripped: bool,
    /// An attempt of this sequence had an unknown outcome (spec §7.2): a replica may have
    /// applied it. Set for the life of the sequence, whatever the later answers.
    unknown: bool,
}

#[derive(Serialize)]
struct Push<'a> {
    session: &'a str,
    sequence: u64,
    report: &'a Report,
}

/// What one attempt to post came to.
enum Attempt {
    /// Acknowledged, or dropped: the pending report is gone.
    Settled,
    /// Post the pending report again at once: it was not applied and its body changed.
    Again,
    /// Post again after this long (not at all when zero).
    Retry(Duration),
}

/// A `Retry-After` in seconds, capped at 300 s (spec §7.2); `None` for another form.
pub(crate) fn retry_after(value: &str) -> Option<Duration> {
    let secs = value.trim().parse::<u64>().ok()?;
    Some(Duration::from_secs(secs).min(MAX_RETRY_AFTER))
}

/// The answer of the controller, or why there is none.
enum Answer {
    Status {
        code: u16,
        retry_after: Option<Duration>,
        applied: bool,
    },
    /// No answer. `sent`: the body may have reached a replica, which may have applied it.
    Error { message: String, sent: bool },
}

/// Statuses that say nothing was applied: a controller answers them before it merges, or
/// refuses the push whole (spec §7.2). Any other answer leaves the outcome unknown.
fn applied_nothing(code: u16) -> bool {
    matches!(code, 400 | 401 | 403 | 405 | 413 | 429 | 503 | 507)
}

/// Ships the report of one [`super::Discovery`].
pub struct Shipper {
    http: reqwest::Client,
    target: Target,
    interval: Duration,
    parts: Parts,
    /// 128 random bits drawn at start (spec §7.2).
    session: String,
    sequence: u64,
    pending: Option<Pending>,
    backoff: Backoff,
    /// Failed attempts of the current streak, 0 outside one.
    failures: u32,
    /// The streak of fallbacks has written its `outbound not shipped` line; an acknowledged
    /// version 2 push ends it.
    fallback_logged: bool,
    /// Fallbacks so far.
    fallbacks: u64,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Shipper {
    pub(super) fn new(config: ShipConfig, parts: Parts) -> Result<Shipper, String> {
        let target = config.target.ok_or("no controller to ship to")?;
        let mut bytes = [0u8; 16];
        rand::fill(&mut bytes[..]);
        Ok(Shipper {
            http: api_client(&target.ca, config.timeout)?,
            target,
            interval: config.interval,
            parts,
            session: bytes.iter().map(|b| format!("{b:02x}")).collect(),
            sequence: 0,
            pending: None,
            backoff: Backoff::default(),
            failures: 0,
            fallback_logged: false,
            fallbacks: 0,
        })
    }

    /// Ships until `stop` completes. The pending report survives: [`Shipper::flush`] goes on
    /// with it.
    pub async fn run(&mut self, stop: impl Future<Output = ()>) {
        tokio::select! {
            () = stop => {}
            () = self.cycle() => {}
        }
    }

    async fn cycle(&mut self) {
        // The pods of a rollout must not push together.
        let first = rand::random_range(0..=self.interval.as_millis().max(1) as u64);
        sleep(Duration::from_millis(first)).await;
        loop {
            self.freeze(true);
            while self.pending.is_some() {
                match self.attempt().await {
                    Attempt::Settled | Attempt::Again => {}
                    Attempt::Retry(wait) => sleep(wait).await,
                }
            }
            sleep(self.interval).await;
        }
    }

    /// Posts what is left within `within`: the pending report, then the current one. What is
    /// not acknowledged then is lost.
    pub async fn flush(&mut self, within: Duration) {
        let deadline = Instant::now() + within;
        // The proxy has drained, but the recorder may still hold exchanges in its queue.
        let settle = deadline.min(Instant::now() + MAX_SETTLE);
        while !self.parts.budget.idle() && Instant::now() < settle {
            sleep(Duration::from_millis(10)).await;
        }
        // Version 2 is tried once more; after a fallback only the inbound part is frozen, so
        // the flush does not post the outbound part again and again.
        let mut outbound = true;
        loop {
            self.freeze(outbound);
            if self.pending.is_none() {
                if !outbound {
                    let held = self.held_exchanges();
                    if held > 0 {
                        tracing::warn!(target: "discovery", exchanges = held, "report not shipped");
                    }
                }
                return;
            }
            let fallbacks = self.fallbacks;
            let attempt = timeout_at(deadline, self.attempt()).await;
            if self.fallbacks != fallbacks {
                outbound = false;
            }
            match attempt {
                Err(_) => break,
                Ok(Attempt::Settled | Attempt::Again) => {}
                Ok(Attempt::Retry(_)) => {
                    // The kubelet may stop the process well before the deadline: no backoff and no
                    // `Retry-After`, a fixed pause (never shorter, so a `Retry-After: 0` cannot spin).
                    let wait = FLUSH_RETRY;
                    if Instant::now() + wait >= deadline {
                        break;
                    }
                    sleep_until(Instant::now() + wait).await;
                }
            }
        }
        tracing::warn!(
            target: "discovery",
            exchanges = self.held_exchanges(),
            "report not shipped"
        );
    }

    /// Exchanges of the pending, the current and the egress report.
    fn held_exchanges(&self) -> u64 {
        let current = self
            .parts
            .report
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .exchanges();
        let egress = self
            .parts
            .egress
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .exchanges();
        let pending = self.pending.as_ref().map_or(0, |p| p.exchanges);
        pending.saturating_add(current).saturating_add(egress)
    }

    /// Freezes the current report, with the egress report when `outbound`, into the pending
    /// one, when nothing is pending and an exchange (or, from F8, an outbound tunnel) was
    /// recorded.
    fn freeze(&mut self, outbound: bool) {
        if self.pending.is_some() {
            return;
        }
        let frozen = {
            let mut report = self
                .parts
                .report
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            // Never held together by the recorder, which takes them one after the other.
            let mut egress = self
                .parts
                .egress
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let outbound = outbound && egress.authorities() > 0;
            if report.exchanges() == 0 && !outbound {
                return;
            }
            let mut frozen = std::mem::replace(&mut *report, Report::new(self.parts.pod.clone()));
            let handoff = &self.parts.handoff;
            // Under the locks: the recorder reads them there.
            let mut outbound_samples = 0;
            if outbound {
                let part = std::mem::replace(&mut *egress, Report::new(self.parts.pod.clone()));
                outbound_samples = part.sample_bytes();
                frozen.merge(&self.parts.limits, part);
                handoff.egress_samples.store(0, Ordering::Release);
                handoff
                    .egress_keys
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clear();
            }
            handoff
                .pending_samples
                .store(frozen.sample_bytes(), Ordering::Release);
            handoff
                .pending_egress_samples
                .store(outbound_samples, Ordering::Release);
            *handoff
                .pending_keys
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = recorder::keys(&frozen);
            handoff.epoch.fetch_add(1, Ordering::AcqRel);
            recorder::publish(&self.parts.metrics, handoff, &report, &BTreeSet::new());
            frozen
        };
        self.sequence += 1;
        self.pending = Some(Pending {
            sequence: self.sequence,
            exchanges: frozen.exchanges(),
            body: Bytes::new(),
            report: frozen,
            stripped: false,
            unknown: false,
        });
        self.encode();
    }

    /// Serialises the pending report. One that cannot be serialised is dropped.
    fn encode(&mut self) {
        let Some(pending) = &mut self.pending else {
            return;
        };
        let push = Push {
            session: &self.session,
            sequence: pending.sequence,
            report: &pending.report,
        };
        match serde_json::to_vec(&push) {
            Ok(body) => pending.body = Bytes::from(body),
            Err(e) => {
                tracing::warn!(
                    target: "discovery",
                    sequence = pending.sequence,
                    exchanges = pending.exchanges,
                    error = %e,
                    "report not shipped"
                );
                self.settle();
            }
        }
    }

    /// The pending report is gone: acknowledged, rejected or lost.
    fn settle(&mut self) {
        self.pending = None;
        self.release(true);
    }

    /// The pending report no longer holds its samples (and its endpoints when `gone`): the gauges
    /// and the budget of the current report follow.
    fn release(&self, gone: bool) {
        let report = self
            .parts
            .report
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let handoff = &self.parts.handoff;
        handoff.pending_samples.store(0, Ordering::Release);
        handoff.pending_egress_samples.store(0, Ordering::Release);
        if gone {
            handoff
                .pending_keys
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clear();
        }
        recorder::publish(
            &self.parts.metrics,
            handoff,
            &report,
            &recorder::keys(&report),
        );
    }

    async fn attempt(&mut self) -> Attempt {
        let Some(pending) = &self.pending else {
            return Attempt::Settled;
        };
        let (sequence, exchanges, bytes) =
            (pending.sequence, pending.exchanges, pending.body.len());
        let answer = self.post(pending.body.clone()).await;
        match answer {
            Answer::Status {
                code: 200, applied, ..
            } => {
                self.parts
                    .metrics
                    .push(if applied { "ok" } else { "duplicate" });
                self.parts.metrics.set_last_push(unix_now());
                tracing::debug!(
                    target: "discovery",
                    sequence,
                    exchanges,
                    bytes,
                    applied,
                    "report pushed"
                );
                self.acknowledged();
                Attempt::Settled
            }
            Answer::Status { code: 413, .. }
                if self.pending.as_ref().is_some_and(|p| !p.stripped) =>
            {
                // Not applied: the same sequence again, without the samples.
                self.parts.metrics.push("too_large");
                if let Some(pending) = &mut self.pending {
                    pending.report.drop_samples();
                    pending.stripped = true;
                }
                self.release(false);
                self.encode();
                Attempt::Again
            }
            Answer::Status {
                code: code @ (400 | 507),
                ..
            } if self
                .pending
                .as_ref()
                .is_some_and(|p| p.report.version() == 2) =>
            {
                self.fall_back(code)
            }
            Answer::Status {
                code: code @ (400 | 405 | 413 | 507),
                ..
            } => {
                self.parts.metrics.push("rejected");
                tracing::warn!(
                    target: "discovery",
                    status = code,
                    sequence,
                    exchanges,
                    "report rejected"
                );
                self.settle();
                // A dropped report ends the streak as an acknowledged one does.
                self.backoff.success();
                self.failures = 0;
                Attempt::Settled
            }
            Answer::Status {
                code, retry_after, ..
            } => {
                if !applied_nothing(code) {
                    self.unknown_outcome();
                }
                let wait = self.failed(Some(code), None);
                Attempt::Retry(wait.max(retry_after.unwrap_or_default()))
            }
            Answer::Error { message, sent } => {
                if sent {
                    self.unknown_outcome();
                }
                Attempt::Retry(self.failed(None, Some(message)))
            }
        }
    }

    /// An attempt of the pending sequence may have been applied (spec §7.2).
    fn unknown_outcome(&mut self) {
        if let Some(pending) = &mut self.pending {
            pending.unknown = true;
        }
    }

    /// Fallback to inbound only (spec §7.2): a `400` or `507` to a version 2 report. The
    /// outbound part goes back into the egress report, unless an attempt of the sequence had
    /// an unknown outcome: it is dropped then, so that it is never counted twice. The inbound
    /// part is posted at once, with the same sequence.
    fn fall_back(&mut self, status: u16) -> Attempt {
        let Some(pending) = &mut self.pending else {
            return Attempt::Settled;
        };
        let sequence = pending.sequence;
        let unknown = pending.unknown;
        let (inbound, outbound) = pending.report.split();
        let outbound = outbound.unwrap_or_else(|| Report::new(None));
        let lost = outbound.exchanges();
        self.fallbacks += 1;
        self.parts.metrics.push("inbound_only");
        // A controller answered: the streak of failures is over.
        self.backoff.success();
        self.failures = 0;
        let posted = inbound.exchanges() > 0;
        if posted {
            pending.exchanges = inbound.exchanges();
            pending.report = inbound;
            self.hold_pending();
            self.encode();
        } else {
            // Nothing to post. The next freeze uses the sequence again, unless a replica may
            // have applied it: it would answer `applied: false` to other data.
            self.settle();
            if !unknown {
                // The freeze that made this sequence incremented the counter, and only one
                // sequence is pending: the counter equals it and is at least 1.
                debug_assert!(sequence >= 1 && self.sequence == sequence);
                self.sequence -= 1;
            }
        }
        if unknown {
            self.parts.metrics.outbound_dropped();
            // The sequence's line also stands for the streak's: one line per event.
            self.fallback_logged = true;
            tracing::warn!(
                target: "discovery",
                status,
                sequence,
                exchanges = lost,
                reason = "unknown_outcome",
                "outbound not shipped"
            );
        } else {
            self.restore(outbound);
            if !self.fallback_logged {
                self.fallback_logged = true;
                tracing::warn!(
                    target: "discovery",
                    status,
                    sequence,
                    "outbound not shipped"
                );
            }
        }
        if posted {
            Attempt::Again
        } else {
            Attempt::Settled
        }
    }

    /// Moves an outbound part back into the egress report, within the sidecar bounds: a
    /// later push ships it.
    fn restore(&self, outbound: Report) {
        let report = self
            .parts
            .report
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut egress = self
            .parts
            .egress
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let handoff = &self.parts.handoff;
        // As the recorder: half of the budget for outbound samples, and what the inbound and
        // pending samples leave.
        let budget = self.parts.limits.sample_budget;
        let inbound = report
            .sample_bytes()
            .saturating_add(handoff.pending_samples.load(Ordering::Acquire));
        let cap = (budget / 2)
            .saturating_sub(handoff.pending_egress_samples.load(Ordering::Acquire))
            .min(budget.saturating_sub(inbound));
        let limits = Limits {
            sample_budget: cap.saturating_mul(2),
            ..self.parts.limits
        };
        egress.merge(&limits, outbound);
        handoff
            .egress_samples
            .store(egress.sample_bytes(), Ordering::Release);
        *handoff
            .egress_keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = recorder::keys(&egress);
        recorder::publish(
            &self.parts.metrics,
            handoff,
            &report,
            &recorder::keys(&report),
        );
    }

    /// The pending report was reduced to its inbound part: the gauges and the budget follow.
    fn hold_pending(&self) {
        let Some(pending) = &self.pending else {
            return;
        };
        let report = self
            .parts
            .report
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let handoff = &self.parts.handoff;
        handoff
            .pending_samples
            .store(pending.report.sample_bytes(), Ordering::Release);
        handoff.pending_egress_samples.store(0, Ordering::Release);
        *handoff
            .pending_keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = recorder::keys(&pending.report);
        recorder::publish(
            &self.parts.metrics,
            handoff,
            &report,
            &recorder::keys(&report),
        );
    }

    fn acknowledged(&mut self) {
        // An acknowledged version 2 push ends the streak of fallbacks.
        if self
            .pending
            .as_ref()
            .is_some_and(|p| p.report.version() == 2)
        {
            self.fallback_logged = false;
        }
        self.settle();
        self.backoff.success();
        if self.failures > 0 {
            self.failures = 0;
            tracing::info!(target: "discovery", "report push recovered");
        }
    }

    /// Counts a failed attempt; how long to wait before the next.
    fn failed(&mut self, status: Option<u16>, error: Option<String>) -> Duration {
        self.parts.metrics.push("error");
        self.failures += 1;
        let wait = self.backoff.failure();
        if self.failures == 1 {
            tracing::warn!(
                target: "discovery",
                status,
                error,
                attempt = self.failures,
                retry_in_seconds = wait.as_secs(),
                "report push failed"
            );
        }
        wait
    }

    /// One post of `body`. The token is read again for each: the kubelet rotates it.
    async fn post(&self, body: Bytes) -> Answer {
        let token = match tokio::fs::read_to_string(&self.target.token_file).await {
            Ok(token) => token,
            Err(e) => {
                return Answer::Error {
                    message: format!("token file: {}", e.kind()),
                    sent: false,
                };
            }
        };
        let url = format!(
            "{}/v1/workloads/{}/report",
            self.target.url, self.target.workload
        );
        let response = self
            .http
            .post(url)
            .bearer_auth(token.trim())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            // Without the URL: it is the configured one, and nothing else is useful here.
            // The body reached no replica when the connection was not made.
            Err(e) => {
                return Answer::Error {
                    sent: !e.is_connect(),
                    message: e.without_url().to_string(),
                };
            }
        };
        let code = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(retry_after);
        let answer = read_capped(response).await;
        let applied = if code == 200 {
            // An acknowledgement says whether it applied the report: nothing else settles it.
            let bytes = match answer {
                Ok(bytes) => bytes,
                Err(message) => {
                    return Answer::Error {
                        message,
                        sent: true,
                    };
                }
            };
            match serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| v["applied"].as_bool())
            {
                Some(applied) => applied,
                None => {
                    return Answer::Error {
                        message: "unreadable answer".to_string(),
                        sent: true,
                    };
                }
            }
        } else {
            false
        };
        Answer::Status {
            code,
            retry_after,
            applied,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctl_16_retry_after_is_read_in_seconds_and_capped_at_300() {
        assert_eq!(retry_after(" 3 "), Some(Duration::from_secs(3)));
        assert_eq!(retry_after("0"), Some(Duration::ZERO));
        assert_eq!(retry_after("300"), Some(Duration::from_secs(300)));
        assert_eq!(retry_after("86400"), Some(Duration::from_secs(300)));
        assert_eq!(retry_after("soon"), None);
        assert_eq!(retry_after("-1"), None);
    }
}
