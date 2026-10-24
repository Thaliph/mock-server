//! API des sidecars (spec §9.2b) : chaque appel est authentifié et limité au pod appelant.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, FromRequest, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use mimic_core::unmatched::{MAX_REPORTED, UnmatchedReport};
use serde::Deserialize;

use crate::auth::{AuthError, authorize, bearer};
use crate::config::Identity;
use crate::metrics::ReasonLabel;
use crate::mock_form::{self, Encoding};
use crate::reconcile::PodReport;
use crate::state::{Ctl, IngestError, valid_name};

/// Un segment ne dépasse pas cette taille.
const MAX_SEGMENT: usize = 64 * 1024 * 1024;
/// Un battement ne dépasse pas cette taille.
const MAX_HEARTBEAT: usize = 64 * 1024;

/// Les corps sont lus après `guard` : un appelant non authentifié ne fait rien lire ni
/// garder en mémoire (spec §9.2b).
pub fn router(ctl: Arc<Ctl>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        // Collecte Prometheus, sans jeton comme `/healthz` (spec §9.2b).
        .route("/metrics", get(crate::ui::metrics))
        .route(
            "/v1/workloads/{ns}/{wl}/segments",
            post(post_segment).get(list_segments),
        )
        .route(
            "/v1/workloads/{ns}/{wl}/segments/{instance}/{file}",
            get(get_segment),
        )
        .route(
            "/v1/workloads/{ns}/{wl}/heartbeat",
            post(heartbeat).layer(DefaultBodyLimit::max(MAX_HEARTBEAT)),
        )
        .route("/v1/workloads/{ns}/{wl}/snapshots/{id}", get(get_snapshot))
        .route(
            "/v1/workloads/{ns}/{wl}/snapshots/{id}/mock",
            get(get_mock_snapshot),
        )
        .layer(DefaultBodyLimit::max(MAX_SEGMENT))
        .layer(axum::middleware::from_fn_with_state(
            ctl.clone(),
            crate::leader::passive_guard,
        ))
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
) -> Result<Identity, Box<Response>> {
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
    authorize(&id, ns, wl, instance).map_err(|e| Box::new(refuse(ctl, e)))?;
    Ok(id)
}

#[derive(Deserialize)]
struct SegmentQuery {
    instance: String,
    /// Identifiant choisi par le sidecar : un renvoi du même segment est sans effet.
    segment: Option<String>,
}

/// Corps de la requête, lu jusqu'à la borne de la route (`DefaultBodyLimit`) : `413` au-delà.
/// Refus boîté, comme pour `guard`.
async fn body_of(req: Request) -> Result<Bytes, Box<Response>> {
    Bytes::from_request(req, &())
        .await
        .map_err(|e| Box::new(e.into_response()))
}

async fn post_segment(
    State(ctl): State<Arc<Ctl>>,
    Path((ns, wl)): Path<(String, String)>,
    Query(q): Query<SegmentQuery>,
    req: Request,
) -> Response {
    if let Err(r) = guard(&ctl, req.headers(), &ns, &wl, Some(&q.instance)).await {
        return *r;
    }
    let body = match body_of(req).await {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let result = ctl
        .ingest(&ns, &wl, &q.instance, q.segment.as_deref(), body)
        .await;
    let label = match &result {
        Ok(i) if i.duplicate => "duplicate",
        Ok(_) => "ok",
        Err(_) => "error",
    };
    ctl.metrics
        .segments
        .get_or_create(&crate::metrics::ResultLabel {
            result: label.into(),
        })
        .inc();
    match result {
        Ok(i) if i.duplicate => (StatusCode::OK, i.key).into_response(),
        Ok(i) => (StatusCode::CREATED, i.key).into_response(),
        Err(IngestError::Invalid(e)) => (StatusCode::BAD_REQUEST, e).into_response(),
        Err(IngestError::Storage(e)) => {
            tracing::warn!(error = %e, "ingestion d'un segment");
            (StatusCode::SERVICE_UNAVAILABLE, "support indisponible").into_response()
        }
    }
}

#[derive(Deserialize)]
struct SegmentsQuery {
    /// Seuls les segments datés d'au moins cette valeur (ms Unix) sont listés.
    since_ms: Option<i64>,
}

async fn list_segments(
    State(ctl): State<Arc<Ctl>>,
    Path((ns, wl)): Path<(String, String)>,
    Query(q): Query<SegmentsQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = guard(&ctl, &headers, &ns, &wl, None).await {
        return *r;
    }
    match ctl.segments(&ns, &wl, q.since_ms).await {
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
    #[serde(default)]
    capture_inbound: Option<String>,
    #[serde(default)]
    mock_websocket: Option<String>,
    /// Facultatif (sidecar d'une version antérieure) ; illisible, il est ignoré plutôt que de
    /// faire refuser le battement.
    #[serde(default, deserialize_with = "lenient")]
    unmatched: Option<UnmatchedReport>,
}

fn lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<UnmatchedReport>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v).ok())
}

/// Attente maximale du sel du workload par un battement.
const SALT_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

async fn heartbeat(
    State(ctl): State<Arc<Ctl>>,
    Path((ns, wl)): Path<(String, String)>,
    req: Request,
) -> Response {
    // L'instance est dans le corps : le jeton et le workload sont vérifiés avant de le lire,
    // l'instance après.
    let id = match guard(&ctl, req.headers(), &ns, &wl, None).await {
        Ok(id) => id,
        Err(r) => return *r,
    };
    let body = match body_of(req).await {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let hb: Heartbeat = match serde_json::from_slice(&body) {
        Ok(hb) => hb,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("battement invalide: {e}")).into_response();
        }
    };
    if !valid_name(&hb.instance) {
        return (StatusCode::BAD_REQUEST, "nom invalide").into_response();
    }
    if let Err(e) = authorize(&id, &ns, &wl, Some(&hb.instance)) {
        return refuse(&ctl, e);
    }
    let desired = match ctl
        .heartbeat(
            &ns,
            &wl,
            &hb.instance,
            PodReport {
                mode: hb.mode,
                snapshot: hb.snapshot,
                capture_inbound: hb.capture_inbound,
                mock_websocket: hb.mock_websocket,
                // Borné quoi qu'annonce le sidecar (spec §9.2b).
                unmatched: hb.unmatched.map(|u| u.truncated(MAX_REPORTED)),
            },
            id.labels,
        )
        .await
    {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(namespace = %ns, workload = %wl, error = %e, "état souhaité illisible sur le support");
            return (StatusCode::SERVICE_UNAVAILABLE, "support indisponible").into_response();
        }
    };
    // Sel du workload (spec §7) : à côté de l'état souhaité, pour ses seuls pods. Support en
    // échec : le battement répond sans, le sidecar garde le sien ou n'enregistre pas.
    // Le battement n'attend jamais le support plus de `SALT_WAIT` (CTL-08) : la lecture se
    // poursuit en tâche de fond, un battement suivant portera le sel.
    let mut reply = serde_json::to_value(&desired).unwrap_or_default();
    let read = {
        let (ctl, ns, wl) = (ctl.clone(), ns.clone(), wl.clone());
        tokio::spawn(async move { ctl.workload_salt(&ns, &wl).await })
    };
    match tokio::time::timeout(SALT_WAIT, read).await {
        Ok(Ok(Ok(salt))) => reply["redact_salt"] = serde_json::Value::String(salt),
        Ok(Ok(Err(e))) => {
            tracing::warn!(namespace = %ns, workload = %wl, error = %e, "sel du workload illisible sur le support");
        }
        Ok(Err(_)) | Err(_) => {}
    }
    axum::Json(reply).into_response()
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

/// Forme de mock d'un snapshot (spec §5.6) : ce que télécharge un sidecar à jour, compressée
/// selon `Accept-Encoding`, avec un `ETag`.
async fn get_mock_snapshot(
    State(ctl): State<Arc<Ctl>>,
    Path((ns, wl, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = guard(&ctl, &headers, &ns, &wl, None).await {
        return *r;
    }
    let Some(form) = ctl.mock_snapshot(&ns, &wl, &id).await else {
        return (StatusCode::NOT_FOUND, "snapshot inconnu").into_response();
    };
    let text = |name| headers.get(name).and_then(|v| v.to_str().ok());
    let etag = format!("\"{}\"", form.etag);
    let out = Response::builder()
        .header(header::ETAG, &etag)
        .header(
            header::CACHE_CONTROL,
            "private, max-age=31536000, immutable",
        )
        .header(header::VARY, "accept-encoding");
    if mock_form::not_modified(text(header::IF_NONE_MATCH), &etag) {
        return out
            .status(StatusCode::NOT_MODIFIED)
            .body(axum::body::Body::empty())
            .unwrap_or_default();
    }
    let encoding = mock_form::pick(text(header::ACCEPT_ENCODING));
    let body = match encoding {
        Encoding::Zstd => form.zstd.clone(),
        Encoding::Gzip => form.gzip.clone(),
        // Sans compression acceptée : décompressée hors des threads du serveur.
        Encoding::Identity => match tokio::task::spawn_blocking(move || form.identity()).await {
            Ok(Ok(b)) => Bytes::from(b),
            _ => {
                return (StatusCode::INTERNAL_SERVER_ERROR, "forme de mock illisible")
                    .into_response();
            }
        },
    };
    let mut out = out.header(header::CONTENT_TYPE, "application/json");
    if let Some(e) = encoding.header() {
        out = out.header(header::CONTENT_ENCODING, e);
    }
    out.body(axum::body::Body::from(body)).unwrap_or_default()
}

pub async fn snapshot_response(ctl: &Ctl, ns: &str, wl: &str, id: &str) -> Response {
    match ctl.snapshot_bytes(ns, wl, id).await {
        Some(b) => ([(header::CONTENT_TYPE, "application/x-ndjson")], b).into_response(),
        None => (StatusCode::NOT_FOUND, "snapshot inconnu").into_response(),
    }
}
