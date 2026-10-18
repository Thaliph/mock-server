//! Interface en lecture seule (spec §5.5 et §9.2b), joignable par `kubectl port-forward`.

use std::fmt::Write as _;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use mimic_core::catalog::{STYLE, esc, operations, render_operations};
use mimic_core::model::Direction;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::{Ctl, now_ms, valid_name};

pub fn router(ctl: Arc<Ctl>) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/healthz", get(|| async { "ok" }))
        .route("/metrics", get(metrics))
        .route("/ui/{ns}/{wl}", get(page))
        .route("/v1/workloads", get(workloads))
        .route("/v1/workloads/{ns}/{wl}/mode", get(mode))
        .route("/v1/workloads/{ns}/{wl}/snapshots", get(snapshots))
        .route("/v1/workloads/{ns}/{wl}/snapshots/{id}", get(snapshot))
        .route("/v1/workloads/{ns}/{wl}/operations", get(ops))
        .route(
            "/v1/workloads/{ns}/{wl}/openapi/inbound",
            get(openapi_inbound),
        )
        .route(
            "/v1/workloads/{ns}/{wl}/openapi/outbound/{authority}",
            get(openapi_outbound),
        )
        .route("/v1/workloads/{ns}/{wl}/requests", get(requests))
        .layer(axum::middleware::from_fn_with_state(
            ctl.clone(),
            crate::leader::passive_guard,
        ))
        .with_state(ctl)
}

type Wl = Path<(String, String)>;

fn bad_name() -> Response {
    (StatusCode::BAD_REQUEST, "nom invalide").into_response()
}

async fn metrics(State(ctl): State<Arc<Ctl>>) -> Response {
    // Une passive ne touche pas au support : ses jauges restent à zéro (spec §5.5.4).
    if ctl.is_active() {
        ctl.refresh_metrics().await;
    }
    (
        [(
            header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        ctl.metrics.encode(),
    )
        .into_response()
}

async fn workload_json(ctl: &Ctl, ns: &str, wl: &str) -> Value {
    ctl.with_live(ns, wl, |w| {
        let summary = ctl.summary(w);
        let pods: Vec<Value> = ctl
            .pod_states(w)
            .into_iter()
            .map(|(p, s, selected)| {
                json!({
                    "instance": p.instance,
                    "mode": p.report.mode,
                    "snapshot": p.report.snapshot,
                    "capture_inbound": p.report.capture_inbound,
                    "last_seen_ms": p.last_seen_ms,
                    "state": s.as_str(),
                    "selected": selected,
                })
            })
            .collect();
        json!({
            "namespace": ns,
            "name": wl,
            "spec": w.spec,
            "desired": w.desired,
            "pods": pods,
            "selected_pods": summary.pods,
            "aligned": summary.aligned,
            "ready": summary.ready,
            "reason": summary.reason,
        })
    })
    .await
}

async fn workloads(State(ctl): State<Arc<Ctl>>) -> Response {
    let mut out = vec![];
    for (ns, wl) in ctl.workload_keys().await {
        out.push(workload_json(&ctl, &ns, &wl).await);
    }
    axum::Json(out).into_response()
}

async fn mode(State(ctl): State<Arc<Ctl>>, Path((ns, wl)): Wl) -> Response {
    if !valid_name(&ns) || !valid_name(&wl) {
        return bad_name();
    }
    axum::Json(ctl.with_live(&ns, &wl, |w| w.desired.clone()).await).into_response()
}

async fn snapshots(State(ctl): State<Arc<Ctl>>, Path((ns, wl)): Wl) -> Response {
    if !valid_name(&ns) || !valid_name(&wl) {
        return bad_name();
    }
    axum::Json(ctl.snapshots(&ns, &wl).await).into_response()
}

async fn snapshot(
    State(ctl): State<Arc<Ctl>>,
    Path((ns, wl, id)): Path<(String, String, String)>,
) -> Response {
    if !valid_name(&ns) || !valid_name(&wl) {
        return bad_name();
    }
    crate::api::snapshot_response(&ctl, &ns, &wl, &id).await
}

async fn ops(State(ctl): State<Arc<Ctl>>, Path((ns, wl)): Wl) -> Response {
    if !valid_name(&ns) || !valid_name(&wl) {
        return bad_name();
    }
    let d = ctl.discovery(&ns, &wl).await;
    axum::Json(operations(&d.inbound, &d.outbound)).into_response()
}

async fn openapi_inbound(State(ctl): State<Arc<Ctl>>, Path((ns, wl)): Wl) -> Response {
    if !valid_name(&ns) || !valid_name(&wl) {
        return bad_name();
    }
    axum::Json(ctl.discovery(&ns, &wl).await.inbound.clone()).into_response()
}

async fn openapi_outbound(
    State(ctl): State<Arc<Ctl>>,
    Path((ns, wl, authority)): Path<(String, String, String)>,
) -> Response {
    if !valid_name(&ns) || !valid_name(&wl) {
        return bad_name();
    }
    match ctl.discovery(&ns, &wl).await.outbound.get(&authority) {
        Some(doc) => axum::Json(doc.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, "dépendance inconnue").into_response(),
    }
}

#[derive(Deserialize)]
struct RequestsQuery {
    direction: Option<String>,
    limit: Option<usize>,
    format: Option<String>,
}

async fn requests(
    State(ctl): State<Arc<Ctl>>,
    Path((ns, wl)): Wl,
    Query(q): Query<RequestsQuery>,
) -> Response {
    if !valid_name(&ns) || !valid_name(&wl) {
        return bad_name();
    }
    let direction = match q.direction.as_deref().unwrap_or("inbound") {
        "inbound" => Direction::Inbound,
        "outbound" => Direction::Outbound,
        _ => return (StatusCode::BAD_REQUEST, "direction: inbound ou outbound").into_response(),
    };
    let limit = q.limit.unwrap_or(1_000).min(100_000);
    let format = q.format.unwrap_or_else(|| "json".into());
    if !["json", "jsonl", "k6"].contains(&format.as_str()) {
        return (StatusCode::BAD_REQUEST, "format: json, jsonl ou k6").into_response();
    }
    // Jusqu'à 100 000 échanges triés et mis en forme hors des threads du serveur, sans copie.
    ctl.with_requests(&ns, &wl, direction, limit, move |list| {
        match format.as_str() {
            "json" => {
                let out: Vec<Value> = list
                    .iter()
                    .map(|t| {
                        let mut v = serde_json::to_value(t.1).unwrap_or_default();
                        v["instance"] = json!(t.0);
                        v
                    })
                    .collect();
                axum::Json(out).into_response()
            }
            "jsonl" => {
                let mut body = String::new();
                for t in &list {
                    if let Ok(line) = serde_json::to_string(t.1) {
                        body.push_str(&line);
                        body.push('\n');
                    }
                }
                ([(header::CONTENT_TYPE, "application/x-ndjson")], body).into_response()
            }
            _ => {
                let exchanges: Vec<&mimic_core::model::Exchange> =
                    list.iter().map(|t| t.1).collect();
                (
                    [(
                        header::CONTENT_TYPE,
                        "application/javascript; charset=utf-8",
                    )],
                    mimic_core::replay::k6_script(&exchanges),
                )
                    .into_response()
            }
        }
    })
    .await
    .unwrap_or_else(|| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn head(h: &mut String, title: &str) {
    let _ = write!(
        h,
        "<!doctype html><html lang=\"fr\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>mimic · {}</title><style>{STYLE}</style></head><body><main>",
        esc(title)
    );
}

async fn home(State(ctl): State<Arc<Ctl>>) -> Html<String> {
    let mut h = String::new();
    head(&mut h, "contrôleur");
    h.push_str("<h1>Workloads observés</h1><div class=\"meta\"><span><a href=\"/metrics\">métriques</a></span></div>");
    let keys = ctl.workload_keys().await;
    if keys.is_empty() {
        h.push_str("<p class=\"empty\">Aucun sidecar ne s'est encore annoncé.</p>");
    } else {
        h.push_str("<table><tr><th>Workload</th><th>Mode souhaité</th><th>Pods alignés</th><th>Échanges</th></tr>");
        for (ns, wl) in keys {
            let (mode, s) = ctl
                .with_live(&ns, &wl, |w| (w.desired.mode.clone(), ctl.summary(w)))
                .await;
            let _ = write!(
                h,
                "<tr><td><a href=\"/ui/{0}/{1}\">{0}/{1}</a></td><td>{2}</td><td>{3}/{4}</td><td>{5}</td></tr>",
                esc(&ns),
                esc(&wl),
                esc(mode.as_deref().unwrap_or("libre")),
                s.aligned,
                s.pods,
                ctl.exchange_count(&ns, &wl)
            );
        }
        h.push_str("</table>");
    }
    h.push_str("</main></body></html>");
    Html(h)
}

async fn page(State(ctl): State<Arc<Ctl>>, Path((ns, wl)): Wl) -> Response {
    if !valid_name(&ns) || !valid_name(&wl) {
        return bad_name();
    }
    let d = ctl.discovery(&ns, &wl).await;
    let ops = operations(&d.inbound, &d.outbound);
    let (summary, pods, desired, has_spec) = ctl
        .with_live(&ns, &wl, |w| {
            (
                ctl.summary(w),
                ctl.pod_states(w),
                w.desired.clone(),
                w.spec.is_some(),
            )
        })
        .await;
    let recent = ctl.requests(&ns, &wl, Direction::Inbound, 50).await;
    let snaps = ctl.snapshots(&ns, &wl).await;
    let api = format!("/v1/workloads/{ns}/{wl}");

    let mut h = String::new();
    head(&mut h, &wl);
    let _ = write!(
        h,
        "<h1>{} / {}</h1><div class=\"meta\"><span>mode souhaité <span class=\"pill mode\">{}</span></span><span>snapshot {}</span><span>{} / {} pods alignés{}</span><span><a href=\"{api}/openapi/inbound\">OpenAPI du service</a></span></div>",
        esc(&ns),
        esc(&wl),
        esc(desired.mode.as_deref().unwrap_or("libre")),
        esc(desired.snapshot.as_deref().unwrap_or("aucun")),
        summary.aligned,
        summary.pods,
        summary
            .reason
            .as_ref()
            .map(|r| format!(" · {}", esc(r)))
            .unwrap_or_default()
    );

    h.push_str("<h2>Bascule</h2><p>La bascule passe par la ressource <code>MimicWorkload</code>, soumise au RBAC Kubernetes :</p>");
    for m in ["mock", "record"] {
        let _ = write!(
            h,
            "<pre>kubectl patch mimicworkload {0} -n {1} --type merge -p '{{\"spec\":{{\"mode\":\"{2}\"}}}}'</pre>",
            esc(&wl),
            esc(&ns),
            m
        );
    }
    if !has_spec {
        let _ = write!(
            h,
            "<p class=\"empty\">Aucune ressource MimicWorkload pour ce workload : créer d'abord <code>kubectl apply -f - &lt;&lt;&lt; '{{\"apiVersion\":\"mimic.platform.io/v1alpha1\",\"kind\":\"MimicWorkload\",\"metadata\":{{\"name\":\"{}\",\"namespace\":\"{}\"}},\"spec\":{{\"mode\":\"record\"}}}}'</code></p>",
            esc(&wl),
            esc(&ns)
        );
    }

    h.push_str("<h2>Pods</h2>");
    if pods.is_empty() {
        h.push_str("<p class=\"empty\">Aucun battement reçu.</p>");
    } else {
        h.push_str("<table><tr><th>Instance</th><th>Mode</th><th>Snapshot</th><th>Dernier battement</th><th>État</th></tr>");
        let now = now_ms();
        for (p, s, selected) in &pods {
            let _ = write!(
                h,
                "<tr><td class=\"p\">{}</td><td>{}</td><td>{}</td><td>il y a {} s</td><td>{}</td></tr>",
                esc(&p.instance),
                esc(&p.report.mode),
                esc(p.report.snapshot.as_deref().unwrap_or("-")),
                now.saturating_sub(p.last_seen_ms) / 1000,
                match s.as_str() {
                    _ if !selected => "<span class=\"c5\">non sélectionné</span>",
                    "aligned" => "<span class=\"c2\">aligné</span>",
                    "diverging" => "<span class=\"c4\">en cours d'alignement</span>",
                    _ => "<span class=\"c5\">absent</span>",
                }
            );
        }
        h.push_str("</table>");
    }

    h.push_str("<h2>API découvertes</h2>");
    if ops.is_empty() {
        h.push_str("<p class=\"empty\">Aucun trafic observé pour l'instant.</p>");
    }
    let api_link = api.clone();
    render_operations(&mut h, &ops, &move |a| {
        format!("{api_link}/openapi/outbound/{a}")
    });

    h.push_str("<h2>Snapshots</h2>");
    if snaps.is_empty() {
        h.push_str("<p class=\"empty\">Aucun snapshot : il est construit au passage en mock.</p>");
    } else {
        h.push_str("<table><tr><th>Identifiant</th><th>Taille</th></tr>");
        for s in &snaps {
            let id = s["id"].as_str().unwrap_or_default();
            let _ = write!(
                h,
                "<tr><td class=\"p\"><a href=\"{api}/snapshots/{0}\">{0}</a></td><td>{1} octets</td></tr>",
                esc(id),
                s["size"]
            );
        }
        h.push_str("</table>");
    }

    let _ = write!(
        h,
        "<h2>Requêtes enregistrées</h2><p>Export pour le rejeu : <a href=\"{api}/requests?format=k6\">script k6</a> · <a href=\"{api}/requests?format=jsonl\">JSONL</a> · <a href=\"{api}/requests\">JSON</a></p>"
    );
    if recent.is_empty() {
        h.push_str("<p class=\"empty\">Aucune requête entrante enregistrée.</p>");
    } else {
        h.push_str("<table><tr><th>Méthode</th><th>Chemin</th><th>Code</th><th>Durée</th><th>Instance</th></tr>");
        for (instance, e) in &recent {
            let query: Vec<String> = e.query.iter().map(|(k, v)| format!("{k}={v}")).collect();
            let path = if query.is_empty() {
                e.path.clone()
            } else {
                format!("{}?{}", e.path, query.join("&"))
            };
            let _ = write!(
                h,
                "<tr><td class=\"m\">{}</td><td class=\"p\">{}</td><td class=\"c{}\">{}</td><td>{:.1} ms</td><td>{}</td></tr>",
                esc(&e.method),
                esc(&path),
                e.status / 100,
                e.status,
                e.total_us as f64 / 1000.0,
                esc(instance)
            );
        }
        h.push_str("</table>");
    }
    h.push_str("</main></body></html>");
    Html(h).into_response()
}
