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
use super::redact::MARKER;
use super::report::{Direction, Endpoint, Report, Stats};
use super::schema::string_format;
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

fn id_of(port: u16) -> String {
    format!("inbound-{port}")
}

/// The inbound endpoints, in the order of the report. Outbound endpoints get their documents
/// with F8 (spec §7.6).
fn inbound(report: &Report) -> Vec<Endpoint<'_>> {
    let mut endpoints = report.endpoints();
    endpoints.retain(|e| e.direction == Direction::Inbound);
    endpoints
}

/// `{"documents": [{"id", "direction", "port", "endpoints"}]}`, sorted by id.
pub fn index(report: &Report) -> Value {
    let mut counts: BTreeMap<String, (u16, u64)> = BTreeMap::new();
    for e in inbound(report) {
        counts.entry(id_of(e.port)).or_insert((e.port, 0)).1 += 1;
    }
    let documents: Vec<Value> = counts
        .into_iter()
        .map(|(id, (port, endpoints))| {
            json!({"id": id, "direction": "inbound", "port": port, "endpoints": endpoints})
        })
        .collect();
    json!({ "documents": documents })
}

/// `{"endpoints": [...]}` in the order of the report's endpoints.
pub fn endpoint_list(report: &Report) -> Value {
    let endpoints: Vec<Value> = inbound(report)
        .iter()
        .map(|e| {
            let mut row = json!({
                "direction": "inbound",
                "port": e.port,
                "method": e.method,
                "template": e.template,
                "grpc": e.grpc,
                "exchanges": e.stats.total(),
                "statuses": statuses(e.stats),
                "hosts": e.stats.hosts,
                "document": id_of(e.port),
            });
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
    let port = parse_id(id)?;
    let endpoints: Vec<Endpoint<'_>> = inbound(report)
        .into_iter()
        .filter(|e| e.port == port)
        .collect();
    if endpoints.is_empty() {
        return None;
    }
    let mut paths: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for e in &endpoints {
        let key = path_key(e);
        let item = paths.entry(key.clone()).or_default();
        let operation = operation(e, &key);
        match METHODS.iter().find(|(upper, _)| *upper == e.method) {
            Some((_, lower)) => {
                item.insert((*lower).to_string(), operation);
            }
            None => {
                let extra = item
                    .entry("x-mimic-methods")
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Value::Object(extra) = extra {
                    extra.insert(e.method.to_string(), operation);
                }
            }
        }
    }
    let paths: Map<String, Value> = paths
        .into_iter()
        .map(|(key, item)| (key, Value::Object(item)))
        .collect();
    let mut doc = json!({
        "openapi": OPENAPI_VERSION,
        "info": {
            "title": format!("{namespace}/{deployment} inbound :{port}"),
            "version": version.to_string(),
            "description": DESCRIPTION,
        },
        "paths": paths,
    });
    let servers = servers(&endpoints);
    if !servers.is_empty() {
        doc["servers"] = servers.into();
    }
    Some(doc)
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

fn operation(e: &Endpoint<'_>, key: &str) -> Value {
    let mut op = Map::new();
    if e.grpc {
        let service = e.template.trim_start_matches('/').split('/').next();
        if let Some(service) = service.filter(|s| !s.is_empty()) {
            op.insert("tags".into(), json!([service]));
        }
        op.insert("x-mimic-grpc".into(), true.into());
    } else {
        let parameters = path_parameters(e, key);
        if !parameters.is_empty() {
            op.insert("parameters".into(), parameters.into());
        }
    }
    op.insert("x-mimic-exchanges".into(), e.stats.total().into());
    if let Some(latency) = latency(&e.stats.latency) {
        op.insert("x-mimic-latency-ms".into(), latency);
    }
    if !e.stats.hosts.is_empty() {
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
