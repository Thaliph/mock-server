//! The shipping client (spec §7.2): freezes the local report into a pending report every
//! `MIMIC_DISCOVERY_PUSH_INTERVAL` and posts it to the sidecar API until it is settled.

use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mimic_core::discovery::report::Report;
use mimic_core::proxy::tls::Backoff;
use mimic_runtime::config::{ConfigError, Env};
use serde::Serialize;
use tokio::time::{Instant, sleep, sleep_until, timeout_at};

use super::Handoff;
use super::capture::Budget;
use super::metrics::DiscoveryMetrics;
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
        let target = match env.optional(URL_VAR) {
            None => None,
            Some(url) => Some(target(env, url)?),
        };
        Ok(ShipConfig {
            interval,
            timeout: POST_TIMEOUT,
            target,
        })
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
    pub(super) pod: Option<String>,
    pub(super) handoff: Arc<Handoff>,
    pub(super) budget: Arc<Budget>,
    pub(super) metrics: DiscoveryMetrics,
}

/// A frozen report, posted unchanged until it is settled.
struct Pending {
    sequence: u64,
    report: Report,
    body: Vec<u8>,
    exchanges: u64,
    /// The samples were removed after a `413`.
    stripped: bool,
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
    /// Post again after this long (not at all when zero).
    Retry(Duration),
}

/// The answer of the controller, or why there is none.
enum Answer {
    Status {
        code: u16,
        retry_after: Option<Duration>,
        applied: bool,
    },
    Error(String),
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
            self.freeze();
            while self.pending.is_some() {
                match self.attempt().await {
                    Attempt::Settled => {}
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
        loop {
            self.freeze();
            if self.pending.is_none() {
                return;
            }
            match timeout_at(deadline, self.attempt()).await {
                Err(_) => break,
                Ok(Attempt::Settled) => {}
                Ok(Attempt::Retry(wait)) => {
                    if Instant::now() + wait >= deadline {
                        break;
                    }
                    sleep_until(Instant::now() + wait).await;
                }
            }
        }
        let current = self
            .parts
            .report
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .exchanges();
        let pending = self.pending.as_ref().map_or(0, |p| p.exchanges);
        tracing::warn!(
            target: "discovery",
            exchanges = pending.saturating_add(current),
            "report not shipped"
        );
    }

    /// Freezes the current report into the pending one, when nothing is pending and an
    /// exchange was recorded.
    fn freeze(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let frozen = {
            let mut report = self
                .parts
                .report
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if report.exchanges() == 0 {
                return;
            }
            let frozen = std::mem::replace(&mut *report, Report::new(self.parts.pod.clone()));
            // Under the lock: the recorder reads them there.
            self.parts
                .handoff
                .pending_samples
                .store(frozen.sample_bytes(), Ordering::Release);
            self.parts.handoff.epoch.fetch_add(1, Ordering::AcqRel);
            frozen
        };
        self.sequence += 1;
        self.pending = Some(Pending {
            sequence: self.sequence,
            exchanges: frozen.exchanges(),
            body: Vec::new(),
            report: frozen,
            stripped: false,
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
            Ok(body) => pending.body = body,
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
        self.parts
            .handoff
            .pending_samples
            .store(0, Ordering::Release);
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
                self.parts
                    .handoff
                    .pending_samples
                    .store(0, Ordering::Release);
                self.encode();
                Attempt::Retry(Duration::ZERO)
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
                Attempt::Settled
            }
            Answer::Status {
                code, retry_after, ..
            } => {
                let wait = self.failed(Some(code), None);
                Attempt::Retry(wait.max(retry_after.unwrap_or_default()))
            }
            Answer::Error(error) => Attempt::Retry(self.failed(None, Some(error))),
        }
    }

    fn acknowledged(&mut self) {
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
    async fn post(&self, body: Vec<u8>) -> Answer {
        let token = match tokio::fs::read_to_string(&self.target.token_file).await {
            Ok(token) => token,
            Err(e) => return Answer::Error(format!("token file: {}", e.kind())),
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
            Err(e) => return Answer::Error(e.without_url().to_string()),
        };
        let code = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|secs| Duration::from_secs(secs).min(MAX_RETRY_AFTER));
        let applied = match read_capped(response).await {
            Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .is_some_and(|v| v["applied"] != false),
            Err(e) if code == 200 => return Answer::Error(e),
            Err(_) => false,
        };
        Answer::Status {
            code,
            retry_after,
            applied,
        }
    }
}
