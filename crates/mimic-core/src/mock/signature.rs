//! The request signature (spec §8): what of a request is compared with the samples, redacted
//! as the recorder would have redacted it, and the fields of a request or of a sample.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use xxhash_rust::xxh3::xxh3_64;

use crate::discovery::observed::{Content, MAX_ITEMS, MAX_LEVEL, parse, pointer_token};
use crate::discovery::redact::{self, MARKER, Redactor};
use crate::discovery::schema::{cut_name, query_pairs};
use crate::discovery::scope::authority_key;
use crate::discovery::template::split_path;

use super::Request;

/// One field: the name it has in the endpoint's `volatile` and `redacted` lists (array
/// indices written `*`), its values in order, and whether one of them holds the marker.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    star: String,
    values: Vec<String>,
    marked: bool,
}

/// The query and body fields of a request or of a sample's request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Fields {
    entries: BTreeMap<String, Entry>,
}

/// Fields as they are compared: sorted `(name hash, value hash)`, and the sorted hashes of
/// the names whose value holds the marker.
#[derive(Debug, Default)]
pub(super) struct Hashed {
    pub fields: Vec<(u64, u64)>,
    pub marked: Vec<u64>,
}

fn hash_values(values: &[String]) -> u64 {
    // A string is never `0xFF` in UTF-8: the separator cannot be part of a value.
    xxh3_64(values.join("\u{ff}").as_bytes())
}

impl Fields {
    fn add(&mut self, name: String, star: String, value: String) {
        let marked = value.contains(MARKER);
        let entry = self.entries.entry(name).or_insert_with(|| Entry {
            star,
            values: Vec::new(),
            marked: false,
        });
        entry.marked |= marked;
        entry.values.push(value);
    }

    /// `query/<key>`, values in order.
    pub(super) fn query<'a>(&mut self, pairs: impl IntoIterator<Item = (&'a str, &'a str)>) {
        for (key, value) in pairs {
            let name = format!("query/{key}");
            self.add(name.clone(), name, value.to_string());
        }
    }

    /// `request.body/<media>/<key>` of a form.
    pub(super) fn form<'a>(
        &mut self,
        media: &str,
        pairs: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) {
        for (key, value) in pairs {
            let name = format!("request.body/{media}/{key}");
            self.add(name.clone(), name, value.to_string());
        }
    }

    /// `request.body/<media><pointer>` of every scalar leaf of a JSON body, as the recorder
    /// names them.
    pub(super) fn json(&mut self, media: &str, value: &Value) {
        let prefix = format!("request.body/{media}");
        self.leaves(&prefix, (String::new(), String::new()), value, 1);
    }

    fn leaves(&mut self, prefix: &str, at: (String, String), value: &Value, level: usize) {
        if level > MAX_LEVEL {
            return;
        }
        let (pointer, star) = &at;
        match value {
            Value::Object(map) => {
                for (key, v) in map {
                    let token = pointer_token(cut_name(key));
                    let next = (format!("{pointer}/{token}"), format!("{star}/{token}"));
                    self.leaves(prefix, next, v, level + 1);
                }
            }
            Value::Array(items) => {
                for (i, v) in items.iter().take(MAX_ITEMS).enumerate() {
                    let next = (format!("{pointer}/{i}"), format!("{star}/*"));
                    self.leaves(prefix, next, v, level + 1);
                }
            }
            leaf => self.add(
                format!("{prefix}{pointer}"),
                format!("{prefix}{star}"),
                leaf.to_string(),
            ),
        }
    }

    /// The fields to compare for an endpoint: those whose name is not in `excluded`, and
    /// `path/<n>` for the segments of `path` at the parameter `positions` of its template.
    pub(super) fn hashed(
        &self,
        excluded: &BTreeSet<String>,
        path: (&[String], &[usize]),
    ) -> Hashed {
        let mut out = Hashed::default();
        let mut push = |name: &str, values: &[String], marked: bool| {
            let name = xxh3_64(name.as_bytes());
            out.fields.push((name, hash_values(values)));
            if marked {
                out.marked.push(name);
            }
        };
        for (name, entry) in &self.entries {
            if !excluded.contains(entry.star.as_str()) {
                push(name, &entry.values, entry.marked);
            }
        }
        let (segments, positions) = path;
        for position in positions {
            if let Some(segment) = segments.get(*position) {
                let marked = segment.contains(MARKER);
                push(
                    &format!("path/{}", position + 1),
                    std::slice::from_ref(segment),
                    marked,
                );
            }
        }
        out.fields.sort_unstable();
        out.marked.sort_unstable();
        out.marked.dedup();
        out
    }
}

/// A request as it is looked up: redacted as the recorder redacts an outbound request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    authority: String,
    path: String,
    segments: Vec<String>,
    grpc: bool,
    media: Option<String>,
    fields: Fields,
}

impl Signature {
    /// The scope key of the request (spec §6.1).
    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// The path, redacted.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The query and body fields compared, by name, with their values in order.
    pub fn fields(&self) -> BTreeMap<&str, &[String]> {
        self.fields
            .entries
            .iter()
            .map(|(name, entry)| (name.as_str(), entry.values.as_slice()))
            .collect()
    }

    pub(super) fn is_grpc(&self) -> bool {
        self.grpc
    }

    pub(super) fn segments(&self) -> &[String] {
        &self.segments
    }

    /// The media type of the parsed body, when it has one.
    pub(super) fn media(&self) -> Option<&str> {
        self.media.as_deref()
    }

    pub(super) fn request_fields(&self) -> &Fields {
        &self.fields
    }
}

/// A gRPC request: HTTP/2 with a `content-type` of `application/grpc` or `application/grpc+…`.
fn is_grpc(req: &Request<'_>) -> bool {
    req.h2
        && req
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .any(|(_, value)| {
                let media = value.split(';').next().unwrap_or_default().trim();
                media.to_ascii_lowercase().starts_with("application/grpc")
            })
}

/// The signature of a request. `redactor` holds the names of `MIMIC_DISCOVERY_REDACT_NAMES`;
/// the outbound request view of spec §6.1 is added here.
pub fn signature(redactor: &Redactor, req: &Request<'_>) -> Signature {
    let redactor = redactor.outbound_request();
    let default_port = if req.tls { 443 } else { 80 };
    let authority = authority_key(req.target, default_port, req.sni, req.dst);
    let path = redact::path(req.path);
    let segments = split_path(&path).into_iter().map(str::to_string).collect();

    let mut fields = Fields::default();
    let query = query_pairs(&redactor, req.query);
    fields.query(query.iter().map(|p| (p.key.as_str(), p.value.as_str())));
    let mut media = None;
    if let Some(parsed) = req
        .body
        .and_then(|body| parse(&redactor, req.headers, body))
    {
        match &parsed.content {
            Content::Opaque => {}
            Content::Json { value, .. } => {
                fields.json(&parsed.media, value);
                media = Some(parsed.media.clone());
            }
            Content::Form { pairs, .. } => {
                fields.form(
                    &parsed.media,
                    pairs.iter().map(|p| (p.key.as_str(), p.value.as_str())),
                );
                media = Some(parsed.media.clone());
            }
        }
    }
    Signature {
        authority,
        path,
        segments,
        grpc: is_grpc(req),
        media,
        fields,
    }
}
