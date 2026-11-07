//! `GET /v1/workloads/<deployment>/egress?format=1` (spec §7.7): the egress set of the
//! caller's own deployment, built from the stored `:outbound`.
//!
//! The answer to one pull is `authorize_and_answer`; building a projection is `build`, the one
//! place that reads `:outbound`, so that the cache, the single flight and the egress budget of
//! F9 step 4 wrap that call and nothing else.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::Mutex;
use std::time::Instant;

use axum::http::HeaderValue;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use mimic_core::discovery::egress::{EgressSet, FORMAT, etag, matches_etag};

use super::*;
use crate::store::{ReadError, parse_outbound, sha256_hex};

pub(super) const ENDPOINT: &str = "/v1/workloads/{deployment}/egress";

/// Entries kept of what was logged; past it the set starts again.
const MAX_LOGGED: usize = 1024;

const UNREADABLE: Refusal = refusal(StatusCode::INTERNAL_SERVER_ERROR, "error");
const EGRESS_LIMIT: Refusal = refusal(StatusCode::INSUFFICIENT_STORAGE, "egress_limit");

/// The `egress limit reached` warnings already written: one per namespace, deployment and
/// reason for the life of the process.
#[derive(Default)]
pub struct Logged(Mutex<BTreeSet<(String, String, &'static str)>>);

impl Logged {
    fn first(&self, namespace: &str, deployment: &str, reason: &'static str) -> bool {
        let mut seen = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.len() >= MAX_LOGGED {
            seen.clear();
        }
        seen.insert((namespace.to_string(), deployment.to_string(), reason))
    }
}

/// What a pull is answered with.
enum Answered {
    /// `304`: the caller holds this `ETag`.
    NotModified(String),
    /// `200`: the serialised set and its `ETag`.
    Set { etag: String, body: Vec<u8> },
}

/// What building a projection gives.
enum Built {
    /// The hex SHA-256 of the stored bytes and the serialised set.
    Set(String, Vec<u8>),
    /// The stored bytes do not parse or fail the check.
    Unreadable,
    /// The serialised set is this many bytes, over the limit.
    TooLarge(usize),
}

/// Counts what is written and keeps the first `max` bytes: the size of a set that is over the
/// limit is known without holding it.
struct Capped {
    kept: Vec<u8>,
    max: usize,
    total: usize,
}

impl io::Write for Capped {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.total = self.total.saturating_add(data.len());
        if self.total <= self.max {
            self.kept.extend_from_slice(data);
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The projection of stored `:outbound` bytes (none: the empty set) with the hash of the bytes,
/// for a blocking thread: parsing and serialising are CPU work.
fn project(stored: Option<Vec<u8>>, max: usize) -> Built {
    let hex = sha256_hex(stored.as_deref().unwrap_or_default());
    let set = match &stored {
        None => EgressSet {
            authorities: BTreeMap::new(),
            format: FORMAT,
        },
        Some(bytes) => match parse_outbound(bytes) {
            Some(report) => EgressSet::project(&report),
            None => return Built::Unreadable,
        },
    };
    drop(stored);
    let mut out = Capped {
        kept: Vec::new(),
        max,
        total: 0,
    };
    // Plain data into memory: serialisation cannot fail.
    if serde_json::to_writer(&mut out, &set).is_err() {
        return Built::Unreadable;
    }
    if out.total > max {
        Built::TooLarge(out.total)
    } else {
        Built::Set(hex, out.kept)
    }
}

/// Whether the query asks for `format=1`, the only format there is.
fn wants_format_one(query: Option<&str>) -> bool {
    query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|pair| pair.strip_prefix("format="))
        .is_some_and(|value| value == FORMAT.to_string())
}

/// Why a read of the store gave no answer.
fn read_refusal(error: ReadError, caller: &mut Caller) -> Refusal {
    match error {
        ReadError::Name => invalid("deployment"),
        ReadError::Unreadable => UNREADABLE,
        ReadError::NotFound | ReadError::Unavailable => {
            caller.error = Some("store unavailable".to_string());
            UNAVAILABLE
        }
    }
}

fn with_etag(mut response: Response, etag: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(etag) {
        response.headers_mut().insert(ETAG, value);
    }
    response
}

impl SidecarApi {
    /// `GET /v1/workloads/<deployment>/egress` (spec §7.7).
    pub(super) async fn egress(
        &self,
        deployment: &str,
        headers: &HeaderMap,
        query: Option<&str>,
        peer: Option<std::net::IpAddr>,
    ) -> Response {
        let mut caller = Caller::default();
        let outcome = self
            .authorize_and_answer(deployment, headers, query, peer, &mut caller)
            .await;
        let result = match &outcome {
            Ok(Answered::Set { .. }) => "ok",
            Ok(Answered::NotModified(_)) => "not_modified",
            Err(refusal) => refusal.result,
        };
        let reason = outcome.as_ref().err().and_then(|r| r.reason);
        self.record(ENDPOINT, &caller, result, reason);
        match outcome {
            Ok(Answered::Set { etag, body }) => {
                let response = (
                    [
                        (CONTENT_TYPE, HeaderValue::from_static("application/json")),
                        (CACHE_CONTROL, HeaderValue::from_static("no-store")),
                    ],
                    body,
                )
                    .into_response();
                with_etag(response, &etag)
            }
            Ok(Answered::NotModified(etag)) => {
                with_etag(StatusCode::NOT_MODIFIED.into_response(), &etag)
            }
            Err(refusal) => refusal.into_response(),
        }
    }

    async fn authorize_and_answer(
        &self,
        deployment: &str,
        headers: &HeaderMap,
        query: Option<&str>,
        peer: Option<std::net::IpAddr>,
        caller: &mut Caller,
    ) -> Result<Answered, Refusal> {
        self.admit(peer)?;
        let identity = self.authenticate(headers, peer, caller).await?;
        if !valid_name(deployment) {
            return Err(invalid("deployment"));
        }
        caller.deployment = deployment.to_string();
        let pod = identity.pod.as_deref().ok_or(FORBIDDEN)?;
        if !pod_of_deployment(pod, deployment) {
            return Err(FORBIDDEN);
        }
        if !wants_format_one(query) {
            return Err(invalid("format"));
        }

        let namespace = identity.namespace.as_str();
        // `:outbound` is read only when the digest cannot answer.
        let digest = self
            .store
            .outbound_digest(namespace, deployment)
            .await
            .map_err(|e| read_refusal(e, caller))?;
        if let Some(hex) = digest {
            let current = etag(&hex);
            let held = headers.get(IF_NONE_MATCH).and_then(|v| v.to_str().ok());
            if held.is_some_and(|held| matches_etag(held, &current)) {
                caller.bytes = Some(0);
                return Ok(Answered::NotModified(current));
            }
        }
        self.build(namespace, deployment, caller).await
    }

    /// Builds the projection of the stored `:outbound` (also the one a stale digest forces).
    /// It holds a share of the body budget, the stored size plus the largest projection, from
    /// before the read until the set is built (spec §7.7).
    async fn build(
        &self,
        namespace: &str,
        deployment: &str,
        caller: &mut Caller,
    ) -> Result<Answered, Refusal> {
        let started = Instant::now();
        let stored = self
            .store
            .outbound_len(namespace, deployment)
            .await
            .map_err(|e| read_refusal(e, caller))?;
        let share = stored.saturating_add(self.max_egress as u64);
        // It could never be served: refused without a read, and not `503`, which is retried.
        if share > self.budget.capacity() {
            let stored = usize::try_from(stored).unwrap_or(usize::MAX);
            return Err(self.too_large(namespace, deployment, stored, "stored"));
        }
        let _share = self.budget.reserve(share).ok_or_else(|| {
            caller.error = Some("body budget spent".to_string());
            UNAVAILABLE
        })?;
        let bytes = self
            .store
            .outbound_bytes(namespace, deployment)
            .await
            .map_err(|e| read_refusal(e, caller))?;
        let max = self.max_egress;
        let built = tokio::task::spawn_blocking(move || project(bytes, max))
            .await
            .map_err(|e| {
                // Never the panic payload: it could hold stored content.
                caller.error = Some(
                    if e.is_panic() {
                        "egress task panicked"
                    } else {
                        "egress task cancelled"
                    }
                    .into(),
                );
                UNAVAILABLE
            })?;
        match built {
            Built::Set(hex, body) => {
                self.count_build("ok");
                self.metrics
                    .egress_build_duration
                    .observe(started.elapsed().as_secs_f64());
                caller.bytes = Some(body.len());
                Ok(Answered::Set {
                    etag: etag(&hex),
                    body,
                })
            }
            Built::TooLarge(size) => Err(self.too_large(namespace, deployment, size, "projection")),
            Built::Unreadable => {
                self.count_build("error");
                tracing::error!(
                    namespace,
                    deployment,
                    key = "outbound",
                    "stored report unreadable"
                );
                Err(UNREADABLE)
            }
        }
    }

    fn count_build(&self, result: &str) {
        self.metrics
            .egress_builds
            .get_or_create(&vec![("result".to_string(), result.to_string())])
            .inc();
    }

    /// A set that cannot be served: counted every time, warned once per deployment and reason.
    fn too_large(
        &self,
        namespace: &str,
        deployment: &str,
        bytes: usize,
        reason: &'static str,
    ) -> Refusal {
        self.count_build("too_large");
        if self.egress_logged.first(namespace, deployment, reason) {
            tracing::warn!(namespace, deployment, bytes, reason, "egress limit reached");
        }
        EGRESS_LIMIT
    }
}
