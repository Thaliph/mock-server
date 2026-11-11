//! The parts of an OpenAPI operation that come from what was observed (spec §7.6): query and
//! header parameters, request body, responses, security schemes. Every value is a redacted
//! sample or a type from the report; nothing recorded is written into a Markdown field.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::placeholder::{Placeholder, is_marked, upgrade_text};
use super::redact::{MARKER, auth_scheme};
use super::report::Endpoint;
use super::sample::Sample;

/// Most media types of a request body or of a response.
const MAX_MEDIA: usize = 16;
/// Request headers that OpenAPI ignores as parameters.
const IGNORED_HEADERS: [&str; 3] = ["accept", "content-type", "authorization"];

/// The key a recorded name must not take in a map of the document: a `$ref` member there
/// would be read as a reference by OpenAPI tools.
const REF: &str = "$ref";

/// Examples in sample-key order: `(sample key, value)`, one per distinct value.
type Examples = Vec<(u64, Value)>;

fn add_example(list: &mut Examples, key: u64, value: Value) {
    if !list.iter().any(|(_, seen)| *seen == value) {
        list.push((key, value));
    }
}

/// `{"sample-<16 hex of the sample key>": {"value": ...}}`.
fn examples_map(list: Examples) -> Value {
    Value::Object(
        list.into_iter()
            .map(|(key, value)| (format!("sample-{key:016x}"), json!({ "value": value })))
            .collect(),
    )
}

/// Query parameters, then request header parameters, each in byte order.
pub(super) fn parameters(e: &Endpoint<'_>, observed: &Value) -> Vec<Value> {
    let mut out = params(e, "query", &observed["query"], "query", &[], Sample::query);
    out.extend(params(
        e,
        "header",
        &observed["request"]["headers"],
        "request.header",
        &IGNORED_HEADERS,
        Sample::request_headers,
    ));
    out
}

fn params(
    e: &Endpoint<'_>,
    location: &str,
    schema: &Value,
    field_prefix: &str,
    skip: &[&str],
    pairs: fn(&Sample) -> &[(String, String)],
) -> Vec<Value> {
    let required = |name: &str| {
        schema["required"]
            .as_array()
            .is_some_and(|names| names.iter().any(|n| n == name))
    };
    let fields = e.stats.observed().volatility();
    let Some(properties) = schema["properties"].as_object() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, schema) in properties
        .iter()
        .filter(|(n, _)| !skip.contains(&n.as_str()))
    {
        let field = format!("{field_prefix}/{name}");
        let (volatile, redacted) = (fields.is_volatile(&field), fields.is_redacted(&field));
        let array = schema["type"] == "array";
        let mut list = Examples::new();
        for (key, sample) in e.stats.samples().iter() {
            let values: Vec<&String> = pairs(sample)
                .iter()
                .filter(|(k, _)| k == name)
                .map(|(_, v)| v)
                .collect();
            if values.is_empty() {
                continue;
            }
            // A redacted field shows a placeholder only, whatever a sample holds.
            let shown = |v: &String| {
                if is_marked(v) {
                    upgrade_text(v)
                } else {
                    Placeholder::Secret.to_string()
                }
            };
            let value = match (redacted, array) {
                (true, true) => json!(values.iter().map(|v| shown(v)).collect::<Vec<_>>()),
                (true, false) => json!(shown(values[0])),
                (false, true) => json!(values),
                (false, false) => json!(values[0]),
            };
            add_example(&mut list, key, value);
        }
        let mut p = json!({
            "name": name,
            "in": location,
            "required": required(name),
            "schema": schema,
        });
        if !list.is_empty() {
            p["examples"] = examples_map(list);
        }
        if volatile {
            p["x-mimic-volatile"] = true.into();
        }
        if redacted {
            p["x-mimic-redacted"] = true.into();
        }
        out.push(p);
    }
    out
}

/// `requestBody`: one entry per media type with its §6 schema and the samples' bodies.
pub(super) fn request_body(e: &Endpoint<'_>, observed: &Value) -> Option<Value> {
    let bodies = observed["request"]["body"].as_object()?;
    let mut content = Map::new();
    let mut counted = 0u64;
    for (media, m) in bodies
        .iter()
        .filter(|(media, _)| *media != REF)
        .take(MAX_MEDIA)
    {
        let count = m["count"].as_u64().unwrap_or(0);
        counted = counted.saturating_add(count);
        let mut list = Examples::new();
        for (key, sample) in e.stats.samples().iter() {
            if let Some((sample_media, value)) = sample.request_body()
                && sample_media == media
            {
                add_example(&mut list, key, value);
            }
        }
        let mut o = Map::new();
        if let Some(schema) = m.get("schema") {
            o.insert("schema".into(), schema.clone());
        }
        if !list.is_empty() {
            o.insert("examples".into(), examples_map(list));
        }
        o.insert("x-mimic-count".into(), count.into());
        let mean = m["bytes"].as_u64().unwrap_or(0) / count.max(1);
        o.insert("x-mimic-bytes".into(), mean.into());
        content.insert(media.clone(), Value::Object(o));
    }
    if content.is_empty() {
        return None;
    }
    let mut body = json!({ "content": content });
    if counted >= e.stats.total() {
        body["required"] = true.into();
    }
    Some(body)
}

/// The media type of a `content-type` header: lower case, without parameters.
fn media_of(headers: &[(String, String)]) -> Option<String> {
    let (_, value) = headers.iter().find(|(name, _)| name == "content-type")?;
    let media = value.split(';').next()?.trim().to_ascii_lowercase();
    (!media.is_empty()).then_some(media)
}

/// `responses`: one per status of the counters, content and headers from that status's own
/// samples, schemas from the endpoint-wide report.
pub(super) fn responses(e: &Endpoint<'_>, observed: &Value) -> Value {
    let header_schemas = &observed["response"]["headers"]["properties"];
    let mut out = Map::new();
    for (status, count) in &e.stats.statuses {
        let samples: Vec<(u64, &Sample)> = e
            .stats
            .samples()
            .iter()
            .filter(|(_, s)| s.status() == *status)
            .collect();
        let mut response = json!({
            "description": reason(*status).map_or_else(|| format!("Status {status}"), String::from),
            "x-mimic-count": count,
        });
        let mut media: BTreeMap<String, Examples> = BTreeMap::new();
        let mut headers: BTreeMap<&str, Value> = BTreeMap::new();
        for (key, sample) in &samples {
            if let Some(m) = media_of(sample.response_headers()) {
                let list = media.entry(m.clone()).or_default();
                if let Some((body_media, value)) = sample.response_body()
                    && body_media == m
                {
                    add_example(list, *key, value);
                }
            }
            for (name, _) in sample.response_headers() {
                if name != "content-type" && name != REF {
                    let schema = header_schemas.get(name).cloned();
                    headers.insert(name, schema.unwrap_or_else(|| json!({"type": "string"})));
                }
            }
        }
        let mut content = Map::new();
        for (m, list) in media.into_iter().filter(|(m, _)| m != REF).take(MAX_MEDIA) {
            let mut o = Map::new();
            if let Some(schema) = observed["response"]["body"][&m].get("schema") {
                o.insert("schema".into(), schema.clone());
            }
            if !list.is_empty() {
                o.insert("examples".into(), examples_map(list));
            }
            content.insert(m, Value::Object(o));
        }
        if !content.is_empty() {
            response["content"] = content.into();
        }
        if !headers.is_empty() {
            response["headers"] = headers
                .into_iter()
                .map(|(name, schema)| (name.to_string(), json!({ "schema": schema })))
                .collect();
        }
        out.insert(status.to_string(), response);
    }
    Value::Object(out)
}

/// The security schemes the samples' `authorization` headers show, by their key in
/// `components.securitySchemes`: `http_<scheme>` for `<Scheme> <placeholder>` (a token of
/// `a-z0-9-`), `authorization` (an API key) for a bare placeholder; the legacy marker
/// `<redacted>` reads as a placeholder.
pub(super) fn schemes(e: &Endpoint<'_>) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for (_, sample) in e.stats.samples().iter() {
        for (name, value) in sample.request_headers() {
            if name != "authorization" {
                continue;
            }
            if value == MARKER || value == Placeholder::Auth.as_str() {
                out.insert(
                    "authorization".to_string(),
                    json!({"type": "apiKey", "in": "header", "name": "Authorization"}),
                );
            } else if let Some((scheme, rest)) = value.split_once(' ') {
                // Only a registered scheme is kept, in its lower-case spelling.
                if let (Some(scheme), true) = (auth_scheme(scheme), credential(rest)) {
                    let scheme = scheme.to_ascii_lowercase();
                    out.insert(
                        format!("http_{scheme}"),
                        json!({"type": "http", "scheme": scheme}),
                    );
                }
            }
        }
    }
    out
}

/// The credential after a scheme: a placeholder of §6, or the legacy marker.
fn credential(rest: &str) -> bool {
    rest == MARKER || rest == Placeholder::Token.as_str() || rest == Placeholder::Auth.as_str()
}

/// The reason phrase of the IANA HTTP status code registry.
fn reason(status: u16) -> Option<&'static str> {
    Some(match status {
        100 => "Continue",
        101 => "Switching Protocols",
        102 => "Processing",
        103 => "Early Hints",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        207 => "Multi-Status",
        208 => "Already Reported",
        226 => "IM Used",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        305 => "Use Proxy",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Content Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        417 => "Expectation Failed",
        421 => "Misdirected Request",
        422 => "Unprocessable Content",
        423 => "Locked",
        424 => "Failed Dependency",
        425 => "Too Early",
        426 => "Upgrade Required",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        451 => "Unavailable For Legal Reasons",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        506 => "Variant Also Negotiates",
        507 => "Insufficient Storage",
        508 => "Loop Detected",
        511 => "Network Authentication Required",
        _ => return None,
    })
}

/// True for a method name that would put a `$ref` member into `x-mimic-methods`.
pub(super) fn is_ref(name: &str) -> bool {
    name == REF
}
