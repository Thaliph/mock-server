//! OpenAPI 3.1 documents from a stored report (spec §7.6). Pure: a function of the checked
//! report, the namespace, the deployment and the `:meta` version; no clock, no I/O.
//!
//! Output is `serde_json::Value`, whose objects are sorted maps, with arrays in the orders of
//! the spec, so the same input gives the same bytes. Nothing recorded is written into a
//! `description`, `summary` or `title` (Swagger UI renders those as Markdown).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use crate::proxy::authority::{INVALID_AUTHORITY, normalise_authority};

use super::histogram::{Bound, Histogram};
use super::openapi_parts as parts;
use super::redact::MARKER;
use super::report::{Direction, Endpoint, OutboundMeta, Report, Stats};
use super::schema::string_format;
use super::scope::ScopeKey;
use super::template::{render_with, split_path};

/// `openapi` of every document.
pub const OPENAPI_VERSION: &str = "3.1.1";

/// Most `servers` of a document.
const MAX_SERVERS: usize = 16;
/// `info.description`: constant, nothing recorded.
pub(super) const DESCRIPTION: &str =
    "Inferred by mimic from observed traffic; not an authoritative contract.";
/// The last day shown, 9999-12-31, in days since 1970-01-01.
const MAX_DAY: u64 = 2_932_896;

/// Methods that have a field in an OpenAPI path item.
const METHODS: [(&str, &str); 8] = [
    ("GET", "get"),
    ("PUT", "put"),
    ("POST", "post"),
    ("DELETE", "delete"),
    ("OPTIONS", "options"),
    ("HEAD", "head"),
    ("PATCH", "patch"),
    ("TRACE", "trace"),
];

/// The port of an inbound document id (`inbound-<port>`, decimal, no leading zero, 1-65535).
pub fn parse_id(id: &str) -> Option<u16> {
    let digits = id.strip_prefix("inbound-")?;
    if digits.is_empty() || digits.len() > 5 || digits.starts_with('0') {
        return None;
    }
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// A document id (spec §7.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocId {
    Inbound(u16),
    /// The authority of the scope, in the grammar of §6.1.
    Outbound(String),
}

/// `inbound-<port>` or `outbound-<authority>`: the scope key grammar of §6.1, no other.
pub fn parse_document_id(id: &str) -> Option<DocId> {
    if let Some(authority) = id.strip_prefix("outbound-") {
        return match ScopeKey::parse(authority)? {
            ScopeKey::Authority(authority) => Some(DocId::Outbound(authority)),
            ScopeKey::Port(_) => None,
        };
    }
    parse_id(id).map(DocId::Inbound)
}

fn id_of(port: u16) -> String {
    format!("inbound-{port}")
}

fn outbound_id(authority: &str) -> String {
    format!("outbound-{authority}")
}

/// The inbound endpoints, in the order of the report.
fn inbound(report: &Report) -> Vec<Endpoint<'_>> {
    let mut endpoints = report.endpoints();
    endpoints.retain(|e| e.direction == Direction::Inbound);
    endpoints
}

/// The outbound endpoints, in the order of the report (authority, template, method).
fn outbound(report: &Report) -> Vec<Endpoint<'_>> {
    let mut endpoints = report.endpoints();
    endpoints.retain(|e| e.direction == Direction::Outbound);
    endpoints
}

/// `{"documents": [{"id", "direction", "port" | "authority", "endpoints", "tunnels"?}]}`,
/// sorted by id.
pub fn index(report: &Report) -> Value {
    let mut documents: BTreeMap<String, Value> = BTreeMap::new();
    let mut counts: BTreeMap<String, (u16, u64)> = BTreeMap::new();
    for e in inbound(report) {
        counts.entry(id_of(e.port)).or_insert((e.port, 0)).1 += 1;
    }
    for (id, (port, endpoints)) in counts {
        let entry = json!({"id": id, "direction": "inbound", "port": port, "endpoints": endpoints});
        documents.insert(id, entry);
    }
    let mut counts: BTreeMap<&str, u64> = BTreeMap::new();
    for e in outbound(report) {
        if let Some(authority) = e.authority {
            *counts.entry(authority).or_default() += 1;
        }
    }
    for (authority, meta) in report.outbound_meta() {
        let id = outbound_id(authority);
        let tunnels = meta
            .tunnels
            .values()
            .fold(0u64, |n, t| n.saturating_add(*t));
        let entry = json!({
            "id": id,
            "direction": "outbound",
            "authority": authority,
            "endpoints": counts.get(authority).copied().unwrap_or(0),
            "tunnels": tunnels,
        });
        documents.insert(id, entry);
    }
    json!({ "documents": documents.into_values().collect::<Vec<_>>() })
}

/// `{"endpoints": [...]}` in the order of the report's endpoints.
pub fn endpoint_list(report: &Report) -> Value {
    let all = inbound(report).into_iter().chain(outbound(report));
    let endpoints: Vec<Value> = all
        .map(|e| {
            let mut row = json!({
                "method": e.method,
                "template": e.template,
                "grpc": e.grpc,
                "exchanges": e.stats.total(),
                "statuses": statuses(e.stats),
            });
            if let Some(authority) = e.authority {
                row["direction"] = "outbound".into();
                row["authority"] = authority.into();
                row["document"] = outbound_id(authority).into();
            } else {
                row["direction"] = "inbound".into();
                row["port"] = e.port.into();
                row["hosts"] = json!(e.stats.hosts);
                row["document"] = id_of(e.port).into();
            }
            if let Some(latency) = latency(&e.stats.latency) {
                row["latency_ms"] = latency;
            }
            if let Some(day) = e.stats.last_seen() {
                row["last_seen"] = date(day).into();
            }
            row
        })
        .collect();
    json!({ "endpoints": endpoints })
}

/// The document `id`, or `None` when the report holds no such scope.
pub fn document(
    report: &Report,
    namespace: &str,
    deployment: &str,
    version: u64,
    id: &str,
) -> Option<Value> {
    let (endpoints, scope) = match parse_document_id(id)? {
        DocId::Inbound(port) => {
            let endpoints: Vec<Endpoint<'_>> = inbound(report)
                .into_iter()
                .filter(|e| e.port == port)
                .collect();
            if endpoints.is_empty() {
                return None;
            }
            (endpoints, DocScope::Inbound(port))
        }
        DocId::Outbound(authority) => {
            let meta = report.outbound_meta().remove(authority.as_str())?;
            let endpoints: Vec<Endpoint<'_>> = outbound(report)
                .into_iter()
                .filter(|e| e.authority == Some(authority.as_str()))
                .collect();
            (endpoints, DocScope::Outbound(authority, meta))
        }
    };
    let mut paths: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    let mut schemes: BTreeMap<String, Value> = BTreeMap::new();
    for e in &endpoints {
        // A method named `$ref` would put a `$ref` member into the document.
        if parts::is_ref(e.method) {
            continue;
        }
        let key = path_key(e);
        let item = paths.entry(key.clone()).or_default();
        let auth = parts::schemes(e);
        let operation = operation(e, &key, auth.keys());
        schemes.extend(auth);
        place(item, e.method, operation);
    }
    let paths: Map<String, Value> = paths
        .into_iter()
        .map(|(key, item)| (key, Value::Object(item)))
        .collect();
    let title = match &scope {
        DocScope::Inbound(port) => format!("{namespace}/{deployment} inbound :{port}"),
        // The authority is recorded data: it is not in a Markdown field.
        DocScope::Outbound(..) => format!("{namespace}/{deployment} outbound"),
    };
    let mut doc = json!({
        "openapi": OPENAPI_VERSION,
        "info": {
            "title": title,
            "version": version.to_string(),
            "description": DESCRIPTION,
        },
        "paths": paths,
    });
    let servers = match &scope {
        DocScope::Inbound(_) => servers(&endpoints),
        DocScope::Outbound(authority, meta) => {
            doc["info"]["x-mimic-authority"] = authority.as_str().into();
            outbound_servers(authority, meta)
        }
    };
    if !servers.is_empty() {
        doc["servers"] = servers.into();
    }
    if let DocScope::Outbound(_, meta) = &scope {
        if !meta.sni.is_empty() || !meta.dst.is_empty() {
            doc["x-mimic-origin"] = json!({"sni": meta.sni, "dst": meta.dst});
        }
        if !meta.tunnels.is_empty() {
            doc["x-mimic-tunnels"] = json!(meta.tunnels);
        }
    }
    if !schemes.is_empty() {
        doc["components"] = json!({ "securitySchemes": schemes });
    }
    Some(doc)
}

/// The scope of a document.
enum DocScope<'a> {
    Inbound(u16),
    Outbound(String, OutboundMeta<'a>),
}

/// One `{"url": "<scheme>://<authority>"}` per scheme seen, `http` then `https`; none for a
/// redacted host.
fn outbound_servers(authority: &str, meta: &OutboundMeta<'_>) -> Vec<Value> {
    if authority.contains(MARKER) {
        return Vec::new();
    }
    meta.schemes
        .iter()
        .map(|scheme| json!({"url": format!("{scheme}://{authority}")}))
        .collect()
}

/// Puts an operation into a path item: under the lower-case key of a standard method, else in
/// `x-mimic-methods` under the method. Endpoints that share a key and a method (a gRPC and an
/// HTTP one, literals that escape alike) never overwrite each other: the first, in the order
/// of the report, takes the slot, the next ones are `x-mimic-methods` entries `<METHOD>#2`,
/// `<METHOD>#3`, and so on.
fn place(item: &mut Map<String, Value>, method: &str, operation: Value) {
    if let Some((_, lower)) = METHODS.iter().find(|(upper, _)| *upper == method)
        && !item.contains_key(*lower)
    {
        item.insert((*lower).to_string(), operation);
        return;
    }
    let extra = item
        .entry("x-mimic-methods")
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(extra) = extra else {
        return;
    };
    // A standard method is here because its slot is taken: it starts at `#2`.
    let standard = METHODS.iter().any(|(upper, _)| *upper == method);
    let name = std::iter::once(method.to_string())
        .filter(|_| !standard)
        .chain((2..).map(|n| format!("{method}#{n}")))
        .find(|name| !extra.contains_key(name));
    if let Some(name) = name {
        extra.insert(name, operation);
    }
}

/// `{`/`}` of a literal are percent-encoded so that a literal is never read as a parameter.
fn escape(literal: &str) -> String {
    literal.replace('{', "%7B").replace('}', "%7D")
}

/// The key of the path item: the template with its literals escaped; `/` for the root.
fn path_key(e: &Endpoint<'_>) -> String {
    if e.grpc {
        escape(&e.template)
    } else if e.segs.is_empty() {
        "/".to_string()
    } else {
        render_with(&e.segs, escape)
    }
}

fn operation<'a>(e: &Endpoint<'_>, key: &str, security: impl Iterator<Item = &'a String>) -> Value {
    let observed = e.stats.observed().to_json();
    let mut op = Map::new();
    let mut parameters = Vec::new();
    if e.grpc {
        let service = e.template.trim_start_matches('/').split('/').next();
        if let Some(service) = service.filter(|s| !s.is_empty()) {
            op.insert("tags".into(), json!([service]));
        }
        op.insert("x-mimic-grpc".into(), true.into());
    } else {
        parameters = path_parameters(e, key);
    }
    parameters.extend(parts::parameters(e, &observed));
    if !parameters.is_empty() {
        op.insert("parameters".into(), parameters.into());
    }
    // One requirement per scheme, in byte order of its key.
    let security: Vec<Value> = security.map(|name| json!({ name: [] })).collect();
    if !security.is_empty() {
        op.insert("security".into(), security.into());
    }
    if let Some(body) = parts::request_body(e, &observed) {
        op.insert("requestBody".into(), body);
    }
    op.insert("responses".into(), parts::responses(e, &observed));
    let fields = e.stats.observed().volatility();
    let volatile: Vec<&str> = fields
        .fields()
        .into_iter()
        .filter(|f| fields.is_volatile(f))
        .collect();
    let redacted: Vec<&str> = fields
        .fields()
        .into_iter()
        .filter(|f| fields.is_redacted(f))
        .collect();
    if !volatile.is_empty() {
        op.insert("x-mimic-volatile".into(), json!(volatile));
    }
    if !redacted.is_empty() {
        op.insert("x-mimic-redacted".into(), json!(redacted));
    }
    op.insert("x-mimic-exchanges".into(), e.stats.total().into());
    if let Some(latency) = latency(&e.stats.latency) {
        op.insert("x-mimic-latency-ms".into(), latency);
    }
    if e.direction == Direction::Inbound && !e.stats.hosts.is_empty() {
        op.insert("x-mimic-hosts".into(), json!(e.stats.hosts));
    }
    if let Some(day) = e.stats.last_seen() {
        op.insert("x-mimic-last-seen".into(), date(day).into());
    }
    Value::Object(op)
}

/// The path parameters in template order. The schema is a string, with the §6 format when
/// every value of that position in the endpoint's sample paths has the same one.
fn path_parameters(e: &Endpoint<'_>, key: &str) -> Vec<Value> {
    // Escaped literals hold no brace, so a segment that starts with one is a parameter.
    let segments = split_path(key);
    let samples: Vec<Vec<&str>> = e
        .stats
        .samples()
        .iter()
        .map(|(_, s)| split_path(s.path()))
        .filter(|s| s.len() == segments.len())
        .collect();
    segments
        .iter()
        .enumerate()
        .filter_map(|(at, segment)| {
            let name = segment.strip_prefix('{')?.strip_suffix('}')?;
            let mut formats = samples.iter().map(|s| string_format(s[at]));
            let format = formats
                .next()
                .flatten()
                .filter(|first| formats.all(|f| f == Some(*first)));
            let mut schema = json!({"type": "string"});
            if let Some(format) = format {
                schema["format"] = format.into();
            }
            Some(json!({"name": name, "in": "path", "required": true, "schema": schema}))
        })
        .collect()
}

/// `{"<status>": count}`.
fn statuses(stats: &Stats) -> Value {
    Value::Object(
        stats
            .statuses
            .iter()
            .map(|(status, n)| (status.to_string(), (*n).into()))
            .collect(),
    )
}

/// `{"p50", "p90", "p99"}`: the upper bound of the bucket holding each quantile, `null` for
/// the overflow bucket; `None` without data.
fn latency(histogram: &Histogram) -> Option<Value> {
    let quantile = |percent| {
        histogram.quantile(percent).map(|bound| match bound {
            Bound::Ms(ms) => Value::from(ms),
            Bound::Overflow => Value::Null,
        })
    };
    Some(json!({"p50": quantile(50)?, "p90": quantile(90)?, "p99": quantile(99)?}))
}

/// One `{"url": "http://<host>"}` per distinct valid host of the scope, sorted, at most 16.
fn servers(endpoints: &[Endpoint<'_>]) -> Vec<Value> {
    let hosts: BTreeSet<&str> = endpoints
        .iter()
        .flat_map(|e| e.stats.hosts.iter().map(String::as_str))
        .filter(|h| is_authority(h))
        .collect();
    hosts
        .into_iter()
        .take(MAX_SERVERS)
        .map(|host| json!({"url": format!("http://{host}")}))
        .collect()
}

/// A §4.1 normalised authority that is not redacted.
fn is_authority(host: &str) -> bool {
    host != INVALID_AUTHORITY && !host.contains(MARKER) && normalise_authority(host, 80) == host
}

/// `YYYY-MM-DD` of a day counted since 1970-01-01 (proleptic Gregorian), at most 9999-12-31.
fn date(day: u64) -> String {
    // Civil from days, after Howard Hinnant.
    let z = i64::try_from(day.min(MAX_DAY)).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}
