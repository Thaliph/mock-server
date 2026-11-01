//! Sidecar API (spec §7.2): sidecars authenticate with their projected service-account token,
//! checked with a `TokenReview`, and ask for an intermediate CA certificate.
//!
//! Neither the token nor the posted key is ever logged or put in a metric label.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, PoisonError};

use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{AUTHORIZATION, CONTENT_LENGTH, RETRY_AFTER};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine;
use futures::StreamExt;
use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec};
use kube::api::{Api, PostParams};
use mimic_core::discovery::redact::Redactor;
use mimic_core::discovery::report::Report;
use mimic_core::identity::pod_of_deployment;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::registry::Registry;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio::sync::OnceCell;

use crate::store::{Outcome, Push, SeqId, Store, StoreError};
use crate::tls_ca::{SeedSlot, derive_root};
use crate::workloads::WorkloadCache;

mod admission;

pub use admission::{BodyBudget, RateLimiter, ReviewCache};

/// Audience of the projected token (spec §7.1).
pub const AUDIENCE: &str = "mimic.platform.io";
/// Largest request body accepted.
pub const MAX_BODY: usize = 64 * 1024;
const ENDPOINT: &str = "/v1/tls/intermediate";
const REPORT_ENDPOINT: &str = "/v1/workloads/{deployment}/report";
/// Claim holding the pod a service-account token is bound to.
const POD_NAME_CLAIM: &str = "authentication.kubernetes.io/pod-name";

type Labels = Vec<(String, String)>;

/// `mimic_sidecar_api_requests_total{endpoint,result}`.
#[derive(Clone, Default)]
pub struct ApiMetrics {
    requests: Family<Labels, Counter>,
}

impl ApiMetrics {
    pub fn register(registry: &mut Registry) -> ApiMetrics {
        let metrics = ApiMetrics::default();
        registry.register(
            "sidecar_api_requests",
            "Requests answered by the sidecar API",
            metrics.requests.clone(),
        );
        metrics
    }
}

/// What a `TokenReview` says about a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub authenticated: bool,
    /// `system:serviceaccount:<namespace>:<name>`.
    pub username: String,
    /// The pod the token is bound to.
    pub pod: Option<String>,
}

pub type ReviewFuture<'a> = Pin<Box<dyn Future<Output = Result<Review, String>> + Send + 'a>>;

/// Validates tokens; an error means the answer could not be obtained.
pub trait Reviewer: Send + Sync {
    fn review<'a>(&'a self, token: &'a str) -> ReviewFuture<'a>;
}

/// `TokenReview` against the Kubernetes API server.
pub struct KubeReviewer {
    client: Arc<OnceCell<kube::Client>>,
}

impl KubeReviewer {
    /// `client` is set once the cluster client is available; reviews fail until then.
    pub fn new(client: Arc<OnceCell<kube::Client>>) -> KubeReviewer {
        KubeReviewer { client }
    }
}

impl Reviewer for KubeReviewer {
    fn review<'a>(&'a self, token: &'a str) -> ReviewFuture<'a> {
        Box::pin(async move {
            let review = TokenReview {
                spec: TokenReviewSpec {
                    token: Some(token.to_string()),
                    audiences: Some(vec![AUDIENCE.to_string()]),
                },
                ..TokenReview::default()
            };
            let client = self.client.get().ok_or("no Kubernetes client yet")?;
            let answer = Api::<TokenReview>::all(client.clone())
                .create(&PostParams::default(), &review)
                .await
                .map_err(|e| format!("token review: {e}"))?;
            let status = answer.status.unwrap_or_default();
            // The API server answers for the audiences it validated: ours must be among them.
            let audience = status
                .audiences
                .as_ref()
                .is_some_and(|audiences| audiences.iter().any(|a| a == AUDIENCE));
            let user = status.user.unwrap_or_default();
            let pod = user
                .extra
                .as_ref()
                .and_then(|extra| extra.get(POD_NAME_CLAIM))
                .and_then(|values| values.first())
                .cloned();
            Ok(Review {
                authenticated: status.authenticated.unwrap_or(false) && audience,
                username: user.username.unwrap_or_default(),
                pod,
            })
        })
    }
}

/// `TokenReview` budget of one source IP: burst and calls per second (spec §7.2).
pub const SOURCE_BURST: u32 = 10;
pub const SOURCE_PER_SECOND: u32 = 2;
/// Budget of all sources together.
pub const GLOBAL_BURST: u32 = 200;
pub const GLOBAL_PER_SECOND: u32 = 100;
/// Sources tracked; the oldest is forgotten first.
pub const MAX_SOURCES: usize = 4096;

/// A token bucket.
struct Bucket {
    tokens: f64,
    at: tokio::time::Instant,
}

impl Bucket {
    fn full(burst: u32) -> Bucket {
        Bucket {
            tokens: f64::from(burst),
            at: tokio::time::Instant::now(),
        }
    }

    /// Whether one call is left, without taking it.
    fn refill(&mut self, burst: u32, per_second: u32) -> bool {
        let now = tokio::time::Instant::now();
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * f64::from(per_second)).min(f64::from(burst));
        self.at = now;
        self.tokens >= 1.0
    }
}

struct LimiterState {
    global: Bucket,
    sources: std::collections::BTreeMap<std::net::IpAddr, Bucket>,
    /// Insertion order of `sources`, for eviction.
    order: std::collections::VecDeque<std::net::IpAddr>,
}

/// Limits `TokenReview` calls: per source IP, so that one pod with garbage tokens cannot starve
/// the sidecars of the others, and in total.
pub struct ReviewLimiter {
    source: (u32, u32),
    global: (u32, u32),
    max_sources: usize,
    state: std::sync::Mutex<LimiterState>,
}

impl ReviewLimiter {
    /// `source` and `global` are `(burst, per second)`.
    pub fn new(source: (u32, u32), global: (u32, u32), max_sources: usize) -> ReviewLimiter {
        ReviewLimiter {
            source,
            global,
            max_sources,
            state: std::sync::Mutex::new(LimiterState {
                global: Bucket::full(global.0),
                sources: std::collections::BTreeMap::new(),
                order: std::collections::VecDeque::new(),
            }),
        }
    }

    /// Takes one call from the budget of `source` and the global one; false when either is
    /// spent (a refused call costs the global budget nothing).
    pub fn allow(&self, source: Option<std::net::IpAddr>) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let state = &mut *state;
        // Without a known source the call is only held to the global budget.
        if let Some(ip) = source {
            if !state.sources.contains_key(&ip) {
                if state.order.len() >= self.max_sources
                    && let Some(oldest) = state.order.pop_front()
                {
                    state.sources.remove(&oldest);
                }
                state.order.push_back(ip);
                state.sources.insert(ip, Bucket::full(self.source.0));
            }
            if let Some(bucket) = state.sources.get_mut(&ip)
                && !bucket.refill(self.source.0, self.source.1)
            {
                return false;
            }
        }
        if !state.global.refill(self.global.0, self.global.1) {
            return false;
        }
        state.global.tokens -= 1.0;
        if let Some(ip) = source
            && let Some(bucket) = state.sources.get_mut(&ip)
        {
            bucket.tokens -= 1.0;
        }
        true
    }

    /// Sources tracked now.
    #[cfg(test)]
    fn sources(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sources
            .len()
    }
}

impl Default for ReviewLimiter {
    fn default() -> ReviewLimiter {
        ReviewLimiter::new(
            (SOURCE_BURST, SOURCE_PER_SECOND),
            (GLOBAL_BURST, GLOBAL_PER_SECOND),
            MAX_SOURCES,
        )
    }
}

/// The IP of the connection a request came in on, put in the request by the listener.
#[derive(Debug, Clone, Copy)]
pub struct Peer(pub std::net::IpAddr);

/// What the endpoints need.
pub struct SidecarApi {
    pub seed: Arc<SeedSlot>,
    pub reviewer: Arc<dyn Reviewer>,
    pub metrics: ApiMetrics,
    /// `TokenReview` calls per source and in total (spec §7.2).
    pub limiter: ReviewLimiter,
    /// Requests per source (`MIMIC_SIDECAR_API_RATE`).
    pub rate: RateLimiter,
    pub reviews: ReviewCache,
    /// Push bodies being read (`MIMIC_SIDECAR_API_BODY_BUDGET`).
    pub budget: BodyBudget,
    pub store: Arc<Store>,
    pub workloads: Arc<WorkloadCache>,
    /// `MIMIC_SIDECAR_API_MAX_REPORT`.
    pub max_report: usize,
    /// `MIMIC_STORE_MAX_PUSH_ENDPOINTS`.
    pub max_push_endpoints: usize,
}

pub fn router(api: Arc<SidecarApi>) -> Router {
    Router::new()
        .route(
            ENDPOINT,
            post(
                |State(api): State<Arc<SidecarApi>>, request: axum::extract::Request| async move {
                    let peer = request.extensions().get::<Peer>().map(|peer| peer.0);
                    let headers = request.headers().clone();
                    api.intermediate(&headers, peer, request.into_body()).await
                },
            ),
        )
        .route(
            "/v1/workloads/{deployment}/report",
            post(
                |State(api): State<Arc<SidecarApi>>,
                 Path(deployment): Path<String>,
                 request: axum::extract::Request| async move {
                    let peer = request.extensions().get::<Peer>().map(|peer| peer.0);
                    let headers = request.headers().clone();
                    api.report(&deployment, &headers, peer, request.into_body())
                        .await
                },
            ),
        )
        .with_state(api)
}

/// Why a request was refused: the status and the `result` label of the log line and metric.
struct Refusal {
    status: StatusCode,
    result: &'static str,
    /// Answered with `Retry-After: 1`.
    retry: bool,
    /// For `invalid`, why: a fixed string, never part of the request.
    reason: Option<&'static str>,
}

impl Refusal {
    fn into_response(self) -> Response {
        if self.retry {
            (self.status, [(RETRY_AFTER, "1")]).into_response()
        } else {
            self.status.into_response()
        }
    }
}

const fn refusal(status: StatusCode, result: &'static str) -> Refusal {
    Refusal {
        status,
        result,
        retry: false,
        reason: None,
    }
}

const UNAUTHENTICATED: Refusal = refusal(StatusCode::UNAUTHORIZED, "unauthenticated");
const FORBIDDEN: Refusal = refusal(StatusCode::FORBIDDEN, "forbidden");
const TOO_LARGE: Refusal = refusal(StatusCode::PAYLOAD_TOO_LARGE, "too_large");
const PUSH_LIMIT: Refusal = refusal(StatusCode::INSUFFICIENT_STORAGE, "push_limit");
const DEPLOYMENT_LIMIT: Refusal = refusal(StatusCode::INSUFFICIENT_STORAGE, "deployment_limit");
const RATE_LIMITED: Refusal = Refusal {
    retry: true,
    ..refusal(StatusCode::TOO_MANY_REQUESTS, "rate_limited")
};
const UNAVAILABLE: Refusal = Refusal {
    retry: true,
    ..refusal(StatusCode::SERVICE_UNAVAILABLE, "unavailable")
};

const fn invalid(reason: &'static str) -> Refusal {
    Refusal {
        reason: Some(reason),
        ..refusal(StatusCode::BAD_REQUEST, "invalid")
    }
}

/// What is known of the caller, for the log line.
#[derive(Default)]
struct Caller {
    namespace: String,
    pod: String,
    deployment: String,
    /// A push: its session, sequence and declared size.
    session: Option<String>,
    sequence: Option<u64>,
    bytes: Option<usize>,
    /// Why the controller could not answer (never the token or the key).
    error: Option<String>,
}

/// Why the signing task gave no certificate.
enum Signing {
    /// The posted key cannot be certified.
    Key,
    /// The root could not be derived.
    Derive(String),
}

#[derive(Deserialize)]
struct IntermediateRequest {
    deployment: String,
    public_key: String,
}

/// The body of a push; the report is checked as a whole, after the envelope.
#[derive(Deserialize)]
struct PushRequest {
    session: String,
    sequence: u64,
    report: Box<RawValue>,
}

/// An authenticated caller.
struct Identity {
    namespace: String,
    pod: Option<String>,
}

impl SidecarApi {
    /// Counts the request and writes its one log line.
    fn record(&self, endpoint: &str, caller: &Caller, result: &str, reason: Option<&str>) {
        self.metrics
            .requests
            .get_or_create(&vec![
                ("endpoint".to_string(), endpoint.to_string()),
                ("result".to_string(), result.to_string()),
            ])
            .inc();
        // A flood of rate-limited requests must not flood the logs.
        macro_rules! line {
            ($level:ident) => {
                tracing::$level!(
                    target: "sidecar_api",
                    endpoint,
                    namespace = caller.namespace,
                    pod = caller.pod,
                    deployment = caller.deployment,
                    session = caller.session,
                    sequence = caller.sequence,
                    bytes = caller.bytes,
                    reason,
                    error = caller.error,
                    result,
                    "sidecar api request"
                )
            };
        }
        if result == "rate_limited" {
            line!(debug);
        } else {
            line!(info);
        }
    }

    /// The per-source rate limit, before anything else is done for the request.
    fn admit(&self, peer: Option<std::net::IpAddr>) -> Result<(), Refusal> {
        if self.rate.allow(peer) {
            Ok(())
        } else {
            Err(RATE_LIMITED)
        }
    }

    /// Authenticates from the headers alone: a cached review, or a `TokenReview` within the
    /// limits.
    async fn authenticate(
        &self,
        headers: &HeaderMap,
        peer: Option<std::net::IpAddr>,
        caller: &mut Caller,
    ) -> Result<Identity, Refusal> {
        let token = bearer(headers).ok_or(UNAUTHENTICATED)?;
        let review = match self.reviews.get(token) {
            Some(review) => review,
            None => {
                if !self.limiter.allow(peer) {
                    caller.error = Some("token review budget spent".to_string());
                    return Err(UNAVAILABLE);
                }
                let review = self.reviewer.review(token).await.map_err(|e| {
                    caller.error = Some(e);
                    UNAVAILABLE
                })?;
                if !review.authenticated {
                    return Err(UNAUTHENTICATED);
                }
                self.reviews.put(token, review.clone());
                review
            }
        };
        let namespace = review
            .username
            .strip_prefix("system:serviceaccount:")
            .and_then(|rest| rest.split_once(':'))
            .map(|(namespace, _)| namespace)
            .filter(|namespace| !namespace.is_empty())
            .ok_or(FORBIDDEN)?;
        caller.namespace = namespace.to_string();
        caller.pod = review.pod.clone().unwrap_or_default();
        Ok(Identity {
            namespace: namespace.to_string(),
            pod: review.pod,
        })
    }

    async fn intermediate(
        &self,
        headers: &HeaderMap,
        peer: Option<std::net::IpAddr>,
        body: Body,
    ) -> Response {
        let mut caller = Caller::default();
        let outcome = self
            .authorize_and_sign(headers, peer, body, &mut caller)
            .await;
        let result = outcome.as_ref().map_or_else(|r| r.result, |_| "ok");
        let reason = outcome.as_ref().err().and_then(|r| r.reason);
        self.record(ENDPOINT, &caller, result, reason);
        match outcome {
            Ok(certificate) => Json(json!({ "certificate": certificate })).into_response(),
            Err(refusal) => refusal.into_response(),
        }
    }

    async fn authorize_and_sign(
        &self,
        headers: &HeaderMap,
        peer: Option<std::net::IpAddr>,
        body: Body,
        caller: &mut Caller,
    ) -> Result<String, Refusal> {
        self.admit(peer)?;
        let identity = self.authenticate(headers, peer, caller).await?;

        let bytes = read_body(body).await?;
        // Parse errors quote the offending value: they are never logged or returned.
        let request: IntermediateRequest =
            serde_json::from_slice(&bytes).map_err(|_| invalid("malformed"))?;
        if !valid_name(&request.deployment) {
            return Err(invalid("deployment"));
        }
        caller.deployment = request.deployment.clone();
        let public_key = base64::engine::general_purpose::STANDARD
            .decode(request.public_key.as_bytes())
            .map_err(|_| invalid("public_key"))?;

        let pod = identity.pod.as_deref().ok_or(FORBIDDEN)?;
        if !pod_of_deployment(pod, &request.deployment) {
            return Err(FORBIDDEN);
        }
        let seed = self.seed.get().ok_or(UNAVAILABLE)?;
        let (namespace, deployment) = (identity.namespace, request.deployment);
        // HKDF and ECDSA are CPU work: not on the threads that serve the fail-closed webhook.
        let signed = tokio::task::spawn_blocking(move || {
            let root = derive_root(&seed, &namespace, &deployment).map_err(Signing::Derive)?;
            root.sign_intermediate(&public_key, SystemTime::now())
                .map_err(|_| Signing::Key)
        })
        .await;
        match signed {
            Ok(Ok(certificate)) => Ok(certificate),
            // The posted key is not one that can be certified.
            Ok(Err(Signing::Key)) => Err(invalid("public_key")),
            Ok(Err(Signing::Derive(e))) => {
                caller.error = Some(e);
                Err(UNAVAILABLE)
            }
            Err(e) => {
                caller.error = Some(format!("signing task: {e}"));
                Err(UNAVAILABLE)
            }
        }
    }

    /// `POST /v1/workloads/<deployment>/report` (spec §7.2).
    async fn report(
        &self,
        deployment: &str,
        headers: &HeaderMap,
        peer: Option<std::net::IpAddr>,
        body: Body,
    ) -> Response {
        let mut caller = Caller::default();
        let outcome = self
            .authorize_and_store(deployment, headers, peer, body, &mut caller)
            .await;
        let result = match &outcome {
            Ok(Outcome::Applied) => "ok",
            Ok(Outcome::Duplicate) => "duplicate",
            Err(refusal) => refusal.result,
        };
        let reason = outcome.as_ref().err().and_then(|r| r.reason);
        self.record(REPORT_ENDPOINT, &caller, result, reason);
        match outcome {
            Ok(outcome) => Json(json!({ "applied": outcome == Outcome::Applied })).into_response(),
            Err(refusal) => refusal.into_response(),
        }
    }

    async fn authorize_and_store(
        &self,
        deployment: &str,
        headers: &HeaderMap,
        peer: Option<std::net::IpAddr>,
        body: Body,
        caller: &mut Caller,
    ) -> Result<Outcome, Refusal> {
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

        // Authorised: only now is the body announced, bounded and read.
        let length = content_length(headers)?;
        caller.bytes = Some(length);
        if length > self.max_report {
            return Err(TOO_LARGE);
        }
        let _reservation = self.budget.reserve(length as u64).ok_or_else(|| {
            caller.error = Some("body budget spent".to_string());
            UNAVAILABLE
        })?;
        let bytes = read_exact(body, length).await?;

        let extra = self.workloads.redact(&identity.namespace, deployment);
        let max_push = self.max_push_endpoints;
        let (seq, report) = tokio::task::spawn_blocking(move || prepare(&bytes, max_push, &extra))
            .await
            .map_err(|e| {
                caller.error = Some(format!("push task: {e}"));
                UNAVAILABLE
            })?;
        if let Some(seq) = &seq {
            caller.session = Some(seq.session.clone());
            caller.sequence = Some(seq.sequence);
        }
        let push = Push {
            pod: pod.to_string(),
            seq,
            report: report?,
        };
        self.store
            .push(&identity.namespace, deployment, push)
            .await
            .map_err(|e| match e {
                StoreError::Name => invalid("name"),
                StoreError::Deployments => DEPLOYMENT_LIMIT,
                StoreError::Unavailable => {
                    caller.error = Some("store unavailable".to_string());
                    UNAVAILABLE
                }
            })
    }
}

/// The envelope of a push, then its report: checked, re-redacted with the built-in rules
/// and the names of the `MimicWorkload`s, and stamped with today (spec §7.3). Nothing of
/// the push is logged before this is done.
fn prepare(
    bytes: &[u8],
    max_push_endpoints: usize,
    extra: &[String],
) -> (Option<SeqId>, Result<Report, Refusal>) {
    let Ok(request) = serde_json::from_slice::<PushRequest>(bytes) else {
        return (None, Err(invalid("malformed")));
    };
    let Ok(seq) = SeqId::parse(&request.session, request.sequence) else {
        let hex = SeqId::parse(&request.session, 1).is_ok();
        return (None, Err(invalid(if hex { "sequence" } else { "session" })));
    };
    let report = Report::parse_checked(request.report.get(), max_push_endpoints).map_err(|v| {
        if v.is_push_limit() {
            Refusal {
                reason: Some(v.reason()),
                ..PUSH_LIMIT
            }
        } else {
            invalid(v.reason())
        }
    });
    let report = report.map(|mut report| {
        report.re_redact(&Redactor::new(extra.iter().map(String::as_str)));
        let today = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() / 86_400);
        report.stamp(today);
        report
    });
    (Some(seq), report)
}

/// The Content-Length of a push, required.
fn content_length(headers: &HeaderMap) -> Result<usize, Refusal> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|value| value.parse().ok())
        .ok_or(invalid("content_length"))
}

/// The token of an `Authorization: Bearer` header, if any.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?.trim();
    (!token.is_empty()).then_some(token)
}

/// A Kubernetes object name: it is logged, so nothing else gets through.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
}

/// Reads the body, refusing more than [`MAX_BODY`] bytes.
async fn read_body(body: Body) -> Result<Vec<u8>, Refusal> {
    let mut stream = body.into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| invalid("body"))?;
        if bytes.len() + chunk.len() > MAX_BODY {
            return Err(TOO_LARGE);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Reads a body of exactly the `length` that was declared.
async fn read_exact(body: Body, length: usize) -> Result<Vec<u8>, Refusal> {
    let mut stream = body.into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| invalid("body"))?;
        if bytes.len() + chunk.len() > length {
            return Err(invalid("content_length"));
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.len() == length {
        Ok(bytes)
    } else {
        Err(invalid("content_length"))
    }
}

#[cfg(test)]
mod push_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{StoreConfig, StoreMetrics};
    use crate::tls_ca::{Seed, derive_root};
    use axum::http::{Method, Request};
    use http_body_util::BodyExt;
    use mimic_runtime::metrics::Metrics;
    use rcgen::{KeyPair, PublicKeyData};
    use serde_json::{Value, json};
    use std::sync::Mutex;
    use std::time::SystemTime;
    use tower::ServiceExt;
    use x509_parser::pem::parse_x509_pem;

    const CANARY: &str = "canary-token-7f3a91";
    const POD: &str = "pay-7d9f8b6c4d-x2k9p";

    pub(super) type Reviews = Arc<Mutex<Vec<Value>>>;

    /// An API server answering `TokenReview` by token: `ok` is the pod `POD` of namespace
    /// `shop`, `other-pod` another deployment's, `no-claim` has no bound pod, `wrong-audience`
    /// is accepted but for another audience, `boom` makes the call fail.
    pub(super) fn api_server() -> (kube::Client, Reviews) {
        let reviews = Reviews::default();
        let seen = reviews.clone();
        let service = tower::service_fn(move |request: Request<kube::client::Body>| {
            let seen = seen.clone();
            async move {
                assert_eq!(
                    request.uri().path(),
                    "/apis/authentication.k8s.io/v1/tokenreviews"
                );
                let bytes = request.into_body().collect_bytes().await.unwrap();
                let review: Value = serde_json::from_slice(&bytes).unwrap();
                seen.lock().unwrap().push(review.clone());
                let token = review["spec"]["token"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let user = |pod: Option<&str>| {
                    let mut user = json!({"username": "system:serviceaccount:shop:default"});
                    if let Some(pod) = pod {
                        user["extra"] = json!({POD_NAME_CLAIM: [pod]});
                    }
                    user
                };
                let (code, status) = match token.as_str() {
                    // `ns|<namespace>|<pod>|<anything>`: a pod of any namespace.
                    t if t.starts_with("ns|") => {
                        let parts: Vec<&str> = t.split('|').collect();
                        let user = json!({
                            "username": format!("system:serviceaccount:{}:default", parts[1]),
                            "extra": {POD_NAME_CLAIM: [parts[2]]},
                        });
                        (
                            200,
                            json!({"authenticated": true, "user": user,
                                   "audiences": [AUDIENCE]}),
                        )
                    }
                    t if t.starts_with("ok") => (
                        200,
                        json!({"authenticated": true, "user": user(Some(POD)),
                               "audiences": [AUDIENCE]}),
                    ),
                    "other-pod" => (
                        200,
                        json!({"authenticated": true,
                               "user": user(Some("web-7d9f8b6c4d-x2k9p")),
                               "audiences": [AUDIENCE]}),
                    ),
                    "no-claim" => (
                        200,
                        json!({"authenticated": true, "user": user(None),
                               "audiences": [AUDIENCE]}),
                    ),
                    "wrong-audience" => (
                        200,
                        json!({"authenticated": true, "user": user(Some(POD)),
                               "audiences": ["https://kubernetes.default.svc"]}),
                    ),
                    "boom" => (
                        500,
                        json!({"kind": "Status", "apiVersion": "v1",
                               "status": "Failure", "message": "boom", "code": 500}),
                    ),
                    _ => (
                        200,
                        json!({"authenticated": false, "error": "invalid bearer token"}),
                    ),
                };
                let body = if code == 200 {
                    json!({"apiVersion": "authentication.k8s.io/v1", "kind": "TokenReview",
                           "metadata": {}, "spec": {}, "status": status})
                } else {
                    status
                };
                Ok::<_, std::convert::Infallible>(
                    axum::http::Response::builder()
                        .status(code)
                        .header("content-type", "application/json")
                        .body(kube::client::Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
            }
        });
        (kube::Client::new(service, "default"), reviews)
    }

    fn seed() -> Arc<SeedSlot> {
        let slot = SeedSlot::new();
        slot.set(Seed::generate(SystemTime::now()));
        Arc::new(slot)
    }

    struct Setup {
        router: Router,
        metrics: Arc<Metrics>,
        reviews: Reviews,
        seed: Arc<SeedSlot>,
    }

    fn setup() -> Setup {
        setup_with(seed())
    }

    fn setup_with(seed: Arc<SeedSlot>) -> Setup {
        setup_limited(seed, ReviewLimiter::default())
    }

    fn setup_limited(seed: Arc<SeedSlot>, limiter: ReviewLimiter) -> Setup {
        let (client, reviews) = api_server();
        let mut metrics = Metrics::new("controller", "test");
        let api = Arc::new(SidecarApi {
            seed: seed.clone(),
            reviewer: Arc::new(KubeReviewer::new(Arc::new(OnceCell::new_with(Some(
                client,
            ))))),
            metrics: ApiMetrics::register(metrics.registry_mut()),
            limiter,
            rate: RateLimiter::new(10_000),
            reviews: ReviewCache::default(),
            budget: BodyBudget::new(1 << 30),
            // These tests do not push: the store is never reached.
            store: Store::new(
                redis::Client::open("redis://127.0.0.1:1").expect("a valid URL"),
                StoreConfig {
                    ttl: std::time::Duration::from_secs(86_400),
                    limits: mimic_core::discovery::report::Limits {
                        max_literals: 32,
                        max_endpoints: 256,
                        sample_budget: 1 << 20,
                    },
                    max_deployments: 50,
                },
                StoreMetrics::register(metrics.registry_mut()),
            ),
            workloads: Arc::new(WorkloadCache::new()),
            max_report: 1 << 20,
            max_push_endpoints: 256,
        });
        Setup {
            router: router(api),
            metrics: Arc::new(metrics),
            reviews,
            seed,
        }
    }

    fn body(deployment: &str, key: &KeyPair) -> Vec<u8> {
        use base64::Engine;
        serde_json::to_vec(&json!({
            "deployment": deployment,
            "public_key": base64::engine::general_purpose::STANDARD
                .encode(key.subject_public_key_info()),
        }))
        .unwrap()
    }

    async fn call(
        router: &Router,
        method: Method,
        token: Option<&str>,
        body: Vec<u8>,
    ) -> (u16, Vec<u8>) {
        call_from(router, None, method, token, body).await
    }

    async fn call_from(
        router: &Router,
        peer: Option<&str>,
        method: Method,
        token: Option<&str>,
        body: Vec<u8>,
    ) -> (u16, Vec<u8>) {
        let mut request = Request::builder().method(method).uri(ENDPOINT);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let mut request = request.body(Body::from(body)).unwrap();
        if let Some(peer) = peer {
            request.extensions_mut().insert(Peer(peer.parse().unwrap()));
        }
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, bytes.to_vec())
    }

    async fn post_status(s: &Setup, token: Option<&str>, body: Vec<u8>) -> u16 {
        call_from(&s.router, Some("10.1.1.1"), Method::POST, token, body)
            .await
            .0
    }

    #[tokio::test]
    async fn tls_09_a_valid_token_gets_an_intermediate_chaining_to_the_deployment_root() {
        let s = setup();
        let key = KeyPair::generate().unwrap();
        let (status, bytes) = call(&s.router, Method::POST, Some("ok"), body("pay", &key)).await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
        let answer: Value = serde_json::from_slice(&bytes).unwrap();
        let pem = answer["certificate"].as_str().unwrap();
        let der = parse_x509_pem(pem.as_bytes()).unwrap().1.contents;
        let (_, inter) = x509_parser::parse_x509_certificate(&der).unwrap();
        assert_eq!(inter.public_key().raw, &key.subject_public_key_info()[..]);
        // Signed by the root of (token namespace, requested deployment), not by another.
        let verifies_under = |ns: &str, dep: &str| {
            let root = derive_root(&s.seed.get().unwrap(), ns, dep).unwrap();
            let der = parse_x509_pem(root.certificate_pem().as_bytes())
                .unwrap()
                .1
                .contents;
            let (_, root) = x509_parser::parse_x509_certificate(&der).unwrap();
            inter.verify_signature(Some(root.public_key())).is_ok()
        };
        assert!(verifies_under("shop", "pay"));
        assert!(!verifies_under("other", "pay"));
        assert!(!verifies_under("shop", "web"));
    }

    #[tokio::test]
    async fn tls_09_the_token_review_asks_for_the_mimic_audience() {
        let s = setup();
        let key = KeyPair::generate().unwrap();
        assert_eq!(post_status(&s, Some("ok"), body("pay", &key)).await, 200);
        let reviews = s.reviews.lock().unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0]["spec"]["token"], "ok");
        assert_eq!(reviews[0]["spec"]["audiences"], json!([AUDIENCE]));
    }

    #[tokio::test]
    async fn tls_09_missing_invalid_and_foreign_audience_tokens_get_401() {
        let s = setup();
        let key = KeyPair::generate().unwrap();
        for token in [None, Some(""), Some("bad"), Some("wrong-audience")] {
            assert_eq!(
                post_status(&s, token, body("pay", &key)).await,
                401,
                "{token:?}"
            );
        }
        // Without a token the API server is not asked.
        let asked = s.reviews.lock().unwrap().len();
        assert_eq!(asked, 2, "bad and wrong-audience only");
        // Not a bearer scheme.
        let request = Request::builder()
            .method(Method::POST)
            .uri(ENDPOINT)
            .header("authorization", "Basic abc")
            .body(Body::from(body("pay", &key)))
            .unwrap();
        let response = s.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), 401);
    }

    #[tokio::test]
    async fn tls_09_a_pod_of_another_deployment_or_without_claim_gets_403() {
        let s = setup();
        let key = KeyPair::generate().unwrap();
        // The token is bound to a `pay` pod.
        assert_eq!(post_status(&s, Some("ok"), body("web", &key)).await, 403);
        assert_eq!(post_status(&s, Some("ok"), body("pa", &key)).await, 403);
        assert_eq!(
            post_status(&s, Some("other-pod"), body("pay", &key)).await,
            403
        );
        assert_eq!(
            post_status(&s, Some("no-claim"), body("pay", &key)).await,
            403
        );
    }

    #[tokio::test]
    async fn tls_09_malformed_bodies_get_400_oversized_413_other_methods_405() {
        let s = setup();
        let key = KeyPair::generate().unwrap();
        let p384 = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
        for bad in [
            b"nope".to_vec(),
            b"{}".to_vec(),
            serde_json::to_vec(&json!({"deployment": "pay", "public_key": "!!!"})).unwrap(),
            serde_json::to_vec(&json!({"deployment": "pay", "public_key": "AAAA"})).unwrap(),
            serde_json::to_vec(&json!({"deployment": "", "public_key": "AAAA"})).unwrap(),
            body("pay", &p384),
        ] {
            assert_eq!(post_status(&s, Some("ok"), bad).await, 400);
        }
        let big = vec![b' '; MAX_BODY + 1];
        assert_eq!(post_status(&s, Some("ok"), big).await, 413);
        for method in [Method::GET, Method::PUT, Method::DELETE] {
            let (status, _) = call(&s.router, method, Some("ok"), body("pay", &key)).await;
            assert_eq!(status, 405);
        }
    }

    #[tokio::test]
    async fn tls_09_a_failing_token_review_or_a_missing_seed_gets_503() {
        let s = setup();
        let key = KeyPair::generate().unwrap();
        assert_eq!(post_status(&s, Some("boom"), body("pay", &key)).await, 503);
        let unseeded = setup_with(Arc::new(SeedSlot::new()));
        assert_eq!(
            post_status(&unseeded, Some("ok"), body("pay", &key)).await,
            503
        );
    }

    /// SPKI of a P-256 key whose point is moved off the curve.
    fn off_curve_body() -> Vec<u8> {
        use base64::Engine;
        let key = KeyPair::generate().unwrap();
        let mut spki = key.subject_public_key_info();
        let last = spki.len() - 1;
        spki[last] ^= 0x01;
        serde_json::to_vec(&json!({
            "deployment": "pay",
            "public_key": base64::engine::general_purpose::STANDARD.encode(spki),
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn tls_28_a_key_that_is_not_a_p256_point_gets_400() {
        let s = setup();
        assert_eq!(post_status(&s, Some("ok"), off_curve_body()).await, 400);
        let key = KeyPair::generate().unwrap();
        assert_eq!(post_status(&s, Some("ok"), body("pay", &key)).await, 200);
    }

    #[tokio::test(start_paused = true)]
    async fn tls_28_token_reviews_are_rate_limited_and_the_budget_refills() {
        let s = setup_limited(seed(), ReviewLimiter::new((2, 1), (100, 100), 16));
        let key = KeyPair::generate().unwrap();
        assert_eq!(post_status(&s, Some("ok"), body("pay", &key)).await, 200);
        assert_eq!(post_status(&s, Some("bad"), body("pay", &key)).await, 401);
        // Budget spent (with another token: the first one is cached): no call to the API server.
        assert_eq!(post_status(&s, Some("ok-2"), body("pay", &key)).await, 503);
        assert_eq!(s.reviews.lock().unwrap().len(), 2);
        // A request without a token never costs a review.
        assert_eq!(post_status(&s, None, body("pay", &key)).await, 401);
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        assert_eq!(post_status(&s, Some("ok-3"), body("pay", &key)).await, 200);
        assert_eq!(s.reviews.lock().unwrap().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn tls_28_one_noisy_source_does_not_get_another_refused() {
        let s = setup_limited(seed(), ReviewLimiter::new((2, 1), (100, 100), 16));
        let key = KeyPair::generate().unwrap();
        let post = |peer: &'static str, token: &'static str| {
            let (router, body) = (s.router.clone(), body("pay", &key));
            async move {
                call_from(&router, Some(peer), Method::POST, Some(token), body)
                    .await
                    .0
            }
        };
        for _ in 0..2 {
            assert_eq!(post("10.0.0.1", "garbage").await, 401);
        }
        assert_eq!(post("10.0.0.1", "garbage").await, 503);
        assert_eq!(post("10.0.0.1", "ok").await, 503);
        // Another pod is served, and the refused calls never reached the API server.
        assert_eq!(post("10.0.0.2", "ok").await, 200);
        assert_eq!(s.reviews.lock().unwrap().len(), 3);
    }

    #[test]
    fn tls_28_the_global_ceiling_and_the_table_of_sources_are_bounded() {
        let limiter = ReviewLimiter::new((10, 1), (3, 1), 2);
        let ip = |n: u8| Some(std::net::IpAddr::from([10, 0, 0, n]));
        assert!(limiter.allow(ip(1)) && limiter.allow(ip(2)) && limiter.allow(ip(3)));
        // Global ceiling reached for every source.
        assert!(!limiter.allow(ip(4)));
        assert_eq!(limiter.sources(), 2);
    }

    #[tokio::test]
    async fn tls_28_a_failing_token_review_writes_one_log_line() {
        let (capture, _guard) = crate::webhook::tests::capture_logs();
        let s = setup();
        let key = KeyPair::generate().unwrap();
        assert_eq!(post_status(&s, Some("boom"), body("pay", &key)).await, 503);
        let raw = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<Value> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect();
        assert_eq!(lines.len(), 1, "{raw}");
        assert_eq!(lines[0]["target"], "sidecar_api");
        assert_eq!(lines[0]["result"], "unavailable");
        assert!(lines[0]["error"].as_str().is_some_and(|e| !e.is_empty()));
    }

    #[tokio::test]
    async fn tls_10_one_log_line_and_one_counter_per_request_and_no_secret() {
        let (capture, _guard) = crate::webhook::tests::capture_logs();
        let s = setup();
        let key = KeyPair::generate().unwrap();
        let key_b64 = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(key.subject_public_key_info())
        };
        assert_eq!(post_status(&s, Some("ok"), body("pay", &key)).await, 200);
        assert_eq!(post_status(&s, Some("ok"), body("web", &key)).await, 403);
        assert_eq!(post_status(&s, Some(CANARY), body("pay", &key)).await, 401);
        assert_eq!(post_status(&s, Some("ok"), b"nope".to_vec()).await, 400);
        assert_eq!(post_status(&s, Some("boom"), body("pay", &key)).await, 503);

        let raw = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        let logs: Vec<Value> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["target"] == "sidecar_api")
            .collect();
        assert_eq!(logs.len(), 5, "{logs:?}");
        let field = |v: &Value, k: &str| v[k].as_str().unwrap_or_default().to_string();
        let line = |i: usize| {
            ["endpoint", "namespace", "pod", "deployment", "result"].map(|k| field(&logs[i], k))
        };
        assert_eq!(line(0), [ENDPOINT, "shop", POD, "pay", "ok"]);
        assert_eq!(line(1), [ENDPOINT, "shop", POD, "web", "forbidden"]);
        assert_eq!(field(&logs[2], "result"), "unauthenticated");
        assert_eq!(field(&logs[3], "result"), "invalid");
        assert_eq!(field(&logs[4], "result"), "unavailable");

        let text = s.metrics.encode();
        for line in [
            r#"mimic_sidecar_api_requests_total{endpoint="/v1/tls/intermediate",result="ok"} 1"#,
            r#"mimic_sidecar_api_requests_total{endpoint="/v1/tls/intermediate",result="forbidden"} 1"#,
            r#"mimic_sidecar_api_requests_total{endpoint="/v1/tls/intermediate",result="unauthenticated"} 1"#,
            r#"mimic_sidecar_api_requests_total{endpoint="/v1/tls/intermediate",result="invalid"} 1"#,
            r#"mimic_sidecar_api_requests_total{endpoint="/v1/tls/intermediate",result="unavailable"} 1"#,
        ] {
            assert!(text.contains(line), "{line}\n{text}");
        }
        for secret in [CANARY, key_b64.as_str()] {
            assert!(!raw.contains(secret), "log leak:\n{raw}");
            assert!(!text.contains(secret), "metric leak:\n{text}");
        }
    }
}
