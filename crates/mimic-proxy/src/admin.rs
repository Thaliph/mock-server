//! API et page d'admin (spec §9.2).

use std::fmt::Write as _;
use std::sync::Arc;

use bytes::Bytes;
use http::{Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use mimic_core::model::Direction;
use serde::Serialize;
use serde_json::{Value, json};

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
        "instance": state.config.instance,
        "workload": {
            "namespace": state.config.workload.namespace,
            "name": state.config.workload.name,
        },
        "store": {"local": local, "peer": peer},
        "share": *state.share_status.lock().expect("verrou du partage"),
    })
}

#[derive(Debug, Serialize)]
pub struct Operation {
    pub direction: &'static str,
    pub authority: String,
    pub method: String,
    pub path: String,
    pub count: u64,
    pub status: Value,
    pub latency_ms: Value,
    pub parameters: Vec<Parameter>,
    pub request_body: Option<Content>,
    pub responses: Vec<ResponseInfo>,
}

#[derive(Debug, Serialize)]
pub struct Parameter {
    pub name: String,
    #[serde(rename = "in")]
    pub location: String,
    pub required: bool,
    pub schema: Value,
}

#[derive(Debug, Serialize)]
pub struct Content {
    pub content_type: String,
    pub schema: Value,
    pub example: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct ResponseInfo {
    pub status: String,
    pub content: Option<Content>,
}

/// Liste à plat des opérations découvertes, service puis dépendances.
pub fn operations(d: &Discovery) -> Vec<Operation> {
    let mut out = vec![];
    let service = d.inbound["info"]["title"]
        .as_str()
        .unwrap_or("service")
        .to_string();
    collect_operations(&d.inbound, "inbound", &service, &mut out);
    for (authority, doc) in &d.outbound {
        collect_operations(doc, "outbound", authority, &mut out);
    }
    out
}

fn collect_operations(
    doc: &Value,
    direction: &'static str,
    authority: &str,
    out: &mut Vec<Operation>,
) {
    let Some(paths) = doc["paths"].as_object() else {
        return;
    };
    for (path, item) in paths {
        let Some(item) = item.as_object() else {
            continue;
        };
        for (method, op) in item {
            let stats = &op["x-mimic-stats"];
            let parameters = op["parameters"]
                .as_array()
                .map(|ps| {
                    ps.iter()
                        .map(|p| Parameter {
                            name: p["name"].as_str().unwrap_or_default().into(),
                            location: p["in"].as_str().unwrap_or_default().into(),
                            required: p["required"].as_bool().unwrap_or(false),
                            schema: p["schema"].clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let responses = op["responses"]
                .as_object()
                .map(|rs| {
                    rs.iter()
                        .map(|(status, r)| ResponseInfo {
                            status: status.clone(),
                            content: first_content(&r["content"]),
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.push(Operation {
                direction,
                authority: authority.to_string(),
                method: method.to_ascii_uppercase(),
                path: path.clone(),
                count: stats["count"].as_u64().unwrap_or(0),
                status: stats["status"].clone(),
                latency_ms: stats["latency_ms"].clone(),
                parameters,
                request_body: first_content(&op["requestBody"]["content"]),
                responses,
            });
        }
    }
}

fn first_content(content: &Value) -> Option<Content> {
    let (ct, c) = content.as_object()?.iter().next()?;
    Some(Content {
        content_type: ct.clone(),
        schema: c["schema"].clone(),
        example: c["examples"]
            .as_object()
            .and_then(|e| e.values().next())
            .map(|e| e["value"].clone()),
    })
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn pretty(v: &Value) -> String {
    esc(&serde_json::to_string_pretty(v).unwrap_or_default())
}

fn type_of(schema: &Value) -> String {
    let t = match &schema["type"] {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" | "),
        _ if schema.get("oneOf").is_some() => "oneOf".into(),
        _ => "any".into(),
    };
    match schema["format"].as_str() {
        Some(f) => format!("{t} ({f})"),
        None => t,
    }
}

const STYLE: &str = r#"
:root{--bg:#fafafa;--fg:#1d1d1f;--muted:#6e6e73;--card:#fff;--line:#e3e3e8;--accent:#2457d6;--ok:#1a7f37;--warn:#9a6700;--err:#cf222e;--code:#f2f2f5}
@media (prefers-color-scheme:dark){:root{--bg:#141416;--fg:#ececf1;--muted:#9a9aa3;--card:#1d1d21;--line:#303036;--accent:#7aa2ff;--ok:#4ac26b;--warn:#d4a72c;--err:#ff7b72;--code:#26262b}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:14px/1.5 system-ui,-apple-system,Segoe UI,sans-serif}
main{max-width:1100px;margin:0 auto;padding:24px 16px}h1{font-size:20px;margin:0 0 4px}h2{font-size:16px;margin:32px 0 8px}
.meta{color:var(--muted);display:flex;flex-wrap:wrap;gap:16px}.pill{display:inline-block;padding:1px 8px;border-radius:999px;border:1px solid var(--line);font-size:12px}
.mode{background:var(--accent);color:#fff;border-color:transparent}details{background:var(--card);border:1px solid var(--line);border-radius:8px;margin:6px 0}
summary{cursor:pointer;padding:8px 12px;display:grid;grid-template-columns:70px 1fr auto;gap:12px;align-items:center;list-style:none}summary::-webkit-details-marker{display:none}
.m{font-weight:600;font-family:ui-monospace,monospace;font-size:12px}.p{font-family:ui-monospace,monospace;overflow-wrap:anywhere}.s{color:var(--muted);font-size:12px;text-align:right}
.body{padding:4px 12px 12px;border-top:1px solid var(--line)}table{border-collapse:collapse;width:100%;margin:6px 0}td,th{text-align:left;padding:4px 8px;border-bottom:1px solid var(--line);vertical-align:top}
th{color:var(--muted);font-weight:500;font-size:12px}pre{background:var(--code);padding:8px;border-radius:6px;overflow:auto;max-height:320px;margin:4px 0;font-size:12px}
.req{color:var(--err);font-weight:600;font-size:12px}.c2{color:var(--ok)}.c4{color:var(--warn)}.c5{color:var(--err)}a{color:var(--accent)}.empty{color:var(--muted);font-style:italic}
"#;

fn render_page(state: &AppState) -> String {
    let d = state.discovery();
    let ops = operations(&d);
    let share = state
        .share_status
        .lock()
        .expect("verrou du partage")
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
        "<h1>API découvertes · {}/{}</h1><div class=\"meta\"><span>mode <span class=\"pill mode\">{}</span></span><span>instance {}</span><span>{} échanges locaux, {} importés</span>",
        esc(&state.config.workload.namespace),
        esc(&state.config.workload.name),
        state.mode().as_str(),
        esc(&state.config.instance),
        local,
        peer
    );
    if share.enabled {
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

    let mut groups: Vec<(&'static str, String)> = vec![];
    for op in &ops {
        let key = (op.direction, op.authority.clone());
        if !groups.contains(&key) {
            groups.push(key);
        }
    }
    if ops.is_empty() {
        h.push_str("<p class=\"empty\">Aucun trafic observé pour l'instant.</p>");
    }
    for (direction, authority) in groups {
        if direction == "inbound" {
            let _ = write!(h, "<h2>Service (trafic entrant)</h2>");
        } else {
            let _ = write!(
                h,
                "<h2>Dépendance {} · <a href=\"/openapi/outbound/{}\">OpenAPI</a></h2>",
                esc(&authority),
                esc(&authority)
            );
        }
        for op in ops
            .iter()
            .filter(|o| o.direction == direction && o.authority == authority)
        {
            render_operation(&mut h, op);
        }
    }
    h.push_str("</main></body></html>");
    h
}

fn render_operation(h: &mut String, op: &Operation) {
    let lat = &op.latency_ms;
    let codes: String = op
        .status
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(code, share)| {
                    format!(
                        "<span class=\"c{}\">{} {:.0}%</span>",
                        &code[..1],
                        esc(code),
                        share.as_f64().unwrap_or(0.0) * 100.0
                    )
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let _ = write!(
        h,
        "<details><summary><span class=\"m\">{}</span><span class=\"p\">{}</span><span class=\"s\">{} appels · {} · p50 {} ms · p99 {} ms</span></summary><div class=\"body\">",
        esc(&op.method),
        esc(&op.path),
        op.count,
        codes,
        lat["p50"],
        lat["p99"]
    );
    if op.parameters.is_empty() {
        h.push_str("<p class=\"empty\">Aucun paramètre.</p>");
    } else {
        h.push_str("<table><tr><th>Paramètre</th><th>Où</th><th>Type</th><th></th></tr>");
        for p in &op.parameters {
            let _ = write!(
                h,
                "<tr><td class=\"p\">{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(&p.name),
                esc(&p.location),
                esc(&type_of(&p.schema)),
                if p.required {
                    "<span class=\"req\">requis</span>"
                } else {
                    "optionnel"
                }
            );
        }
        h.push_str("</table>");
    }
    if let Some(b) = &op.request_body {
        let _ = write!(
            h,
            "<p>Body de requête <span class=\"pill\">{}</span></p><pre>{}</pre>",
            esc(&b.content_type),
            pretty(&b.schema)
        );
        if let Some(ex) = &b.example {
            let _ = write!(h, "<p>Exemple</p><pre>{}</pre>", pretty(ex));
        }
    }
    for r in &op.responses {
        let _ = write!(h, "<p>Réponse <strong>{}</strong>", esc(&r.status));
        match &r.content {
            Some(c) => {
                let _ = write!(
                    h,
                    " <span class=\"pill\">{}</span></p><pre>{}</pre>",
                    esc(&c.content_type),
                    pretty(&c.schema)
                );
                if let Some(ex) = &c.example {
                    let _ = write!(h, "<p>Exemple</p><pre>{}</pre>", pretty(ex));
                }
            }
            None => h.push_str(" sans body</p>"),
        }
    }
    h.push_str("</div></details>");
}
