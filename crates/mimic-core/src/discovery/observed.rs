//! What one endpoint has shown of its query, headers and bodies (spec §6): schemas and
//! volatility, merged as one value. Everything is redacted before it is inferred or hashed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use xxhash_rust::xxh3::xxh3_64;

use super::redact::{MARKER, Redactor};
use super::schema::{Pair, Params, Schema, header_pairs, query_pairs};
use super::volatile::Volatility;

/// A body over this is not parsed (encoded or decoded): media type and size only.
pub const MAX_BODY: usize = 256 * 1024;
/// Media types kept per side of an endpoint (the first in byte order).
const MAX_MEDIA: usize = 16;
/// Longest media type kept.
const MAX_MEDIA_LEN: usize = 100;
/// Levels and array items walked for volatile leaves, as for schemas.
const MAX_LEVEL: usize = 16;
const MAX_ITEMS: usize = 256;

/// One exchange as the schemas see it. Bodies are already content-decoded.
#[derive(Debug, Clone, Default)]
pub struct Observation<'a> {
    pub query: &'a str,
    pub req_headers: &'a [(&'a str, &'a str)],
    pub req_body: Option<&'a [u8]>,
    pub resp_headers: &'a [(&'a str, &'a str)],
    pub resp_body: Option<&'a [u8]>,
}

/// The bodies of one media type.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Media {
    count: u64,
    bytes: u64,
    /// JSON bodies.
    json: Option<Schema>,
    /// Form bodies: the schema of the equivalent query.
    form: Option<Params>,
}

impl Media {
    fn merge(mut self, other: Self) -> Self {
        self.count += other.count;
        self.bytes += other.bytes;
        self.json = merge_opt(self.json, other.json, Schema::merge);
        self.form = merge_opt(self.form, other.form, Params::merge);
        self
    }
}

fn merge_opt<T>(a: Option<T>, b: Option<T>, f: impl FnOnce(T, T) -> T) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(f(a, b)),
        (a, b) => a.or(b),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Side {
    headers: Params,
    bodies: BTreeMap<String, Media>,
}

impl Side {
    fn merge(mut self, other: Self) -> Self {
        self.headers = self.headers.merge(other.headers);
        for (media, m) in other.bodies {
            let merged = match self.bodies.remove(&media) {
                Some(mine) => mine.merge(m),
                None => m,
            };
            self.bodies.insert(media, merged);
        }
        while self.bodies.len() > MAX_MEDIA {
            self.bodies.pop_last();
        }
        self
    }

    fn to_json(&self) -> Value {
        let bodies: Map<String, Value> = self
            .bodies
            .iter()
            .map(|(media, m)| {
                let mut o = Map::new();
                o.insert("count".into(), json!(m.count));
                o.insert("bytes".into(), json!(m.bytes));
                if let Some(s) = &m.json {
                    o.insert("schema".into(), s.to_json());
                } else if let Some(p) = &m.form {
                    o.insert("schema".into(), p.to_json());
                }
                (media.clone(), o.into())
            })
            .collect();
        json!({"headers": self.headers.to_json(), "body": bodies})
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observed {
    query: Params,
    request: Side,
    response: Side,
    fields: Volatility,
}

/// True when a redacted value carries the marker (the whole value or a part of it).
fn redacted(value: &str) -> bool {
    value.contains(MARKER)
}

fn observe_pairs(fields: &mut Volatility, prefix: &str, pairs: &[Pair]) {
    for p in pairs {
        fields.observe(&format!("{prefix}/{}", p.key), &p.value, redacted(&p.value));
    }
}

fn pointer_token(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Observes the scalar leaves of a redacted JSON value; array indices are `*`.
fn observe_leaves(
    fields: &mut Volatility,
    prefix: &str,
    pointer: &str,
    value: &Value,
    level: usize,
) {
    if level > MAX_LEVEL {
        return;
    }
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let p = format!("{pointer}/{}", pointer_token(k));
                observe_leaves(fields, prefix, &p, v, level + 1);
            }
        }
        Value::Array(items) => {
            let p = format!("{pointer}/*");
            for v in items.iter().take(MAX_ITEMS) {
                observe_leaves(fields, prefix, &p, v, level + 1);
            }
        }
        leaf => {
            let text = leaf.to_string();
            let is_redacted = leaf.as_str().is_some_and(redacted);
            fields.observe(&format!("{prefix}{pointer}"), &text, is_redacted);
        }
    }
}

fn header<'a>(headers: &[(&'a str, &'a str)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| *v)
}

/// What a body turned out to be, redacted.
pub(super) enum Content {
    /// Not parsed: another media type, or over `MAX_BODY`.
    Opaque,
    Json {
        value: Value,
        schema: Schema,
    },
    Form {
        pairs: Vec<Pair>,
        params: Params,
    },
}

/// A body with a `content-type`, parsed once for both the schemas and the samples.
pub(super) struct Parsed {
    pub media: String,
    pub size: usize,
    pub content: Content,
    /// XXH3 of the schema skeleton; 0 for an opaque body.
    pub shape: u64,
}

/// Parses a body (already content-decoded) by its `content-type`; nothing without one.
pub(super) fn parse(redactor: &Redactor, headers: &[(&str, &str)], body: &[u8]) -> Option<Parsed> {
    let media = header(headers, "content-type")?
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if media.is_empty() || media.len() > MAX_MEDIA_LEN {
        return None;
    }
    let mut content = Content::Opaque;
    let mut shape = 0;
    if body.len() <= MAX_BODY {
        if media == "application/json" || media.ends_with("+json") {
            if let Ok(mut value) = serde_json::from_slice::<Value>(body) {
                redactor.json(&mut value);
                let schema = Schema::infer(&value);
                shape = xxh3_64(schema.to_json().to_string().as_bytes());
                content = Content::Json { value, schema };
            }
        } else if media == "application/x-www-form-urlencoded" {
            let text = String::from_utf8_lossy(body);
            let pairs = query_pairs(redactor, &text);
            let params = Params::from_pairs(&pairs);
            shape = xxh3_64(params.to_json().to_string().as_bytes());
            content = Content::Form { pairs, params };
        }
    }
    Some(Parsed {
        media,
        size: body.len(),
        content,
        shape,
    })
}

impl Observed {
    /// Records one exchange: the same as merging the one-exchange value.
    pub fn record(&mut self, redactor: &Redactor, o: &Observation<'_>) {
        self.record_parsed(redactor, o);
    }

    /// As `record`, and hands back the parsed request and response bodies for the samples.
    pub(super) fn record_parsed(
        &mut self,
        redactor: &Redactor,
        o: &Observation<'_>,
    ) -> (Option<Parsed>, Option<Parsed>) {
        let query = query_pairs(redactor, o.query);
        observe_pairs(&mut self.fields, "query", &query);
        self.query = std::mem::take(&mut self.query).merge(Params::from_pairs(&query));

        let req = header_pairs(redactor, o.req_headers);
        observe_pairs(&mut self.fields, "request.header", &req);
        let resp = header_pairs(redactor, o.resp_headers);
        observe_pairs(&mut self.fields, "response.header", &resp);

        let mut request = Side {
            headers: Params::from_pairs(&req),
            ..Side::default()
        };
        let mut response = Side {
            headers: Params::from_pairs(&resp),
            ..Side::default()
        };
        let req_body = self.body(
            redactor,
            "request.body",
            &mut request,
            o.req_headers,
            o.req_body,
        );
        let resp_body = self.body(
            redactor,
            "response.body",
            &mut response,
            o.resp_headers,
            o.resp_body,
        );
        self.request = std::mem::take(&mut self.request).merge(request);
        self.response = std::mem::take(&mut self.response).merge(response);
        (req_body, resp_body)
    }

    fn body(
        &mut self,
        redactor: &Redactor,
        prefix: &str,
        side: &mut Side,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Option<Parsed> {
        let parsed = parse(redactor, headers, body?)?;
        let field = format!("{prefix}/{}", parsed.media);
        let mut m = Media {
            count: 1,
            bytes: parsed.size as u64,
            ..Media::default()
        };
        match &parsed.content {
            Content::Json { value, schema } => {
                observe_leaves(&mut self.fields, &field, "", value, 1);
                m.json = Some(schema.clone());
            }
            Content::Form { pairs, params } => {
                observe_pairs(&mut self.fields, &field, pairs);
                m.form = Some(params.clone());
            }
            Content::Opaque => {}
        }
        side.bodies.insert(parsed.media.clone(), m);
        Some(parsed)
    }

    pub fn merge(self, other: Self) -> Self {
        Self {
            query: self.query.merge(other.query),
            request: self.request.merge(other.request),
            response: self.response.merge(other.response),
            fields: self.fields.merge(other.fields),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "query": self.query.to_json(),
            "request": self.request.to_json(),
            "response": self.response.to_json(),
            "fields": self.fields.to_json(),
        })
    }
}
