//! Read API on `MIMIC_UI_ADDR` (spec §7.5): the stored discovery data of a deployment, on
//! loopback peers only. Reads go through the `store` module.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::store::{ReadError, Store, Summary};

pub fn router(store: Arc<Store>) -> Router {
    Router::new()
        .route("/v1/workloads", get(list))
        .route("/v1/workloads/{namespace}/{deployment}", get(workload))
        .route("/v1/workloads/{namespace}/{deployment}/report", get(report))
        .layer(middleware::from_fn(loopback_only))
        .with_state(store)
}

/// `403` for a peer that is not a loopback address, whatever the bind address, and for a `Host`
/// that is not a loopback name (DNS rebinding through an open port-forward).
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
        next.run(request).await
    } else {
        StatusCode::FORBIDDEN.into_response()
    }
}

/// `localhost`, `127.0.0.1` or `[::1]`, with or without a port.
fn loopback_host(host: &str) -> bool {
    let name = match host.rsplit_once(':') {
        Some((name, port)) if !name.ends_with(':') && port.bytes().all(|b| b.is_ascii_digit()) => {
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

/// RFC 3339 in UTC, to the millisecond.
fn rfc3339(ms: u64) -> String {
    i128::from(ms)
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

async fn list(State(store): State<Arc<Store>>) -> Result<Json, ReadError> {
    let workloads: Vec<Value> = store
        .list()
        .await?
        .iter()
        .map(|summary| Value::Object(object(summary)))
        .collect();
    Ok(Json(json!({ "workloads": workloads })))
}

async fn workload(
    State(store): State<Arc<Store>>,
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
    State(store): State<Arc<Store>>,
    Path((namespace, deployment)): Path<(String, String)>,
) -> Result<Response, ReadError> {
    let bytes = store.report(&namespace, &deployment).await?;
    Ok(([(header::CONTENT_TYPE, "application/json")], bytes).into_response())
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
