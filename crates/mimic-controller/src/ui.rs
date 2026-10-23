//! Interface du contrôleur (spec §5.5 et §9.2b), joignable par `kubectl port-forward` : en
//! lecture, et en écriture pour le cycle de vie des snapshots (`pin`, import, purge à la
//! demande, CTL-46), protégée comme le reste par le seul RBAC `pods/portforward`.

use std::fmt::Write as _;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use mimic_core::catalog::{STYLE, esc, operations, render_operations};
use mimic_core::model::Direction;
use mimic_core::unmatched::UnmatchedReport;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::{Ctl, now_ms, valid_name};

/// Un snapshot importé ne dépasse pas cette taille (comme `MAX_SEGMENT`, spec §9.2b).
const MAX_IMPORT: usize = 64 * 1024 * 1024;

pub fn router(ctl: Arc<Ctl>) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/healthz", get(|| async { "ok" }))
        .route("/metrics", get(metrics))
        .route("/ui/{ns}/{wl}", get(page))
        .route("/v1/workloads", get(workloads))
        .route("/v1/workloads/{ns}/{wl}/mode", get(mode))
        .route(
            "/v1/workloads/{ns}/{wl}/snapshots",
            get(snapshots)
                .post(import_snapshot)
                .layer(axum::extract::DefaultBodyLimit::max(MAX_IMPORT)),
        )
        .route("/v1/workloads/{ns}/{wl}/snapshots/{id}", get(snapshot))
        .route(
            "/v1/workloads/{ns}/{wl}/snapshots/{id}/pin",
            post(pin_snapshot),
        )
        .route("/v1/workloads/{ns}/{wl}/snapshots/gc", post(gc_snapshots))
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

pub(crate) async fn metrics(State(ctl): State<Arc<Ctl>>) -> Response {
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
                    "mock_websocket": p.report.mock_websocket,
                    "unmatched": p.report.unmatched,
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
            "unmatched": ctl.unmatched(w),
        })
    })
    .await
    // État souhaité illisible sur le support : le workload reste listé, avec l'erreur.
    .unwrap_or_else(|e| json!({"namespace": ns, "name": wl, "error": e}))
}

fn unavailable(e: String) -> Response {
    tracing::warn!(error = %e, "état souhaité illisible sur le support");
    (StatusCode::SERVICE_UNAVAILABLE, "support indisponible").into_response()
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
    match ctl.with_live(&ns, &wl, |w| w.desired.clone()).await {
        Ok(d) => axum::Json(d).into_response(),
        Err(e) => unavailable(e),
    }
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

#[derive(Deserialize)]
struct Pin {
    pinned: bool,
}

/// `mimicctl snapshot pin`/`--unpin` (CTL-46) : protège ou non le snapshot de la purge.
async fn pin_snapshot(
    State(ctl): State<Arc<Ctl>>,
    Path((ns, wl, id)): Path<(String, String, String)>,
    axum::Json(body): axum::Json<Pin>,
) -> Response {
    if !valid_name(&ns) || !valid_name(&wl) || !valid_name(&id) {
        return bad_name();
    }
    match ctl.pin_snapshot(&ns, &wl, &id, body.pinned).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) if e == "snapshot inconnu" => (StatusCode::NOT_FOUND, e).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// `mimicctl snapshot import` (CTL-46) : contenu JSONL canonicalisé, épinglé par défaut.
async fn import_snapshot(State(ctl): State<Arc<Ctl>>, Path((ns, wl)): Wl, body: Bytes) -> Response {
    if !valid_name(&ns) || !valid_name(&wl) {
        return bad_name();
    }
    match ctl.import_snapshot(&ns, &wl, &body).await {
        Ok(snap) => {
            axum::Json(json!({"id": snap.id, "exchanges": snap.exchanges.len()})).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

/// `mimicctl snapshot gc` (CTL-46) : purge immédiate, hors de la passe de maintenance.
async fn gc_snapshots(State(ctl): State<Arc<Ctl>>, Path((ns, wl)): Wl) -> Response {
    if !valid_name(&ns) || !valid_name(&wl) {
        return bad_name();
    }
    let deleted: Vec<String> = ctl.gc_snapshots(&ns, &wl).await.into_iter().collect();
    axum::Json(json!({ "deleted": deleted })).into_response()
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

/// Requêtes sans réponse du mock, agrégées sur les pods sélectionnés (spec §5.3).
fn render_unmatched(h: &mut String, u: &UnmatchedReport, now: u64) {
    let _ = write!(
        h,
        "<h2>Requêtes sans réponse du mock</h2><p>{} requête(s) refusée(s) par le mock (599 sans enregistrement, 413 body trop gros) depuis l'entrée en mock, d'après le dernier battement de chaque pod sélectionné.</p>",
        u.total
    );
    if u.entries.is_empty() {
        h.push_str("<p class=\"empty\">Aucune.</p>");
        return;
    }
    h.push_str("<table><tr><th>Méthode</th><th>Dépendance</th><th>Chemin</th><th>Code</th><th>Occurrences</th><th>Dernière</th></tr>");
    for e in &u.entries {
        let _ = write!(
            h,
            "<tr><td class=\"m\">{}</td><td>{}</td><td class=\"p\">{}</td><td class=\"c{}\">{}</td><td>{}</td><td>il y a {} s</td></tr>",
            esc(&e.method),
            esc(&e.authority),
            esc(&e.path),
            e.reason.status() / 100,
            e.reason.status(),
            e.count,
            now.saturating_sub(e.last_ms) / 1000
        );
    }
    h.push_str("</table>");
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
            let Ok((mode, s)) = ctl
                .with_live(&ns, &wl, |w| (w.desired.mode.clone(), ctl.summary(w)))
                .await
            else {
                let _ = write!(
                    h,
                    "<tr><td>{}/{}</td><td colspan=\"3\">support indisponible</td></tr>",
                    esc(&ns),
                    esc(&wl)
                );
                continue;
            };
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
    let live = ctl
        .with_live(&ns, &wl, |w| {
            (
                ctl.summary(w),
                ctl.pod_states(w),
                w.desired.clone(),
                w.spec.is_some(),
                ctl.unmatched(w),
            )
        })
        .await;
    let (summary, pods, desired, has_spec, unmatched) = match live {
        Ok(l) => l,
        Err(e) => return unavailable(e),
    };
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
        h.push_str("<table><tr><th>Instance</th><th>Mode</th><th>Snapshot</th><th>Sans réponse</th><th>Dernier battement</th><th>État</th></tr>");
        let now = now_ms();
        for (p, s, selected) in &pods {
            let _ = write!(
                h,
                "<tr><td class=\"p\">{}</td><td>{}</td><td>{}</td><td>{}</td><td>il y a {} s</td><td>{}</td></tr>",
                esc(&p.instance),
                esc(&p.report.mode),
                esc(p.report.snapshot.as_deref().unwrap_or("-")),
                p.report.unmatched.as_ref().map_or(0, |u| u.total),
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

    render_unmatched(&mut h, &unmatched, now_ms());

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
