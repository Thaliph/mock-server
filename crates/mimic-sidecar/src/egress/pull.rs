//! The pull loop (spec §7.7): one pull at a time, every `MIMIC_EGRESS_PULL_INTERVAL`.

use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mimic_core::discovery::egress::{EgressSet, Refusal};
use mimic_core::mock::Index;
use mimic_core::proxy::tls::Backoff;
use tokio::time::sleep;

use super::{EgressConfig, EgressMetrics, Snapshot};
use crate::discovery::{Target, retry_after};
use crate::mode::Announced;
use crate::tls::ca::api_client;

/// Why a pull did not give a set.
struct Failure {
    /// The `result` label.
    result: &'static str,
    status: Option<u16>,
    error: Option<String>,
    /// The `reason` of a refused body.
    reason: Option<&'static str>,
    retry_after: Option<Duration>,
}

impl Failure {
    fn error(status: Option<u16>, error: Option<String>, retry_after: Option<Duration>) -> Self {
        Failure {
            result: "error",
            status,
            error,
            reason: None,
            retry_after,
        }
    }

    fn refused(result: &'static str, status: Option<u16>, reason: Option<&'static str>) -> Self {
        Failure {
            result,
            status,
            error: None,
            reason,
            retry_after: None,
        }
    }
}

/// What a pull came to.
enum Fetched {
    Set {
        set: Index,
        authorities: usize,
        endpoints: usize,
        etag: Option<String>,
        bytes: usize,
    },
    NotModified,
    Failed(Failure),
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Pulls the egress set of one deployment into a [`Snapshot`].
pub struct Puller {
    http: reqwest::Client,
    target: Target,
    interval: Duration,
    max_bytes: usize,
    snapshot: Snapshot,
    metrics: EgressMetrics,
    backoff: Backoff,
    /// A failure streak is under way: its warning is written, a `200` or `304` ends it.
    failing: bool,
    /// The controller refused `format=2` (it is of F13 or older): the pulls since, which ask
    /// `format=1` until `PROBE_EVERY` of them have passed (spec §7.7, "Pull client").
    format_one_pulls: Option<u32>,
}

/// An egress set with the gRPC messages (F14), and the one that older controllers serve.
const FORMAT_WITH_GRPC: u8 = 2;
const FORMAT_WITHOUT_GRPC: u8 = 1;
/// After a fallback to format 1, `format=2` is asked again once every this many pulls.
const PROBE_EVERY: u32 = 10;

impl Puller {
    /// The pull client, or `None` (after one INFO line) without a target.
    pub fn new(
        config: EgressConfig,
        metrics: EgressMetrics,
        snapshot: Snapshot,
    ) -> Result<Option<Puller>, String> {
        let Some(target) = config.target else {
            tracing::info!(target: "egress", "egress pull disabled");
            return Ok(None);
        };
        Ok(Some(Puller {
            http: api_client(&target.ca, config.timeout)?,
            target,
            interval: config.interval,
            max_bytes: config.max_bytes,
            snapshot,
            metrics,
            backoff: Backoff::default(),
            failing: false,
            format_one_pulls: None,
        }))
    }

    /// Pulls until `stop` completes; a pull in flight is abandoned.
    pub async fn run(mut self, stop: impl Future<Output = ()>) {
        tokio::select! {
            () = stop => {}
            () = self.cycle() => {}
        }
    }

    async fn cycle(&mut self) {
        // The first pull is at once: the pod learns its mode and gets a snapshot before its
        // app sends much traffic (spec §7.7).
        loop {
            let wait = self.pull().await;
            sleep(wait).await;
        }
    }

    /// One pull; how long to wait before the next.
    async fn pull(&mut self) -> Duration {
        let (fetched, announced) = self.fetch().await;
        let wait = self.settle(fetched);
        // After the snapshot of the same answer: a sidecar that enters mock answers from it.
        if let Some(announced) = announced {
            self.snapshot.mode().observe(announced).await;
        }
        wait
    }

    /// Keeps or replaces the snapshot; how long to wait before the next pull.
    fn settle(&mut self, fetched: Fetched) -> Duration {
        match fetched {
            Fetched::Set {
                set,
                authorities,
                endpoints,
                etag,
                bytes,
            } => {
                self.snapshot.replace(set, etag, bytes, SystemTime::now());
                self.metrics.pull("ok");
                self.metrics.set_snapshot(endpoints, bytes);
                self.metrics.set_last_pull(unix_now());
                tracing::debug!(
                    target: "egress",
                    result = "ok",
                    bytes,
                    authorities,
                    endpoints,
                    "egress pulled"
                );
                self.succeeded();
                self.interval
            }
            Fetched::NotModified => {
                self.snapshot.touch(SystemTime::now());
                self.metrics.pull("not_modified");
                self.metrics.set_last_pull(unix_now());
                tracing::debug!(
                    target: "egress",
                    result = "not_modified",
                    bytes = self.snapshot.bytes(),
                    endpoints = self.snapshot.endpoints(),
                    "egress pulled"
                );
                self.succeeded();
                self.interval
            }
            Fetched::Failed(failure) => self.failed(failure),
        }
    }

    fn succeeded(&mut self) {
        self.backoff.success();
        if self.failing {
            self.failing = false;
            tracing::info!(target: "egress", "egress pull recovered");
        }
    }

    fn failed(&mut self, failure: Failure) -> Duration {
        self.metrics.pull(failure.result);
        if let Some(reason) = failure.reason {
            tracing::warn!(target: "egress", reason, "egress rejected");
        }
        if !self.failing {
            self.failing = true;
            tracing::warn!(
                target: "egress",
                status = failure.status,
                error = failure.error,
                result = failure.result,
                "egress pull failed"
            );
        }
        if failure.result == "error" {
            self.backoff
                .failure()
                .max(failure.retry_after.unwrap_or_default())
        } else {
            // The controller answered: the next pull is one interval away.
            self.backoff.success();
            self.interval
        }
    }

    /// The format to ask first: 2, except between two probes after a fallback.
    fn first_format(&mut self) -> u8 {
        match &mut self.format_one_pulls {
            Some(pulls) if *pulls + 1 < PROBE_EVERY => {
                *pulls += 1;
                FORMAT_WITHOUT_GRPC
            }
            _ => FORMAT_WITH_GRPC,
        }
    }

    /// The request of one pull in `format`; the failure of a request that got no answer.
    async fn send(&self, token: &str, format: u8) -> Result<reqwest::Response, Fetched> {
        let url = format!(
            "{}/v1/workloads/{}/egress?format={format}",
            self.target.url, self.target.workload
        );
        let mut request = self.http.get(url).bearer_auth(token.trim());
        if let Some(etag) = self.snapshot.etag() {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        // Without the URL: it is the configured one, and nothing else is useful here.
        request.send().await.map_err(|e| {
            let error = e.without_url().to_string();
            Fetched::Failed(Failure::error(None, Some(error), None))
        })
    }

    /// The token is read again for each pull: the kubelet rotates it.
    async fn fetch(&mut self) -> (Fetched, Option<Announced>) {
        let token = match tokio::fs::read_to_string(&self.target.token_file).await {
            Ok(token) => token,
            Err(e) => {
                let error = format!("token file: {}", e.kind());
                return (
                    Fetched::Failed(Failure::error(None, Some(error), None)),
                    None,
                );
            }
        };
        let first = self.first_format();
        let mut response = match self.send(&token, first).await {
            Ok(response) => response,
            Err(failed) => return (failed, None),
        };
        // A controller up to F13 does not know format 2: format 1 at once, in the same pull.
        if first == FORMAT_WITH_GRPC && response.status().as_u16() == 400 {
            if self.format_one_pulls.is_none() {
                tracing::info!(target: "egress", "egress format 2 refused, using format 1");
            }
            self.format_one_pulls = Some(0);
            response = match self.send(&token, FORMAT_WITHOUT_GRPC).await {
                Ok(response) => response,
                Err(failed) => return (failed, None),
            };
        } else if first == FORMAT_WITH_GRPC && matches!(response.status().as_u16(), 200 | 304) {
            // The controller serves format 2 (again): the fallback is over.
            self.format_one_pulls = None;
        }
        let status = response.status().as_u16();
        // The mode of a `200` counts even when its body is refused; no other answer has one.
        let announced =
            matches!(status, 200 | 304).then(|| Announced::from_headers(response.headers()));
        let fetched = match status {
            200 => self.read(response).await,
            304 => Fetched::NotModified,
            507 => Fetched::Failed(Failure::refused("too_large", Some(status), None)),
            404 | 405 => Fetched::Failed(Failure::refused("unsupported", Some(status), None)),
            _ => {
                let wait = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(retry_after);
                Fetched::Failed(Failure::error(Some(status), None, wait))
            }
        };
        (fetched, announced)
    }

    /// Reads the body of a `200`, at most `max_bytes` of it, and checks it. The buffer is
    /// freed before the set is returned.
    async fn read(&self, response: reqwest::Response) -> Fetched {
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = match read_capped(response, self.max_bytes).await {
            Ok(body) => body,
            Err(Body::TooLarge) => {
                return Fetched::Failed(Failure::refused("too_large", None, None));
            }
            Err(Body::Error(error)) => {
                return Fetched::Failed(Failure::error(None, Some(error), None));
            }
        };
        let bytes = body.len();
        // On a blocking thread: parsing a large set must not stall the proxy's workers.
        let parsed = tokio::task::spawn_blocking(move || {
            let parsed = std::str::from_utf8(&body)
                .map_err(|_| Refusal::Malformed)
                .and_then(EgressSet::from_json);
            drop(body);
            parsed.map(|set| {
                let (authorities, endpoints) = (set.authorities.len(), set.endpoint_count());
                (Index::build(set), authorities, endpoints)
            })
        })
        .await;
        match parsed {
            Ok(Ok((set, authorities, endpoints))) => Fetched::Set {
                set,
                authorities,
                endpoints,
                etag,
                bytes,
            },
            Ok(Err(refusal)) => {
                Fetched::Failed(Failure::refused("invalid", None, Some(refusal.reason())))
            }
            Err(e) => Fetched::Failed(Failure::error(None, Some(e.to_string()), None)),
        }
    }
}

/// How much the body buffer grows at a time.
const STEP: usize = 1 << 20;

enum Body {
    /// More than the cap: the read stopped there.
    TooLarge,
    Error(String),
}

/// The body of `response`, read chunk by chunk and refused as soon as it goes over `cap`:
/// a body of a gigabyte is never buffered.
async fn read_capped(mut response: reqwest::Response, cap: usize) -> Result<Vec<u8>, Body> {
    let declared = response
        .content_length()
        .map(|n| usize::try_from(n).unwrap_or(usize::MAX));
    if declared.is_some_and(|n| n > cap) {
        return Err(Body::TooLarge);
    }
    // Grown in steps as the bytes arrive: a declared length is not trusted with a reservation.
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| Body::Error(e.without_url().to_string()))?
    {
        if bytes.len() + chunk.len() > cap {
            return Err(Body::TooLarge);
        }
        if bytes.capacity() - bytes.len() < chunk.len() {
            let ahead = declared.map_or(0, |n| n.saturating_sub(bytes.len()));
            bytes.reserve_exact(chunk.len().max(ahead.min(STEP)));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
