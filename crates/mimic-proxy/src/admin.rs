//! API et page d'admin (spec §9.2).

use std::fmt::Write as _;
use std::sync::{Arc, PoisonError};

use bytes::Bytes;
use http::{Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use mimic_core::model::Direction;
use serde::Serialize;
use serde_json::{Value, json};

use mimic_core::catalog::{Operation, STYLE, esc};

use crate::config::Mode;
use crate::proxy::{boxed, parse_query};
use crate::state::{AppState, Discovery, ProxyBody};

pub async fn handle(state: Arc<AppState>, req: Request<Incoming>) -> Response<ProxyBody> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = parse_query(req.uri().query());
    match (&method, path.as_str()) {
        (&Method::GET, "/healthz") | (&Method::GET, "/readyz") => text(StatusCode::OK, "ok"),
        (&Method::GET, "/") => {
            let html = render_page(&state);
            respond(StatusCode::OK, "text/html; charset=utf-8", html)
        }
        (&Method::GET, "/api/operations") => json_response(&operations(&state.discovery())),
        (&Method::GET, "/api/status") => json_response(&status(&state)),
        (&Method::GET, "/openapi/inbound") => json_response(&state.discovery().inbound),
        (&Method::GET, "/openapi/outbound") => {
            let d = state.discovery();
            let list: Vec<&String> = d.outbound.keys().collect();
            json_response(&json!(list))
        }
        (&Method::GET, p) if p.starts_with("/openapi/outbound/") => {
            let authority = &p["/openapi/outbound/".len()..];
            match state.discovery().outbound.get(authority) {
                Some(doc) => json_response(doc),
                None => text(StatusCode::NOT_FOUND, "dépendance inconnue"),
            }
        }
        (&Method::GET, "/mode") => text(StatusCode::OK, state.mode().as_str()),
        (&Method::PUT, "/mode") | (&Method::DELETE, "/store") if state.config.controlled() => text(
            StatusCode::FORBIDDEN,
            "sidecar piloté par le contrôleur : basculer via la ressource MimicWorkload",
        ),
        (&Method::PUT, "/mode") => {
            let body = match Limited::new(req.into_body(), 1024).collect().await {
                Ok(b) => b.to_bytes(),
                Err(_) => return text(StatusCode::PAYLOAD_TOO_LARGE, "body trop gros"),
            };
            let raw = String::from_utf8_lossy(&body);
            let wanted = serde_json::from_str::<Value>(&raw)
                .ok()
                .and_then(|v| v["mode"].as_str().map(String::from))
                .unwrap_or_else(|| raw.trim().to_string());
            match Mode::parse(&wanted) {
                Some(m) => {
                    state.set_mode(m);
                    text(StatusCode::OK, m.as_str())
                }
                None => text(
                    StatusCode::BAD_REQUEST,
                    "mode inconnu (passthrough, record, mock, mock-partial)",
                ),
            }
        }
        (&Method::GET, "/metrics") => {
            state.refresh_store_metrics();
            respond(
                StatusCode::OK,
                "application/openmetrics-text; version=1.0.0; charset=utf-8",
                state.metrics.encode(),
            )
        }
        (&Method::GET, "/exchanges") => {
            let get = |k: &str| query.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
            let direction = match get("direction") {
                Some("inbound") => Some(Direction::Inbound),
                Some("outbound") => Some(Direction::Outbound),
                Some(_) => return text(StatusCode::BAD_REQUEST, "direction: inbound ou outbound"),
                None => None,
            };
            let limit = get("limit")
                .and_then(|l| l.parse().ok())
                .unwrap_or(100usize)
                .min(10_000);
            json_response(&state.store.recent(direction, limit))
        }
        (&Method::DELETE, "/store") => {
            state.store.clear();
            text(StatusCode::OK, "store purgé")
        }
        (&Method::POST, "/share/sync") => match crate::share::trigger(&state) {
            true => text(StatusCode::ACCEPTED, "synchronisation demandée"),
            false => text(StatusCode::NOT_FOUND, "partage désactivé"),
        },
        _ => text(StatusCode::NOT_FOUND, "introuvable"),
    }
}

fn respond(status: StatusCode, content_type: &'static str, body: String) -> Response<ProxyBody> {
    let mut r = Response::new(boxed(http_body_util::Full::new(Bytes::from(body))));
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(content_type),
    );
    r
}

fn text(status: StatusCode, body: &str) -> Response<ProxyBody> {
    respond(status, "text/plain; charset=utf-8", body.to_string())
}

fn json_response<T: Serialize>(v: &T) -> Response<ProxyBody> {
    match serde_json::to_string(v) {
        Ok(s) => respond(StatusCode::OK, "application/json", s),
        Err(e) => text(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

fn status(state: &AppState) -> Value {
    let (local, peer) = state.store.counts();
    json!({
        "mode": state.mode().as_str(),
        "capture_inbound": state.inbound_capture().as_str(),
        "mock_websocket": state.mock_websocket().as_str(),
        "instance": state.config.instance,
        "workload": {
            "namespace": state.config.workload.namespace,
            "name": state.config.workload.name,
        },
        "store": {"local": local, "peer": peer},
        "share": *state.share_status.lock().unwrap_or_else(PoisonError::into_inner),
        "snapshot": state.snapshot().map(|s| s.id.clone()),
        "controller": state.config.controlled().then(|| {
            state.controller_status.lock().unwrap_or_else(PoisonError::into_inner).clone()
        }),
    })
}

/// Catalogue des opérations de la découverte courante.
pub fn operations(d: &Discovery) -> Vec<Operation> {
    mimic_core::catalog::operations(&d.inbound, &d.outbound)
}

fn render_page(state: &AppState) -> String {
    let d = state.discovery();
    let ops = operations(&d);
    let share = state
        .share_status
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let (local, peer) = state.store.counts();
    let mut h = String::new();
    let _ = write!(
        h,
        "<!doctype html><html lang=\"fr\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>mimic · {}</title><style>{STYLE}</style></head><body><main>",
        esc(&state.config.workload.name)
    );
    let _ = write!(
        h,
        "<h1>API découvertes · {}/{}</h1><div class=\"meta\"><span>mode <span class=\"pill mode\">{}</span></span><span>instance {}</span>",
        esc(&state.config.workload.namespace),
        esc(&state.config.workload.name),
        state.mode().as_str(),
        esc(&state.config.instance),
    );
    if state.config.controlled() {
        // Sous contrôleur, rien n'est importé des pairs (spec §5.4) : la page ne montre que ce
        // pod, le workload entier est dans l'interface du contrôleur.
        let _ = write!(
            h,
            "<span>{local} échanges vus par ce pod</span><span>piloté par le contrôleur · snapshot {}</span><span>publication vers le contrôleur{} · workload entier : interface du contrôleur</span>",
            esc(&state
                .snapshot()
                .map(|s| s.id.clone())
                .unwrap_or_else(|| "aucun".into())),
            share
                .last_error
                .as_ref()
                .map(|e| format!(", dernière erreur : {}", esc(e)))
                .unwrap_or_default()
        );
    } else {
        let _ = write!(h, "<span>{local} échanges locaux, {peer} importés</span>");
    }
    if share.enabled && !state.config.controlled() {
        let _ = write!(
            h,
            "<span>partage : {} pair(s){}</span>",
            share.peers.len(),
            share
                .last_error
                .as_ref()
                .map(|e| format!(", dernière erreur : {}", esc(e)))
                .unwrap_or_default()
        );
    }
    h.push_str("<span><a href=\"/openapi/inbound\">OpenAPI du service</a></span><span><a href=\"/metrics\">métriques</a></span></div>");

    mimic_core::catalog::render_operations(&mut h, &ops, &|a| format!("/openapi/outbound/{a}"));
    h.push_str("</main></body></html>");
    h
}
