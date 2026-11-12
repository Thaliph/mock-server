//! `GET /v1/workloads/<deployment>/egress?format=1|2` (spec §7.7): the egress set of the
//! caller's own deployment, built from the stored `:outbound`.
//!
//! The answer to one pull is `authorize_and_answer`; building a projection is `Job::build`, the
//! one place that reads `:outbound`. The cache, the single flight and the egress budget wrap
//! that call: a build runs as a task of its own, so that it ends (and releases its shares)
//! whether or not the pulls that wait for it are still there.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use axum::body::Bytes;
use axum::http::HeaderName;
use axum::http::HeaderValue;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use mimic_core::discovery::egress::{EgressSet, composite_hex, etag, matches_etag};
use prometheus_client::metrics::gauge::Gauge;
use tokio::sync::{Semaphore, watch};

use super::*;
use crate::store::{ReadError, parse_outbound, sha256_hex};
use crate::workloads::Effective;

pub(super) const ENDPOINT: &str = "/v1/workloads/{deployment}/egress";

/// Entries kept of what was logged; past it the set starts again.
const MAX_LOGGED: usize = 1024;

/// The most builds that run at once on a replica.
const MAX_BUILDS: usize = 2;

const UNREADABLE: Refusal = refusal(StatusCode::INTERNAL_SERVER_ERROR, "error");
const EGRESS_LIMIT: Refusal = refusal(StatusCode::INSUFFICIENT_STORAGE, "egress_limit");

/// The `egress limit reached` warnings already written: one per namespace, deployment and
/// reason for the life of the process.
#[derive(Clone, Default)]
pub struct Logged(Arc<Mutex<BTreeSet<LoggedKey>>>);

type LoggedKey = (String, String, &'static str);

impl Logged {
    fn first(&self, namespace: &str, deployment: &str, reason: &'static str) -> bool {
        let mut seen = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.len() >= MAX_LOGGED {
            seen.clear();
        }
        seen.insert((namespace.to_string(), deployment.to_string(), reason))
    }
}

/// Why a pull is refused, and why the controller could not answer (never the token or the key).
#[derive(Clone, Copy)]
struct Denied {
    refusal: Refusal,
    error: Option<&'static str>,
}

const fn denied(refusal: Refusal, error: &'static str) -> Denied {
    Denied {
        refusal,
        error: Some(error),
    }
}

impl From<Refusal> for Denied {
    fn from(refusal: Refusal) -> Denied {
        Denied {
            refusal,
            error: None,
        }
    }
}

/// What `MIMIC_SIDECAR_API_EGRESS_BUDGET` is, and what of it is held.
struct Held {
    budget: u64,
    used: AtomicU64,
    gauge: Gauge,
}

/// A serialised set. It holds its size of the egress budget until the cache dropped it and the
/// last answer that carries it is sent: all answers of one projection share this one share.
pub struct Projection {
    etag: String,
    body: Vec<u8>,
    held: Arc<Held>,
}

impl Drop for Projection {
    fn drop(&mut self) {
        let size = self.body.len();
        self.held.used.fetch_sub(size as u64, Ordering::AcqRel);
        self.held.gauge.dec_by(size as i64);
    }
}

/// The body of an answer: the projection itself, never a copy, so that its share lasts until
/// the answer is sent.
struct Sent(Arc<Projection>);

impl AsRef<[u8]> for Sent {
    fn as_ref(&self) -> &[u8] {
        &self.0.body
    }
}

type Served = Result<Arc<Projection>, Denied>;
type Slot = Option<Served>;
/// Namespace, deployment, format.
type EntryKey = (String, String, u32);
/// Namespace, deployment, format and the digest `:meta` gave (`None`: stale).
type FlightKey = (String, String, u32, Option<String>);

struct Entry {
    digest: String,
    projection: Arc<Projection>,
    /// When it was last served, in ticks of the cache.
    served: u64,
}

#[derive(Default)]
struct State {
    entries: BTreeMap<EntryKey, Entry>,
    flights: BTreeMap<FlightKey, Arc<watch::Sender<Slot>>>,
    tick: u64,
}

pub struct Inner {
    held: Arc<Held>,
    state: Mutex<State>,
    builds: Semaphore,
    /// Tests: the blocking part of a build waits while this is set.
    #[cfg(test)]
    pub pause: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    running: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    peak: std::sync::atomic::AtomicUsize,
}

/// Projections cached by namespace, deployment, format and digest, within
/// `MIMIC_SIDECAR_API_EGRESS_BUDGET`, and the builds in flight (spec §7.7).
#[derive(Clone)]
pub struct Cache(Arc<Inner>);

impl Deref for Cache {
    type Target = Inner;

    fn deref(&self) -> &Inner {
        &self.0
    }
}

/// What a pull finds.
enum Joined {
    Cached(Arc<Projection>),
    /// A build of the same deployment and digest is running.
    Wait(watch::Receiver<Slot>),
    /// None is: this pull starts the build.
    Lead(watch::Receiver<Slot>, Flight),
}

/// A build in flight. Leaving the table of flights is what its end does, also when it panics.
struct Flight {
    cache: Cache,
    key: FlightKey,
    tx: Arc<watch::Sender<Slot>>,
}

impl Flight {
    fn finish(self, served: Served) {
        self.leave();
        self.tx.send_replace(Some(served));
    }

    /// Leaves the table, unless a newer flight of the same key took the place: that one is not
    /// ours to remove.
    fn leave(&self) {
        let mut state = self.cache.lock();
        if state
            .flights
            .get(&self.key)
            .is_some_and(|tx| Arc::ptr_eq(tx, &self.tx))
        {
            state.flights.remove(&self.key);
        }
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        self.leave();
    }
}

impl Cache {
    pub fn new(budget: u64, metrics: &ApiMetrics) -> Cache {
        Cache(Arc::new(Inner {
            held: Arc::new(Held {
                budget,
                used: AtomicU64::new(0),
                gauge: metrics.egress_cache_bytes.clone(),
            }),
            state: Mutex::new(State::default()),
            builds: Semaphore::new(MAX_BUILDS),
            #[cfg(test)]
            pause: Default::default(),
            #[cfg(test)]
            running: Default::default(),
            #[cfg(test)]
            peak: Default::default(),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The cached projection of `digest`, a build to wait for, or the lead of a new one. The
    /// cache is read under the lock that guards the flights, and a build caches before it
    /// leaves them: a pull never starts a build that has just ended.
    fn join(&self, namespace: &str, deployment: &str, format: u32, digest: Option<&str>) -> Joined {
        let mut state = self.lock();
        let key = (
            namespace.to_string(),
            deployment.to_string(),
            format,
            digest.map(str::to_string),
        );
        if let Some(tx) = state.flights.get(&key) {
            return Joined::Wait(tx.subscribe());
        }
        if let Some(digest) = digest {
            state.tick += 1;
            let tick = state.tick;
            let entry_key = (key.0.clone(), key.1.clone(), format);
            if let Some(entry) = state.entries.get_mut(&entry_key)
                && entry.digest == digest
            {
                entry.served = tick;
                return Joined::Cached(entry.projection.clone());
            }
        }
        let (tx, rx) = watch::channel(None);
        let tx = Arc::new(tx);
        state.flights.insert(key.clone(), tx.clone());
        Joined::Lead(
            rx,
            Flight {
                cache: self.clone(),
                key,
                tx,
            },
        )
    }

    /// Caches a built set, dropping what makes room: the older digest of the same deployment,
    /// then the cached projections that are not being sent, the least recently served first.
    /// `None` when it still does not fit.
    fn admit(
        &self,
        namespace: &str,
        deployment: &str,
        format: u32,
        hex: &str,
        body: Vec<u8>,
    ) -> Option<Arc<Projection>> {
        let size = body.len() as u64;
        let mut state = self.lock();
        let key = (namespace.to_string(), deployment.to_string(), format);
        state.entries.remove(&key);
        while self.held.used.load(Ordering::Acquire).saturating_add(size) > self.held.budget {
            let victim = state
                .entries
                .iter()
                .filter(|(_, entry)| Arc::strong_count(&entry.projection) == 1)
                .min_by_key(|(_, entry)| entry.served)
                .map(|(key, _)| key.clone());
            state.entries.remove(&victim?);
        }
        self.held.used.fetch_add(size, Ordering::AcqRel);
        self.held.gauge.inc_by(size as i64);
        let projection = Arc::new(Projection {
            etag: etag(format, hex),
            body,
            held: self.held.clone(),
        });
        state.tick += 1;
        let served = state.tick;
        state.entries.insert(
            key,
            Entry {
                digest: hex.to_string(),
                projection: projection.clone(),
                served,
            },
        );
        Some(projection)
    }

    /// Tests: marks a build running in its blocking part and waits while `pause` is set.
    #[cfg(test)]
    fn gate(&self) -> impl Drop + use<> {
        struct Running(Cache);
        impl Drop for Running {
            fn drop(&mut self) {
                self.0.running.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        let running = Running(self.clone());
        while self.pause.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        running
    }

    /// Tests: builds in their blocking part now.
    #[cfg(test)]
    pub fn running(&self) -> usize {
        self.running.load(Ordering::SeqCst)
    }

    /// Tests: the most builds that were in their blocking part at once.
    #[cfg(test)]
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

/// What a pull is answered with.
enum Answered {
    /// `304`: the caller holds this `ETag`.
    NotModified(String),
    /// `200`: the projection, which carries its `ETag`.
    Set(Arc<Projection>),
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
fn project(stored: Option<Vec<u8>>, grpc: Option<Vec<u8>>, format: u32, max: usize) -> Built {
    let mut hex = sha256_hex(stored.as_deref().unwrap_or_default());
    if let Some(bytes) = &grpc {
        hex = composite_hex(&hex, Some(&sha256_hex(bytes)));
    }
    let set = match &stored {
        None => EgressSet {
            authorities: Default::default(),
            format,
        },
        Some(bytes) => match parse_outbound(bytes) {
            Some(mut report) => {
                if let Some(bytes) = &grpc {
                    match crate::store::read_overlay(bytes) {
                        Some(overlay) => report.apply_grpc(overlay),
                        None => return Built::Unreadable,
                    }
                }
                EgressSet::project_as(&report, format)
            }
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

/// The format the query asks for: `format=1` or `format=2`, nothing else (spec §7.7).
fn wanted_format(query: Option<&str>) -> Option<u32> {
    let value = query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|pair| pair.strip_prefix("format="))?;
    match value {
        "1" => Some(1),
        "2" => Some(2),
        _ => None,
    }
}

/// Why a read of the store gave no answer.
fn read_denied(error: ReadError) -> Denied {
    match error {
        ReadError::Name => invalid("deployment").into(),
        ReadError::Unreadable => UNREADABLE.into(),
        // The reads of a pull map an absent `:meta` or `:outbound` to `None` and 0 (the empty
        // set, §7.7), so they never give `NotFound`; were one to, a retry is the safe answer.
        ReadError::NotFound | ReadError::Unavailable => denied(UNAVAILABLE, "store unavailable"),
    }
}

/// Notes why the controller could not answer, and gives the refusal that goes out.
fn refuse(denied: Denied, caller: &mut Caller) -> Refusal {
    if let Some(error) = denied.error {
        caller.error = Some(error.to_string());
    }
    denied.refusal
}

const MODE: HeaderName = HeaderName::from_static("mimic-mode");
const LATENCY_FACTOR: HeaderName = HeaderName::from_static("mimic-latency-factor");

/// Adds the deployment's effective mode (spec §7.7): outside the `ETag` and the projection.
fn with_mode(mut response: Response, mode: &Effective) -> Response {
    let headers = response.headers_mut();
    headers.insert(MODE, HeaderValue::from_static(mode.mode().as_str()));
    if mode.mock
        && let Ok(factor) = HeaderValue::from_str(&mode.factor_text())
    {
        headers.insert(LATENCY_FACTOR, factor);
    }
    response
}

fn with_etag(mut response: Response, etag: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(etag) {
        response.headers_mut().insert(ETAG, value);
    }
    response
}

/// What a build owns, so that it can run in a task of its own.
struct Job {
    store: Arc<Store>,
    budget: BodyBudget,
    cache: Cache,
    metrics: ApiMetrics,
    logged: Logged,
    max_egress: usize,
}

impl Job {
    /// Builds the projection of the stored `:outbound` (also the one a stale digest forces).
    /// It holds a share of the body budget, the stored size plus the largest projection, from
    /// before the read until the set holds its egress share (or is refused): the share moves
    /// into the blocking task, so it lasts until that task ends (spec §7.7).
    async fn build(&self, namespace: &str, deployment: &str, format: u32) -> Served {
        let _permit = self
            .cache
            .builds
            .acquire()
            .await
            .map_err(|_| denied(UNAVAILABLE, "egress builds closed"))?;
        let started = Instant::now();
        let stored = self
            .store
            .outbound_len(namespace, deployment)
            .await
            .map_err(read_denied)?;
        // Format 2 reads the gRPC overlay too (spec §7.7).
        let grpc_len = if format == 2 {
            self.store
                .grpc_len(namespace, deployment)
                .await
                .map_err(read_denied)?
        } else {
            0
        };
        let stored = stored.saturating_add(grpc_len);
        let share = stored.saturating_add(self.max_egress as u64);
        // It could never be served: refused without a read, and not `503`, which is retried.
        if share > self.budget.capacity() {
            let stored = usize::try_from(stored).unwrap_or(usize::MAX);
            return Err(self.too_large(namespace, deployment, stored, "stored"));
        }
        let reservation = self
            .budget
            .reserve(share)
            .ok_or(denied(UNAVAILABLE, "body budget spent"))?;
        let bytes = self
            .store
            .outbound_bytes(namespace, deployment)
            .await
            .map_err(read_denied)?;
        let grpc = if format == 2 && grpc_len > 0 {
            self.store
                .grpc_bytes(namespace, deployment)
                .await
                .map_err(read_denied)?
        } else {
            None
        };
        let max = self.max_egress;
        let cache = self.cache.clone();
        let (ns, dep) = (namespace.to_string(), deployment.to_string());
        let built = tokio::task::spawn_blocking(move || {
            // Released when this task ends, whoever is still waiting for it.
            let _share = reservation;
            #[cfg(test)]
            let _running = cache.gate();
            match project(bytes, grpc, format, max) {
                Built::Set(hex, body) => Ok(cache.admit(&ns, &dep, format, &hex, body)),
                other => Err(other),
            }
        })
        .await
        // Never the panic payload: it could hold stored content.
        .map_err(|e| {
            denied(
                UNAVAILABLE,
                if e.is_panic() {
                    "egress task panicked"
                } else {
                    "egress task cancelled"
                },
            )
        })?;
        match built {
            Ok(projection) => {
                self.count_build("ok");
                self.metrics
                    .egress_build_duration
                    .observe(started.elapsed().as_secs_f64());
                projection.ok_or(denied(UNAVAILABLE, "egress budget spent"))
            }
            Err(Built::TooLarge(size)) => {
                Err(self.too_large(namespace, deployment, size, "projection"))
            }
            Err(_) => {
                self.count_build("error");
                tracing::error!(
                    namespace,
                    deployment,
                    key = "outbound",
                    "stored report unreadable"
                );
                Err(UNREADABLE.into())
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
    ) -> Denied {
        self.count_build("too_large");
        if self.logged.first(namespace, deployment, reason) {
            tracing::warn!(namespace, deployment, bytes, reason, "egress limit reached");
        }
        EGRESS_LIMIT.into()
    }
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
            Ok(Answered::Set(_)) => "ok",
            Ok(Answered::NotModified(_)) => "not_modified",
            Err(refusal) => refusal.result,
        };
        let reason = outcome.as_ref().err().and_then(|r| r.reason);
        self.record(ENDPOINT, &caller, result, reason);
        let mode = match &outcome {
            Ok(_) => Some(self.pulled(&caller)),
            Err(_) => None,
        };
        let response = match outcome {
            Ok(Answered::Set(projection)) => {
                let etag = projection.etag.clone();
                let response = (
                    [
                        (CONTENT_TYPE, HeaderValue::from_static("application/json")),
                        (CACHE_CONTROL, HeaderValue::from_static("no-store")),
                    ],
                    Body::from(Bytes::from_owner(Sent(projection))),
                )
                    .into_response();
                with_etag(response, &etag)
            }
            Ok(Answered::NotModified(etag)) => {
                with_etag(StatusCode::NOT_MODIFIED.into_response(), &etag)
            }
            Err(refusal) => refusal.into_response(),
        };
        match mode {
            Some(mode) => with_mode(response, &mode),
            None => response,
        }
    }

    /// The mode a `200` or `304` carries, read when the answer is written, and noted in
    /// `:pulls`.
    fn pulled(&self, caller: &Caller) -> Effective {
        let mode = self
            .workloads
            .effective(&caller.namespace, &caller.deployment);
        self.store.record_pull(
            &caller.namespace,
            &caller.deployment,
            &caller.pod,
            mode.mode().as_str(),
        );
        mode
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
        let Some(format) = wanted_format(query) else {
            return Err(invalid("format"));
        };

        let namespace = identity.namespace.as_str();
        // `:outbound` is read only when the digest cannot answer.
        let digest = self
            .store
            .outbound_digest(namespace, deployment)
            .await
            .map_err(|e| refuse(read_denied(e), caller))?;
        // The digest of format 2 holds the gRPC overlay's (spec §7.7).
        let digest = match digest {
            Some(hex) if format == 2 => {
                let grpc = self
                    .store
                    .grpc_digest(namespace, deployment)
                    .await
                    .map_err(|e| refuse(read_denied(e), caller))?;
                Some(composite_hex(&hex, grpc.as_deref()))
            }
            other => other,
        };
        if let Some(hex) = &digest {
            let current = etag(format, hex);
            // Every value of every header (RFC 9110 §13.1.2).
            let mut held = headers
                .get_all(IF_NONE_MATCH)
                .iter()
                .filter_map(|v| v.to_str().ok());
            if held.any(|held| matches_etag(held, &current)) {
                caller.bytes = Some(0);
                return Ok(Answered::NotModified(current));
            }
        }
        let projection = self
            .projection(namespace, deployment, format, digest.as_deref())
            .await
            .map_err(|denied| refuse(denied, caller))?;
        caller.bytes = Some(projection.body.len());
        Ok(Answered::Set(projection))
    }

    /// The projection for a digest: from the cache, from the build that is running, or from a
    /// build this pull starts. The build is a task of its own that ends on its own.
    async fn projection(
        &self,
        namespace: &str,
        deployment: &str,
        format: u32,
        digest: Option<&str>,
    ) -> Served {
        let mut rx = match self.egress.join(namespace, deployment, format, digest) {
            Joined::Cached(projection) => return Ok(projection),
            Joined::Wait(rx) => rx,
            Joined::Lead(rx, flight) => {
                let job = Job {
                    store: self.store.clone(),
                    budget: self.budget.clone(),
                    cache: self.egress.clone(),
                    metrics: self.metrics.clone(),
                    logged: self.egress_logged.clone(),
                    max_egress: self.max_egress,
                };
                let (ns, dep) = (namespace.to_string(), deployment.to_string());
                tokio::spawn(async move {
                    let served = job.build(&ns, &dep, format).await;
                    flight.finish(served);
                });
                rx
            }
        };
        // `Err`: the build task ended without an answer (it panicked).
        match rx.wait_for(Option::is_some).await {
            Ok(slot) => slot
                .clone()
                .unwrap_or(Err(denied(UNAVAILABLE, "egress build lost"))),
            Err(_) => Err(denied(UNAVAILABLE, "egress build lost")),
        }
    }
}

#[cfg(test)]
mod tests {
    use prometheus_client::registry::Registry;

    use super::*;

    #[test]
    fn pul_09_an_old_flight_that_drops_leaves_the_newer_one_of_its_key() {
        let cache = Cache::new(1 << 20, &ApiMetrics::register(&mut Registry::default()));
        let Joined::Lead(_rx, old) = cache.join("shop", "pay", 1, None) else {
            panic!("the first pull leads");
        };
        // The old build failed and left the table; a new pull starts a build of the same key.
        cache.lock().flights.remove(&old.key);
        let Joined::Lead(_rx, _new) = cache.join("shop", "pay", 1, None) else {
            panic!("a new build starts");
        };
        drop(old);
        assert!(
            matches!(cache.join("shop", "pay", 1, None), Joined::Wait(_)),
            "the old guard removed the newer flight: a duplicate build would start"
        );
    }
}
