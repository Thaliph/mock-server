//! Samples (spec §6): at most 20 redacted exchanges per endpoint. The choice is a function of
//! the set of candidates, so any arrival order, and any partition merged later, gives the
//! same samples.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use xxhash_rust::xxh3::xxh3_64;

use super::observed::{Content, Parsed};
use super::redact::{self, Redactor};
use super::report::Exchange;
use super::schema::{Pair, header_pairs, query_pairs};

/// Samples kept per endpoint.
pub const MAX_SAMPLES: usize = 20;

/// A parsed body. Opaque and oversized bodies are not kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Body {
    media: String,
    value: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Option<Body>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Option<Body>,
}

/// One redacted exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    request: Request,
    response: Response,
    /// Time to the response head, in milliseconds. Not part of the content hash.
    head_ms: u64,
    request_shape: u64,
    response_shape: u64,
    /// Serialised size, which the sample budget counts.
    bytes: usize,
}

fn pairs(pairs: Vec<Pair>) -> Vec<(String, String)> {
    pairs.into_iter().map(|p| (p.key, p.value)).collect()
}

fn body(parsed: Option<Parsed>) -> Option<Body> {
    let parsed = parsed?;
    let value = match parsed.content {
        Content::Opaque => return None,
        Content::Json { value, .. } => value,
        Content::Form { pairs, .. } => Value::Array(
            pairs
                .into_iter()
                .map(|p| Value::Array(vec![p.key.into(), p.value.into()]))
                .collect(),
        ),
    };
    Some(Body {
        media: parsed.media,
        value,
    })
}

impl Sample {
    /// The sample of an exchange and the XXH3 hash of its redacted content (latency aside).
    pub(super) fn new(
        redactor: &Redactor,
        exchange: &Exchange<'_>,
        request_body: Option<Parsed>,
        response_body: Option<Parsed>,
    ) -> (u64, Self) {
        let o = &exchange.observation;
        let request_shape = request_body.as_ref().map_or(0, |p| p.shape);
        let response_shape = response_body.as_ref().map_or(0, |p| p.shape);
        let request = Request {
            method: exchange.method.to_string(),
            path: redact::path(exchange.path),
            query: pairs(query_pairs(redactor, o.query)),
            headers: pairs(header_pairs(redactor, o.req_headers)),
            body: body(request_body),
        };
        let response = Response {
            status: exchange.status,
            headers: pairs(header_pairs(redactor, o.resp_headers)),
            body: body(response_body),
        };
        // Plain data: serialisation cannot fail.
        let content = serde_json::to_vec(&(&request, &response)).unwrap_or_default();
        let hash = xxh3_64(&content);
        // The size is taken with a zero latency, so that merging latencies keeps it stable.
        let mut sample = Sample {
            request,
            response,
            head_ms: 0,
            request_shape,
            response_shape,
            bytes: 0,
        };
        sample.bytes = serde_json::to_vec(&sample).map_or(0, |v| v.len());
        sample.head_ms = exchange.head_ms;
        (hash, sample)
    }

    pub fn status(&self) -> u16 {
        self.response.status
    }

    /// Hash of the request body schema skeleton; 0 without a parsed body.
    pub fn request_shape(&self) -> u64 {
        self.request_shape
    }

    /// Hash of the response body schema skeleton; 0 without a parsed body.
    pub fn response_shape(&self) -> u64 {
        self.response_shape
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    fn class(&self) -> (u16, u64, u64) {
        (self.status(), self.request_shape, self.response_shape)
    }
}

/// The samples of one endpoint, by content hash.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Samples(BTreeMap<u64, Sample>);

impl Samples {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, hash: u64) -> bool {
        self.0.contains_key(&hash)
    }

    /// Samples by content hash.
    pub fn iter(&self) -> impl Iterator<Item = (u64, &Sample)> {
        self.0.iter().map(|(h, s)| (*h, s))
    }

    /// Serialised size of the samples.
    pub fn bytes(&self) -> usize {
        self.0.values().map(Sample::bytes).sum()
    }

    pub(super) fn single(hash: u64, sample: Sample) -> Self {
        Samples(BTreeMap::from([(hash, sample)]))
    }

    /// The union, then the selection. The same content seen twice keeps the lowest latency.
    pub fn merge(mut self, other: Self) -> Self {
        for (hash, sample) in other.0 {
            match self.0.get_mut(&hash) {
                Some(mine) => mine.head_ms = mine.head_ms.min(sample.head_ms),
                None => {
                    self.0.insert(hash, sample);
                }
            }
        }
        self.0 = select(self.0);
        self
    }
}

/// At most `MAX_SAMPLES`: one per status code (lowest status first), then one per class
/// (status, request shape, response shape) not yet represented, then the rest; in each pass
/// the lowest content hash wins.
fn select(all: BTreeMap<u64, Sample>) -> BTreeMap<u64, Sample> {
    if all.len() <= MAX_SAMPLES {
        return all;
    }
    let mut by_status: BTreeMap<u16, u64> = BTreeMap::new();
    let mut by_class: BTreeMap<(u16, u64, u64), u64> = BTreeMap::new();
    // Ascending hashes: the first of each key is its lowest.
    for (hash, s) in &all {
        by_status.entry(s.status()).or_insert(*hash);
        by_class.entry(s.class()).or_insert(*hash);
    }
    let mut keep: BTreeSet<u64> = BTreeSet::new();
    let mut classes = BTreeSet::new();
    let mut take = |hash: u64, classes: &mut BTreeSet<(u16, u64, u64)>| {
        if keep.len() < MAX_SAMPLES && keep.insert(hash) {
            classes.extend(all.get(&hash).map(Sample::class));
        }
    };
    for hash in by_status.into_values() {
        take(hash, &mut classes);
    }
    for (class, hash) in by_class {
        if !classes.contains(&class) {
            take(hash, &mut classes);
        }
    }
    for hash in all.keys() {
        take(*hash, &mut classes);
    }
    all.into_iter().filter(|(h, _)| keep.contains(h)).collect()
}
