//! Inférence d'un document OpenAPI 3.1 à partir d'échanges capturés (spec §6).
//!
//! [`infer`] est une fonction pure et déterministe : le résultat ne dépend pas de l'ordre
//! des échanges en entrée (ils sont triés par `started_at_ms` puis `id`).

pub mod path;
pub mod schema;

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use crate::model::{Body, Direction, Exchange, is_json, media_type};
use crate::redact::is_marker;
use path::{PathTemplater, SegmentKind, Templated};
use schema::SchemaAcc;

/// Headers de requête qui ne décrivent pas l'API (transport, proxy, traçage, authentification).
const IGNORED_HEADERS: &[&str] = &[
    "accept",
    "accept-encoding",
    "accept-language",
    "authorization",
    "b3",
    "cache-control",
    "connection",
    "content-encoding",
    "content-length",
    "content-type",
    "cookie",
    "forwarded",
    "host",
    "if-modified-since",
    "if-none-match",
    "keep-alive",
    "origin",
    "pragma",
    "proxy-authorization",
    "referer",
    "te",
    "traceparent",
    "tracestate",
    "transfer-encoding",
    "upgrade",
    "user-agent",
    "via",
    "x-real-ip",
    "x-request-id",
];
const IGNORED_HEADER_PREFIXES: &[&str] = &["x-forwarded-", "x-b3-", "x-envoy-", "x-amzn-", "sec-"];

/// Options d'inférence.
#[derive(Debug, Clone)]
pub struct InferOptions {
    pub title: String,
    pub version: String,
    /// Sens du trafic à documenter ; les autres échanges sont ignorés.
    pub direction: Direction,
    /// Nombre d'exemples conservés par corps (requête ou réponse) et type de contenu.
    pub max_examples: usize,
}

impl InferOptions {
    pub fn new(title: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            version: version.into(),
            direction: Direction::Inbound,
            max_examples: 3,
        }
    }

    pub fn direction(mut self, direction: Direction) -> Self {
        self.direction = direction;
        self
    }
}

/// Regroupe les échanges sortants par authority : un document par dépendance.
pub fn infer_outbound(exchanges: &[Exchange], version: &str) -> BTreeMap<String, Value> {
    let mut by_authority: BTreeMap<&str, Vec<Exchange>> = BTreeMap::new();
    for e in exchanges
        .iter()
        .filter(|e| e.direction == Direction::Outbound)
    {
        by_authority
            .entry(&e.authority)
            .or_default()
            .push(e.clone());
    }
    by_authority
        .into_iter()
        .map(|(authority, list)| {
            let opts = InferOptions::new(authority, version).direction(Direction::Outbound);
            (authority.to_string(), infer(&list, &opts))
        })
        .collect()
}

/// Produit un document OpenAPI 3.1.
pub fn infer(exchanges: &[Exchange], opts: &InferOptions) -> Value {
    let mut selected: Vec<&Exchange> = exchanges
        .iter()
        .filter(|e| e.direction == opts.direction)
        .collect();
    selected.sort_by(|a, b| (a.started_at_ms, &a.id).cmp(&(b.started_at_ms, &b.id)));

    let templater = PathTemplater::new(selected.iter().map(|e| e.path.as_str()));
    let mut ops: BTreeMap<(String, String), OperationAcc> = BTreeMap::new();
    let mut servers = BTreeSet::new();
    for e in &selected {
        servers.insert(format!("{}://{}", e.scheme, e.authority));
        let t = templater.template(&e.path);
        let key = (t.template.clone(), e.method.to_ascii_lowercase());
        ops.entry(key)
            .or_insert_with(|| OperationAcc::new(&t))
            .add(e, opts.max_examples);
    }

    let mut paths = Map::new();
    for ((template, method), op) in &ops {
        let item = paths
            .entry(template.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        item[method.as_str()] = op.to_operation(template, method);
    }

    let direction = match opts.direction {
        Direction::Inbound => "inbound",
        Direction::Outbound => "outbound",
    };
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": opts.title,
            "version": opts.version,
            "x-mimic-direction": direction,
        },
        "servers": servers.into_iter().map(|url| json!({"url": url})).collect::<Vec<_>>(),
        "paths": paths,
    })
}

#[derive(Debug)]
struct OperationAcc {
    count: u64,
    path_params: Vec<(String, BTreeSet<SegmentKind>)>,
    query: BTreeMap<String, (u64, SchemaAcc)>,
    headers: BTreeMap<String, (u64, SchemaAcc)>,
    request: BTreeMap<String, ContentAcc>,
    responses: BTreeMap<u16, ResponseAcc>,
    latencies_us: Vec<u64>,
    started_ms: Vec<u64>,
}

#[derive(Debug, Default)]
struct ResponseAcc {
    count: u64,
    content: BTreeMap<String, ContentAcc>,
}

#[derive(Debug, Default)]
struct ContentAcc {
    schema: SchemaAcc,
    binary: bool,
    examples: Vec<Value>,
}

impl OperationAcc {
    fn new(t: &Templated) -> Self {
        Self {
            count: 0,
            path_params: t
                .params
                .iter()
                .map(|p| (p.name.clone(), p.kinds.clone()))
                .collect(),
            query: BTreeMap::new(),
            headers: BTreeMap::new(),
            request: BTreeMap::new(),
            responses: BTreeMap::new(),
            latencies_us: vec![],
            started_ms: vec![],
        }
    }

    fn add(&mut self, e: &Exchange, max_examples: usize) {
        self.count += 1;
        self.latencies_us.push(e.total_us);
        self.started_ms.push(e.started_at_ms);

        let mut seen = BTreeSet::new();
        for (name, value) in &e.query {
            let entry = self.query.entry(name.clone()).or_default();
            if seen.insert(name) {
                entry.0 += 1;
            }
            if !is_marker(value) {
                entry.1.add_scalar_text(value);
            }
        }
        let mut seen = BTreeSet::new();
        for h in &e.req_headers {
            if is_ignored_header(&h.name) || is_marker(&h.value) {
                continue;
            }
            let entry = self.headers.entry(h.name.clone()).or_default();
            if seen.insert(&h.name) {
                entry.0 += 1;
            }
            entry.1.add_scalar_text(&h.value);
        }

        if let Some(ct) = e.req_header("content-type") {
            add_body(
                &mut self.request,
                &media_type(ct),
                &e.req_body,
                max_examples,
            );
        }
        let resp = self.responses.entry(e.status).or_default();
        resp.count += 1;
        if let Some(ct) = e.resp_header("content-type") {
            add_body(
                &mut resp.content,
                &media_type(ct),
                &e.resp_body,
                max_examples,
            );
        }
    }

    fn to_operation(&self, template: &str, method: &str) -> Value {
        let mut parameters = vec![];
        for (name, kinds) in &self.path_params {
            parameters.push(json!({
                "name": name,
                "in": "path",
                "required": true,
                "schema": path_param_schema(kinds),
            }));
        }
        for (loc, params) in [("query", &self.query), ("header", &self.headers)] {
            for (name, (presence, acc)) in params {
                let schema = if acc.count() == 0 {
                    json!({"type": "string"})
                } else {
                    acc.to_schema()
                };
                parameters.push(json!({
                    "name": name,
                    "in": loc,
                    "required": *presence == self.count,
                    "schema": schema,
                }));
            }
        }

        let mut op = Map::new();
        op.insert("operationId".into(), operation_id(method, template).into());
        if !parameters.is_empty() {
            op.insert("parameters".into(), parameters.into());
        }
        if !self.request.is_empty() {
            op.insert(
                "requestBody".into(),
                json!({"content": content_to_json(&self.request)}),
            );
        }
        let mut responses = Map::new();
        for (status, resp) in &self.responses {
            let mut r = Map::new();
            r.insert("description".into(), "Observed response".into());
            if !resp.content.is_empty() {
                r.insert("content".into(), content_to_json(&resp.content));
            }
            responses.insert(status.to_string(), Value::Object(r));
        }
        op.insert("responses".into(), Value::Object(responses));
        op.insert("x-mimic-stats".into(), self.stats());
        Value::Object(op)
    }

    fn stats(&self) -> Value {
        let status: Map<String, Value> = self
            .responses
            .iter()
            .map(|(s, r)| {
                (
                    s.to_string(),
                    round(r.count as f64 / self.count as f64, 4).into(),
                )
            })
            .collect();
        let mut lat: Vec<u64> = self.latencies_us.clone();
        lat.sort_unstable();
        let ms = |us: u64| round(us as f64 / 1_000.0, 3);
        json!({
            "count": self.count,
            "rate_rps_p95": rate_p95(&self.started_ms),
            "status": status,
            "latency_ms": {
                "p50": ms(nearest_rank(&lat, 0.50)),
                "p90": ms(nearest_rank(&lat, 0.90)),
                "p99": ms(nearest_rank(&lat, 0.99)),
                "max": ms(*lat.last().unwrap_or(&0)),
            },
        })
    }
}

fn add_body(
    target: &mut BTreeMap<String, ContentAcc>,
    media: &str,
    body: &Body,
    max_examples: usize,
) {
    let Some(data) = body.bytes() else {
        return;
    };
    if data.is_empty() {
        return;
    }
    let value = if is_json(media) {
        match serde_json::from_slice::<Value>(data) {
            Ok(v) => Some(v),
            Err(_) => return,
        }
    } else if media == "application/x-www-form-urlencoded" {
        let mut obj = Map::new();
        for pair in String::from_utf8_lossy(data)
            .split('&')
            .filter(|p| !p.is_empty())
        {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let mut acc = SchemaAcc::default();
            acc.add_scalar_text(v);
            let typed = match acc.to_schema()["type"].as_str() {
                Some("integer") => v
                    .parse::<i64>()
                    .map(Value::from)
                    .unwrap_or_else(|_| v.into()),
                Some("boolean") => Value::Bool(v == "true"),
                _ => v.into(),
            };
            obj.insert(k.to_string(), typed);
        }
        Some(Value::Object(obj))
    } else {
        None
    };
    let content = target.entry(media.to_string()).or_default();
    match value {
        Some(v) => {
            content.schema.add(&v);
            if content.examples.len() < max_examples {
                content.examples.push(v);
            }
        }
        None => content.binary = true,
    }
}

fn content_to_json(content: &BTreeMap<String, ContentAcc>) -> Value {
    let mut out = Map::new();
    for (media, c) in content {
        let schema = if c.binary && c.schema.count() == 0 {
            json!({"type": "string", "format": "binary"})
        } else {
            c.schema.to_schema()
        };
        let mut m = Map::new();
        m.insert("schema".into(), schema);
        if !c.examples.is_empty() {
            let examples: Map<String, Value> = c
                .examples
                .iter()
                .enumerate()
                .map(|(i, v)| (format!("sample-{}", i + 1), json!({"value": v})))
                .collect();
            m.insert("examples".into(), Value::Object(examples));
        }
        out.insert(media.clone(), Value::Object(m));
    }
    Value::Object(out)
}

fn path_param_schema(kinds: &BTreeSet<SegmentKind>) -> Value {
    if !kinds.is_empty() && kinds.iter().all(|k| *k == SegmentKind::Integer) {
        json!({"type": "integer"})
    } else if !kinds.is_empty() && kinds.iter().all(|k| *k == SegmentKind::Uuid) {
        json!({"type": "string", "format": "uuid"})
    } else {
        json!({"type": "string"})
    }
}

fn is_ignored_header(name: &str) -> bool {
    IGNORED_HEADERS.contains(&name) || IGNORED_HEADER_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// `get` + `/users/{userId}/orders` → `getUsersByUserIdOrders`.
fn operation_id(method: &str, template: &str) -> String {
    let mut id = method.to_string();
    for seg in template.split('/').filter(|s| !s.is_empty()) {
        let (prefix, word) = match seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            Some(name) => ("By", name),
            None => ("", seg),
        };
        id.push_str(prefix);
        for part in word
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|p| !p.is_empty())
        {
            let mut chars = part.chars();
            if let Some(first) = chars.next() {
                id.push(first.to_ascii_uppercase());
                id.push_str(chars.as_str());
            }
        }
    }
    id
}

/// Percentile par la méthode du rang le plus proche ; `sorted` doit être trié.
fn nearest_rank(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// p95 du nombre de requêtes par seconde sur la fenêtre observée.
fn rate_p95(started_ms: &[u64]) -> f64 {
    let (Some(min), Some(max)) = (started_ms.iter().min(), started_ms.iter().max()) else {
        return 0.0;
    };
    // Seules les secondes actives sont stockées : une fenêtre très longue ne coûte rien.
    let seconds = (max - min) / 1_000 + 1;
    let mut active: BTreeMap<u64, u64> = BTreeMap::new();
    for t in started_ms {
        *active.entry((t - min) / 1_000).or_default() += 1;
    }
    let mut counts: Vec<u64> = active.into_values().collect();
    counts.sort_unstable();
    let idle = seconds - counts.len() as u64;
    let rank = ((0.95 * seconds as f64).ceil() as u64).clamp(1, seconds);
    if rank <= idle {
        0.0
    } else {
        counts[(rank - idle - 1) as usize] as f64
    }
}

fn round(x: f64, decimals: i32) -> f64 {
    let f = 10f64.powi(decimals);
    (x * f).round() / f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_ids() {
        assert_eq!(
            operation_id("get", "/users/{userId}/orders"),
            "getUsersByUserIdOrders"
        );
        assert_eq!(operation_id("post", "/v1/line-items"), "postV1LineItems");
        assert_eq!(operation_id("get", "/"), "get");
    }

    #[test]
    fn percentiles() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(nearest_rank(&v, 0.5), 50);
        assert_eq!(nearest_rank(&v, 0.99), 99);
        assert_eq!(nearest_rank(&[7], 0.99), 7);
    }

    #[test]
    fn rate() {
        assert_eq!(rate_p95(&[0, 10, 20, 1_500]), 3.0);
        assert_eq!(rate_p95(&[]), 0.0);
    }
}
