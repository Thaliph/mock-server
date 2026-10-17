//! Requêtes enregistrées et rejeu (spec §5.7).

use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::model::{Body, Direction, Exchange};

/// Headers jamais rejoués : propres à la connexion ou recalculés par le client.
const NOT_REPLAYED: &[&str] = &[
    "connection",
    "content-length",
    "host",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Échanges d'une direction, sans doublon, les plus récents d'abord. Accepte des échanges
/// ou des références (`E: Borrow<Exchange>`) et rend les mêmes éléments, sans copie.
pub fn recent_requests<E: Borrow<Exchange>>(
    exchanges: impl IntoIterator<Item = E>,
    direction: Direction,
    limit: usize,
) -> Vec<E> {
    let mut list: Vec<E> = exchanges
        .into_iter()
        .filter(|e| e.borrow().direction == direction)
        .collect();
    list.sort_by(|a, b| {
        let (a, b) = (a.borrow(), b.borrow());
        (b.started_at_ms, &b.id).cmp(&(a.started_at_ms, &a.id))
    });
    // Premiers `limit` identifiants distincts : la déduplication s'arrête à la limite.
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(limit.min(list.len()));
    for e in list {
        if out.len() >= limit {
            break;
        }
        if seen.insert(e.borrow().id.clone()) {
            out.push(e);
        }
    }
    out
}

/// Script k6 qui rejoue les requêtes dans l'ordre d'enregistrement.
pub fn k6_script(exchanges: &[Exchange]) -> String {
    let mut ordered: Vec<&Exchange> = exchanges.iter().collect();
    ordered.sort_by(|a, b| (a.started_at_ms, &a.id).cmp(&(b.started_at_ms, &b.id)));
    let mut dropped = BTreeSet::new();
    let requests: Vec<Value> = ordered
        .iter()
        .map(|e| request_json(e, &mut dropped))
        .collect();
    let dropped = if dropped.is_empty() {
        "aucun".to_string()
    } else {
        dropped.into_iter().collect::<Vec<_>>().join(", ")
    };
    let requests = serde_json::to_string(&requests).unwrap_or_else(|_| "[]".into());
    format!(
        r#"// Généré par mimic : rejeu de {count} requête(s) enregistrée(s).
// Lancement : k6 run -e TARGET=http://service:8080 [-e AUTHORIZATION='Bearer …'] [-e VUS=10] [-e ITERATIONS=1000] script.js
// Headers redactés non rejoués : {dropped}. Les headers propres à la connexion ne le sont jamais.
import http from 'k6/http';
import encoding from 'k6/encoding';
import exec from 'k6/execution';

const TARGET = (__ENV.TARGET || 'http://localhost:8080').replace(/\/$/, '');
const AUTHORIZATION = __ENV.AUTHORIZATION;
const REQUESTS = {requests};

export const options = {{
  scenarios: {{
    replay: {{
      executor: 'shared-iterations',
      vus: Number(__ENV.VUS || 1),
      iterations: Number(__ENV.ITERATIONS || Math.max(REQUESTS.length, 1)),
    }},
  }},
}};

export default function () {{
  if (REQUESTS.length === 0) return;
  const r = REQUESTS[exec.scenario.iterationInTest % REQUESTS.length];
  const query = r.query
    .map(([k, v]) => encodeURIComponent(k) + '=' + encodeURIComponent(v))
    .join('&');
  const url = TARGET + r.path + (query ? '?' + query : '');
  const headers = Object.assign({{}}, r.headers);
  if (AUTHORIZATION) headers['authorization'] = AUTHORIZATION;
  const body = r.body_b64 ? encoding.b64decode(r.body_b64, 'std', 'b') : r.body;
  http.request(r.method, url, body, {{ headers, tags: {{ name: r.method + ' ' + r.path }} }});
}}
"#,
        count = ordered.len(),
    )
}

fn request_json(e: &Exchange, dropped: &mut BTreeSet<String>) -> Value {
    let mut headers = BTreeMap::new();
    for h in &e.req_headers {
        if NOT_REPLAYED.contains(&h.name.as_str()) {
            continue;
        }
        if h.value.contains("<redacted:") {
            dropped.insert(h.name.clone());
            continue;
        }
        headers
            .entry(h.name.clone())
            .or_insert_with(|| h.value.clone());
    }
    let (body, body_b64) = match &e.req_body {
        Body::Captured { data } if !data.is_empty() => match std::str::from_utf8(data) {
            Ok(text) => (Value::String(text.into()), Value::Null),
            Err(_) => (Value::Null, Value::String(STANDARD.encode(data))),
        },
        _ => (Value::Null, Value::Null),
    };
    json!({
        "method": e.method,
        "path": e.path,
        "query": e.query,
        "headers": headers,
        "body": body,
        "body_b64": body_b64,
    })
}
