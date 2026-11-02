//! Read API on `MIMIC_UI_ADDR` (spec §7.5, §7.6): the stored discovery data of a deployment
//! and its OpenAPI documents, on loopback peers only. Reads go through the `store` module.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::extract::{ConnectInfo, MatchedPath, Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use mimic_core::discovery::openapi;
use mimic_core::discovery::report::Report;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::{Registry, Unit};
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::Semaphore;

use crate::store::{ReadError, Store, Summary};

/// Renders at once per replica (spec §7.5).
const MAX_RENDERS: usize = 2;

/// Called on the blocking thread of a render, before it starts (tests hold renders with it).
pub(crate) type Hook = Arc<dyn Fn() + Send + Sync>;

type Labels = Vec<(String, String)>;

fn labels(pairs: &[(&str, &str)]) -> Labels {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn render_histogram() -> Histogram {
    Histogram::new([
        0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
    ])
}

/// `mimic_ui_requests_total{route,code}` and `mimic_ui_render_duration_seconds{route}` (spec
/// §9). `route` is a route pattern or `other`: no label holds request content.
#[derive(Clone)]
pub struct UiMetrics {
    requests: Family<Labels, Counter>,
    renders: Family<Labels, Histogram, fn() -> Histogram>,
}

impl Default for UiMetrics {
    fn default() -> Self {
        UiMetrics {
            requests: Family::default(),
            renders: Family::new_with_constructor(render_histogram),
        }
    }
}

impl UiMetrics {
    pub fn register(registry: &mut Registry) -> UiMetrics {
        let metrics = UiMetrics::default();
        registry.register(
            "ui_requests",
            "Answers of the UI listener",
            metrics.requests.clone(),
        );
        registry.register_with_unit(
            "ui_render_duration",
            "Parse and render of a stored report",
            Unit::Seconds,
            metrics.renders.clone(),
        );
        metrics
    }
}

#[derive(Clone)]
struct Ui {
    store: Arc<Store>,
    metrics: UiMetrics,
    renders: Arc<Semaphore>,
    hook: Option<Hook>,
}

pub fn router(store: Arc<Store>, metrics: UiMetrics) -> Router {
    build(store, metrics, None)
}

#[cfg(test)]
pub(crate) fn router_with_hook(store: Arc<Store>, metrics: UiMetrics, hook: Hook) -> Router {
    build(store, metrics, Some(hook))
}

fn build(store: Arc<Store>, metrics: UiMetrics, hook: Option<Hook>) -> Router {
    let ui = Ui {
        store,
        metrics,
        renders: Arc::new(Semaphore::new(MAX_RENDERS)),
        hook,
    };
    Router::new()
        .route("/", get(|| async { Redirect::permanent("/ui/") }))
        .route("/v1/workloads", get(list))
        .route("/v1/workloads/{namespace}/{deployment}", get(workload))
        .route("/v1/workloads/{namespace}/{deployment}/report", get(report))
        .route(
            "/v1/workloads/{namespace}/{deployment}/endpoints",
            get(endpoints),
        )
        .route(
            "/v1/workloads/{namespace}/{deployment}/openapi",
            get(openapi_index),
        )
        .route(
            "/v1/workloads/{namespace}/{deployment}/openapi/{id}",
            get(openapi_document),
        )
        .layer(middleware::from_fn(loopback_only))
        .layer(middleware::from_fn_with_state(ui.clone(), count))
        .layer(middleware::from_fn(security_headers))
        .with_state(ui)
}

/// The `Content-Security-Policy` of spec §7.5: no inline or evaluated script, and the page can
/// only talk to the listener's own origin.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
                   font-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; \
                   frame-ancestors 'none'";

/// The security headers on every answer, the `403`, `404` and `405` ones included (spec §7.5).
/// A handler's own `Cache-Control` (the versioned assets) is kept. No CORS header is ever added.
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    for (name, value) in [
        (header::CONTENT_SECURITY_POLICY, CSP),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (
            header::HeaderName::from_static("cross-origin-opener-policy"),
            "same-origin",
        ),
        (
            header::HeaderName::from_static("cross-origin-resource-policy"),
            "same-origin",
        ),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    headers
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}

/// Counts every answer, the `403` and `405` ones included, by route pattern.
async fn count(State(ui): State<Ui>, request: Request, next: Next) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "other".to_string(), |path| path.as_str().to_string());
    let response = next.run(request).await;
    let code = response.status().as_u16().to_string();
    ui.metrics
        .requests
        .get_or_create(&labels(&[("route", &route), ("code", &code)]))
        .inc();
    response
}

/// `403` for a peer that is not a loopback address, whatever the bind address, and for a `Host`
/// that is not a loopback name (DNS rebinding through an open port-forward). `HEAD` is served
/// by the `GET` routes; a `405` says `Allow: GET, HEAD` (spec §7.5).
async fn loopback_only(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok());
    if peer.ip().to_canonical().is_loopback() && host.is_some_and(loopback_host) {
        let mut response = next.run(request).await;
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            response
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        }
        response
    } else {
        StatusCode::FORBIDDEN.into_response()
    }
}

/// `localhost`, `127.0.0.1` or `[::1]`, with or without a port (but not with an empty one).
fn loopback_host(host: &str) -> bool {
    let name = match host.rsplit_once(':') {
        Some((name, port))
            if !name.ends_with(':')
                && !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit()) =>
        {
            name
        }
        _ => host,
    };
    name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1" || name == "[::1]"
}

impl IntoResponse for ReadError {
    fn into_response(self) -> Response {
        match self {
            ReadError::Name => StatusCode::BAD_REQUEST,
            ReadError::NotFound => StatusCode::NOT_FOUND,
            ReadError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            ReadError::Unreadable => StatusCode::INTERNAL_SERVER_ERROR,
        }
        .into_response()
    }
}

/// The last millisecond of year 9999, the end of what RFC 3339 can write.
const MAX_MS: u64 = 253_402_300_799_999;

/// RFC 3339 in UTC, to the millisecond; a later instant is shown as [`MAX_MS`].
fn rfc3339(ms: u64) -> String {
    i128::from(ms.min(MAX_MS))
        .checked_mul(1_000_000)
        .and_then(|ns| OffsetDateTime::from_unix_timestamp_nanos(ns).ok())
        .and_then(|at| at.format(&Rfc3339).ok())
        .unwrap_or_default()
}

/// A `serde_json` map is sorted by key.
fn object(summary: &Summary) -> Map<String, Value> {
    let Value::Object(map) = json!({
        "namespace": summary.namespace,
        "deployment": summary.deployment,
        "endpoints": summary.endpoints,
        "exchanges": summary.exchanges,
        "sample_bytes": summary.sample_bytes,
        "last_push": rfc3339(summary.last_push_ms),
    }) else {
        unreachable!("an object literal")
    };
    map
}

async fn list(State(Ui { store, .. }): State<Ui>) -> Result<Json, ReadError> {
    let workloads: Vec<Value> = store
        .list()
        .await?
        .iter()
        .map(|summary| Value::Object(object(summary)))
        .collect();
    Ok(Json(json!({ "workloads": workloads })))
}

async fn workload(
    State(Ui { store, .. }): State<Ui>,
    Path((namespace, deployment)): Path<(String, String)>,
) -> Result<Json, ReadError> {
    let (summary, pods) = store.workload(&namespace, &deployment).await?;
    let mut map = object(&summary);
    let pods: Map<String, Value> = pods
        .into_iter()
        .map(|(pod, ms)| (pod, Value::String(rfc3339(ms))))
        .collect();
    map.insert("pods".to_string(), Value::Object(pods));
    Ok(Json(Value::Object(map)))
}

async fn report(
    State(Ui { store, .. }): State<Ui>,
    Path((namespace, deployment)): Path<(String, String)>,
) -> Result<Response, ReadError> {
    let bytes = store.report(&namespace, &deployment).await?;
    Ok(([(header::CONTENT_TYPE, "application/json")], bytes).into_response())
}

/// Parses the stored report (the check of a push), then runs `build` on it, on a blocking
/// thread, at most [`MAX_RENDERS`] at once; `None` from `build` is a scope the report lacks.
async fn render<F>(
    ui: &Ui,
    route: &str,
    namespace: &str,
    deployment: &str,
    build: F,
) -> Result<Response, ReadError>
where
    F: FnOnce(&Report, u64) -> Option<Value> + Send + 'static,
{
    let failed = || {
        tracing::error!(namespace, deployment, "openapi render failed");
        ReadError::Unreadable
    };
    // Names and the read come first: an invalid or unknown request never waits for a slot.
    let (bytes, version) = ui.store.stored_report(namespace, deployment).await?;
    // The permit moves into the blocking closure: a dropped request leaves the render running,
    // and the slot with it.
    let permit = ui
        .renders
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| failed())?;
    let hook = ui.hook.clone();
    let started = Instant::now();
    let rendered = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if let Some(hook) = hook {
            hook();
        }
        let report = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|json| Report::parse_checked(json, usize::MAX).ok())
            .ok_or(Failure::Report)?;
        let value = build(&report, version).ok_or(Failure::Absent)?;
        serde_json::to_vec(&value).map_err(|_| Failure::Render)
    })
    .await;
    ui.metrics
        .renders
        .get_or_create(&labels(&[("route", route)]))
        .observe(started.elapsed().as_secs_f64());
    match rendered {
        Ok(Ok(body)) => Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response()),
        Ok(Err(Failure::Absent)) => Err(ReadError::NotFound),
        Ok(Err(Failure::Report)) => {
            tracing::error!(namespace, deployment, "stored report unreadable");
            Err(ReadError::Unreadable)
        }
        Ok(Err(Failure::Render)) | Err(_) => Err(failed()),
    }
}

/// Why a render stopped, logged by the caller (the blocking thread logs nothing).
enum Failure {
    /// The stored report does not parse or fails the structural check.
    Report,
    /// The report holds no such scope.
    Absent,
    /// The document does not serialise.
    Render,
}

async fn endpoints(
    State(ui): State<Ui>,
    matched: MatchedPath,
    Path((namespace, deployment)): Path<(String, String)>,
) -> Result<Response, ReadError> {
    render(
        &ui,
        matched.as_str(),
        &namespace,
        &deployment,
        |report, _| Some(openapi::endpoint_list(report)),
    )
    .await
}

async fn openapi_index(
    State(ui): State<Ui>,
    matched: MatchedPath,
    Path((namespace, deployment)): Path<(String, String)>,
) -> Result<Response, ReadError> {
    render(
        &ui,
        matched.as_str(),
        &namespace,
        &deployment,
        |report, _| Some(openapi::index(report)),
    )
    .await
}

async fn openapi_document(
    State(ui): State<Ui>,
    matched: MatchedPath,
    Path((namespace, deployment, id)): Path<(String, String, String)>,
) -> Result<Response, ReadError> {
    if openapi::parse_id(&id).is_none() {
        return Err(ReadError::Name);
    }
    let (ns, dep) = (namespace.clone(), deployment.clone());
    render(
        &ui,
        matched.as_str(),
        &namespace,
        &deployment,
        move |report, version| openapi::document(report, &ns, &dep, version, &id),
    )
    .await
}

struct Json(Value);

impl IntoResponse for Json {
    fn into_response(self) -> Response {
        let body = serde_json::to_vec(&self.0).unwrap_or_default();
        ([(header::CONTENT_TYPE, "application/json")], body).into_response()
    }
}

#[cfg(test)]
mod tests;
