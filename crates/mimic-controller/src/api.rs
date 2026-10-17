//! API des sidecars (spec §9.2b) : chaque appel est authentifié et limité au pod appelant.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;

use crate::auth::{AuthError, authorize, bearer};
use crate::metrics::ReasonLabel;
use crate::reconcile::PodReport;
use crate::state::{Ctl, valid_name};

/// Un segment ne dépasse pas cette taille.
const MAX_SEGMENT: usize = 64 * 1024 * 1024;

pub fn router(ctl: Arc<Ctl>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/v1/workloads/{ns}/{wl}/segments",
            post(post_segment).get(list_segments),
        )
        .route(
            "/v1/workloads/{ns}/{wl}/segments/{instance}/{file}",
            get(get_segment),
        )
        .route("/v1/workloads/{ns}/{wl}/heartbeat", post(heartbeat))
        .route("/v1/workloads/{ns}/{wl}/snapshots/{id}", get(get_snapshot))
        .layer(DefaultBodyLimit::max(MAX_SEGMENT))
        .with_state(ctl)
}

fn refuse(ctl: &Ctl, e: AuthError) -> Response {
    ctl.metrics
        .auth_failures
        .get_or_create(&ReasonLabel {
            reason: e.reason().into(),
        })
        .inc();
    match e {
        AuthError::Missing | AuthError::Invalid => {
            (StatusCode::UNAUTHORIZED, "jeton absent ou invalide").into_response()
        }
        AuthError::Forbidden => (
            StatusCode::FORBIDDEN,
            "jeton valide pour un autre pod ou workload",
        )
            .into_response(),
        AuthError::Unavailable(msg) => {
            tracing::warn!(error = %msg, "authentification impossible");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "authentification indisponible",
            )
                .into_response()
        }
    }
}

/// Authentifie, valide les noms, puis vérifie que l'appelant agit pour lui-même.
/// La réponse de refus est boîtée : `Response` est trop volumineuse pour un `Err`.
async fn guard(
    ctl: &Ctl,
    headers: &HeaderMap,
    ns: &str,
    wl: &str,
    instance: Option<&str>,
) -> Result<(), Box<Response>> {
    let token = bearer(headers).ok_or_else(|| Box::new(refuse(ctl, AuthError::Missing)))?;
    let id = ctl
        .auth
        .authenticate(token)
        .await
        .map_err(|e| Box::new(refuse(ctl, e)))?;
    let names_ok = valid_name(ns) && valid_name(wl) && instance.is_none_or(valid_name);
    if !names_ok {
        return Err(Box::new(
            (StatusCode::BAD_REQUEST, "nom invalide").into_response(),
        ));
    }
    authorize(&id, ns, wl, instance).map_err(|e| Box::new(refuse(ctl, e)))
}

#[derive(Deserialize)]
struct InstanceQuery {
    instance: String,
}

async fn post_segment(
    State(ctl): State<Arc<Ctl>>,
    Path((ns, wl)): Path<(String, String)>,
    Query(q): Query<InstanceQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(r) = guard(&ctl, &headers, &ns, &wl, Some(&q.instance)).await {
        return *r;
    }
    let result = ctl.ingest(&ns, &wl, &q.instance, &body).await;
    let label = if result.is_ok() { "ok" } else { "error" };
    ctl.metrics
        .segments
        .get_or_create(&crate::metrics::ResultLabel {
            result: label.into(),
        })
        .inc();
    match result {
        Ok(key) => (StatusCode::CREATED, key).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn list_segments(
    State(ctl): State<Arc<Ctl>>,
    Path((ns, wl)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = guard(&ctl, &headers, &ns, &wl, None).await {
        return *r;
    }
    match ctl.segments(&ns, &wl).await {
        Ok(list) => axum::Json(list).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn get_segment(
    State(ctl): State<Arc<Ctl>>,
    Path((ns, wl, instance, file)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = guard(&ctl, &headers, &ns, &wl, None).await {
        return *r;
    }
    if !valid_name(&instance) || !valid_name(&file) {
        return (StatusCode::BAD_REQUEST, "nom invalide").into_response();
    }
    match ctl.segment(&ns, &wl, &instance, &file).await {
        Some(b) => ([(header::CONTENT_TYPE, "application/x-ndjson")], b).into_response(),
        None => (StatusCode::NOT_FOUND, "segment inconnu").into_response(),
    }
}

#[derive(Deserialize)]
struct Heartbeat {
    instance: String,
    mode: String,
    snapshot: Option<String>,
}

async fn heartbeat(
    State(ctl): State<Arc<Ctl>>,
    Path((ns, wl)): Path<(String, String)>,
    headers: HeaderMap,
    axum::Json(hb): axum::Json<Heartbeat>,
) -> Response {
    if let Err(r) = guard(&ctl, &headers, &ns, &wl, Some(&hb.instance)).await {
        return *r;
    }
    let desired = ctl
        .heartbeat(
            &ns,
            &wl,
            &hb.instance,
            PodReport {
                mode: hb.mode,
                snapshot: hb.snapshot,
            },
        )
        .await;
    axum::Json(desired).into_response()
}

async fn get_snapshot(
    State(ctl): State<Arc<Ctl>>,
    Path((ns, wl, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = guard(&ctl, &headers, &ns, &wl, None).await {
        return *r;
    }
    snapshot_response(&ctl, &ns, &wl, &id).await
}

pub async fn snapshot_response(ctl: &Ctl, ns: &str, wl: &str, id: &str) -> Response {
    match ctl.snapshot_bytes(ns, wl, id).await {
        Some(b) => ([(header::CONTENT_TYPE, "application/x-ndjson")], b).into_response(),
        None => (StatusCode::NOT_FOUND, "snapshot inconnu").into_response(),
    }
}
