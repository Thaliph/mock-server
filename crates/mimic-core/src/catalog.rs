//! Catalogue des opérations découvertes, à partir des documents OpenAPI, et son rendu HTML
//! (spec §9.2). Partagé par la page d'admin du sidecar et l'interface du contrôleur.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;
use serde_json::Value;

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
pub fn operations(inbound: &Value, outbound: &BTreeMap<String, Value>) -> Vec<Operation> {
    let mut out = vec![];
    let service = inbound["info"]["title"]
        .as_str()
        .unwrap_or("service")
        .to_string();
    collect_operations(inbound, "inbound", &service, &mut out);
    for (authority, doc) in outbound {
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

pub fn esc(s: &str) -> String {
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

pub const STYLE: &str = r#"
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

/// Opérations groupées par service puis par dépendance ; `outbound_link` donne le lien
/// vers le document OpenAPI d'une dépendance.
pub fn render_operations(
    h: &mut String,
    ops: &[Operation],
    outbound_link: &dyn Fn(&str) -> String,
) {
    let mut groups: Vec<(&'static str, String)> = vec![];
    for op in ops {
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
                "<h2>Dépendance {} · <a href=\"{}\">OpenAPI</a></h2>",
                esc(&authority),
                esc(&outbound_link(&authority))
            );
        }
        for op in ops
            .iter()
            .filter(|o| o.direction == direction && o.authority == authority)
        {
            render_operation(h, op);
        }
    }
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
