//! The snapshot index and the lookup (spec §8): the endpoints of each authority, and per
//! sample its answer (serialised once, behind an `Arc`) and the hashes of the fields a request
//! is compared on. The index replaces the typed set; it is not held twice.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::Value;

use crate::Mode;
use crate::compact::{Name, SortedMap};
use crate::discovery::egress::{EgressSet, Endpoint, ViewAuthority, ViewEndpoint, render_summary};
use crate::discovery::histogram::Histogram;
use crate::discovery::redact::Redactor;
use crate::discovery::sample::{Payload, Sample};
use crate::discovery::template::split_path;
use crate::grpc::frame::encode_frame;
use crate::grpc::wire::Message;

use super::Step;
use super::signature::{Fields, Hashed, Signature, signature};
use super::{Answer, Hit, Miss, Request, Response, Tier};

/// Headers of a sample that an answer never carries: framing, and hop-by-hop ones. The answer
/// sets its own `content-length`.
const DROPPED: [&str; 9] = [
    "content-length",
    "content-encoding",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "upgrade",
    "proxy-connection",
];

/// `00 00 00 00 00`: a gRPC message of length zero.
const EMPTY_MESSAGE: [u8; 5] = [0; 5];

#[derive(Debug)]
struct IndexSample {
    /// The media type of the parsed request body.
    media: Option<Box<str>>,
    fields: Vec<(u64, u64)>,
    marked: Vec<u64>,
    answer: Arc<Answer>,
}

#[derive(Debug)]
struct IndexEndpoint {
    grpc: bool,
    method: Box<str>,
    template: Box<str>,
    /// The template's segments: `None` is a parameter.
    segments: Vec<Option<Box<str>>>,
    /// Positions of the parameters.
    parameters: Vec<usize>,
    /// Fields never compared: the endpoint's `volatile` and `redacted` names.
    excluded: BTreeSet<String>,
    latency: Histogram,
    /// The statuses seen and their counts, for the local view (spec §7.7).
    statuses: SortedMap<u16, u64>,
    /// Samples that a mock can replay (no `1xx`), in snapshot order.
    samples: Vec<IndexSample>,
    /// The answer of the most frequent status; none when the endpoint has only `1xx`.
    fallback: Option<Arc<Answer>>,
}

/// The egress snapshot, ready for lookups.
#[derive(Debug)]
pub struct Index {
    authorities: SortedMap<Name, Vec<IndexEndpoint>>,
}

fn is_parameter(segment: &str) -> bool {
    segment.len() >= 2 && segment.starts_with('{') && segment.ends_with('}')
}

/// A form encoded as `application/x-www-form-urlencoded`: unreserved bytes as they are, a
/// space as `+`, the rest as `%XX`.
fn encode_form(pairs: &[(String, String)]) -> Vec<u8> {
    fn push(out: &mut Vec<u8>, text: &str) {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        for b in text.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => out.push(b),
                b' ' => out.push(b'+'),
                _ => out.extend([b'%', HEX[usize::from(b >> 4)], HEX[usize::from(b & 15)]]),
            }
        }
    }
    let mut out = Vec::new();
    for (i, (key, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(b'&');
        }
        push(&mut out, key);
        out.push(b'=');
        push(&mut out, value);
    }
    out
}

/// Whether a recorded header or trailer is replayed in a gRPC answer: not `-bin` metadata (its
/// value is a placeholder, not base64), not a framing name, not a pseudo-header.
fn replayed(name: &str) -> bool {
    !DROPPED.contains(&name) && !name.ends_with("-bin") && !name.starts_with(':')
}

/// The answer to a gRPC call (spec §8): status 200, the headers, the response message framed,
/// and the trailers with `grpc-status` first.
fn grpc_answer(sample: &Sample) -> Answer {
    let mut headers: Vec<(Box<str>, Box<str>)> = sample
        .response_headers()
        .iter()
        .filter(|(name, _)| {
            replayed(name) && !matches!(name.as_str(), "grpc-status" | "grpc-message")
        })
        .map(|(name, value)| (name.as_str().into(), value.as_str().into()))
        .collect();
    if !headers.iter().any(|(name, _)| &**name == "content-type") {
        headers.insert(0, ("content-type".into(), "application/grpc".into()));
    }
    // The status and the message: the trailers, else the response headers of a trailers-only
    // response.
    let find = |name: &str| {
        let from = |list: &[(String, String)]| {
            list.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone())
        };
        from(sample.trailers()).or_else(|| from(sample.response_headers()))
    };
    let status = find("grpc-status").unwrap_or_else(|| "0".to_string());
    let mut trailers: Vec<(Box<str>, Box<str>)> =
        vec![("grpc-status".into(), status.as_str().into())];
    if let Some(message) = find("grpc-message") {
        trailers.push(("grpc-message".into(), message.into()));
    }
    trailers.extend(
        sample
            .trailers()
            .iter()
            .filter(|(name, _)| {
                replayed(name) && !matches!(name.as_str(), "grpc-status" | "grpc-message")
            })
            .map(|(name, value)| (name.as_str().into(), value.as_str().into())),
    );
    let message = sample.response_message().and_then(|(_, json)| {
        let payload = Message::from_json(&json).ok()?.encode().ok()?;
        encode_frame(false, &payload).ok()
    });
    let body: Box<[u8]> = match message {
        Some(frame) => frame.into(),
        // No recorded message: the empty one after a success, none after an error.
        None if status == "0" => Box::new(EMPTY_MESSAGE),
        None => Box::default(),
    };
    // The messages after the first, as frames of their own; the list ends at one that does not
    // encode (the recorder wrote canonical messages, so none).
    let steps = sample
        .response_stream()
        .into_iter()
        .map_while(|(delay_ms, json)| {
            let payload = Message::from_json(&json).ok()?.encode().ok()?;
            let frame = encode_frame(false, &payload).ok()?;
            Some(Step {
                delay_ms,
                frame: frame.into(),
            })
        })
        .collect();
    Answer {
        status: 200,
        headers,
        body,
        grpc: true,
        trailers,
        steps,
    }
}

fn answer_of(sample: &Sample, grpc: bool) -> Answer {
    if grpc {
        return grpc_answer(sample);
    }
    let headers = sample
        .response_headers()
        .iter()
        .filter(|(name, _)| !DROPPED.contains(&name.as_str()))
        .map(|(name, value)| (name.as_str().into(), value.as_str().into()))
        .collect();
    let body: Box<[u8]> = match sample.response_payload() {
        Some((_, Payload::Json(text))) => text.as_bytes().into(),
        Some((_, Payload::Form(pairs))) => encode_form(&pairs).into(),
        None => Box::default(),
    };
    Answer {
        status: sample.status(),
        headers,
        body,
        grpc: false,
        trailers: Vec::new(),
        steps: Vec::new(),
    }
}

/// The answer for a status that no sample holds: the status alone, and an empty body.
fn bare_answer(status: u16, grpc: bool) -> Answer {
    let headers = if grpc {
        vec![("content-type".into(), "application/grpc".into())]
    } else {
        Vec::new()
    };
    Answer {
        status: if grpc { 200 } else { status },
        headers,
        body: if grpc {
            Box::new(EMPTY_MESSAGE)
        } else {
            Box::default()
        },
        grpc,
        trailers: if grpc {
            vec![("grpc-status".into(), "0".into())]
        } else {
            Vec::new()
        },
        steps: Vec::new(),
    }
}

fn index_sample(
    sample: &Sample,
    e: &Endpoint,
    parameters: &[usize],
    excluded: &BTreeSet<String>,
) -> IndexSample {
    let mut fields = Fields::default();
    fields.query(sample.query().iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let mut media = None;
    if let Some((m, payload)) = sample.request_payload() {
        match payload {
            Payload::Json(text) => {
                if let Ok(value) = serde_json::from_str::<Value>(text) {
                    fields.json(m, &value);
                }
            }
            Payload::Form(pairs) => {
                fields.form(m, pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            }
        }
        media = Some(m.into());
    } else if let Some((m, json)) = sample.request_message() {
        fields.json(m, &json);
        media = Some(m.into());
    }
    let segments: Vec<String> = split_path(sample.path())
        .into_iter()
        .map(str::to_string)
        .collect();
    let Hashed { fields, marked } = fields.hashed(excluded, (&segments, parameters));
    IndexSample {
        media,
        fields,
        marked,
        answer: Arc::new(answer_of(sample, e.grpc)),
    }
}

fn index_endpoint(e: Endpoint) -> IndexEndpoint {
    let segments: Vec<Option<Box<str>>> = split_path(&e.template)
        .into_iter()
        .map(|s| (!is_parameter(s)).then(|| s.into()))
        .collect();
    let parameters: Vec<usize> = segments
        .iter()
        .enumerate()
        .filter(|(_, s)| s.is_none())
        .map(|(i, _)| i)
        .collect();
    let excluded: BTreeSet<String> = e.volatile.iter().chain(&e.redacted).cloned().collect();
    let mut kept: Vec<(u16, IndexSample)> = Vec::new();
    for sample in e.samples.iter().filter(|s| s.status() >= 200) {
        kept.push((
            sample.status(),
            index_sample(sample, &e, &parameters, &excluded),
        ));
    }
    // The most frequent status other than `1xx`, the lowest on a tie.
    let mut best: Option<(u16, u64)> = None;
    for (status, count) in e.statuses.iter().filter(|(s, _)| **s >= 200) {
        if best.is_none_or(|(_, n)| *count > n) {
            best = Some((*status, *count));
        }
    }
    let fallback = best.map(
        |(status, _)| match kept.iter().find(|(s, _)| *s == status) {
            Some((_, sample)) => Arc::clone(&sample.answer),
            None => Arc::new(bare_answer(status, e.grpc)),
        },
    );
    IndexEndpoint {
        grpc: e.grpc,
        method: e.method.as_str().into(),
        template: e.template.as_str().into(),
        segments,
        parameters,
        excluded,
        latency: e.latency,
        statuses: e.statuses,
        samples: kept.into_iter().map(|(_, s)| s).collect(),
        fallback,
    }
}

/// How a request compares with a sample: `(exact, shared fields)`. A field whose value holds
/// the marker on either side is left out on both.
fn compare(request: &Hashed, media: Option<&str>, sample: &IndexSample) -> (bool, usize) {
    let dropped = |name: u64| {
        request.marked.binary_search(&name).is_ok() || sample.marked.binary_search(&name).is_ok()
    };
    let mine = request.fields.iter().filter(|(name, _)| !dropped(*name));
    let theirs = sample.fields.iter().filter(|(name, _)| !dropped(*name));
    let (mut mine, mut theirs) = (mine.peekable(), theirs.peekable());
    let (mut exact, mut shared) = (media == sample.media.as_deref(), 0);
    loop {
        match (mine.peek(), theirs.peek()) {
            (None, None) => break,
            (Some(_), None) => {
                exact = false;
                mine.next();
            }
            (None, Some(_)) => {
                exact = false;
                theirs.next();
            }
            (Some((a, av)), Some((b, bv))) => match a.cmp(b) {
                std::cmp::Ordering::Less => {
                    exact = false;
                    mine.next();
                }
                std::cmp::Ordering::Greater => {
                    exact = false;
                    theirs.next();
                }
                std::cmp::Ordering::Equal => {
                    if av == bv {
                        shared += 1;
                    } else {
                        exact = false;
                    }
                    mine.next();
                    theirs.next();
                }
            },
        }
    }
    (exact, shared)
}

impl IndexEndpoint {
    /// Whether the endpoint takes a request of `method` and the path of `signature`, and how
    /// specific its template is: one flag per segment, a parameter after a literal.
    fn takes(&self, method: &str, signature: &Signature) -> Option<Vec<bool>> {
        if &*self.method != method || self.grpc != signature.is_grpc() {
            return None;
        }
        if self.grpc {
            return (&*self.template == signature.path()).then(Vec::new);
        }
        let segments = signature.segments();
        if self.segments.len() != segments.len() {
            return None;
        }
        let mut specificity = Vec::with_capacity(segments.len());
        for (template, segment) in self.segments.iter().zip(segments) {
            match template {
                Some(literal) if **literal != **segment => return None,
                Some(_) => specificity.push(false),
                None => specificity.push(true),
            }
        }
        Some(specificity)
    }

    /// The tiers of spec §8; `None` when the endpoint has nothing to answer with.
    fn answer(&self, signature: &Signature) -> Option<(Tier, &Arc<Answer>)> {
        let request = signature
            .request_fields()
            .hashed(&self.excluded, (signature.segments(), &self.parameters));
        let mut best: Option<(usize, &IndexSample)> = None;
        for sample in &self.samples {
            let (exact, shared) = compare(&request, signature.media(), sample);
            if exact {
                return Some((Tier::Exact, &sample.answer));
            }
            if shared > best.map_or(0, |(n, _)| n) {
                best = Some((shared, sample));
            }
        }
        if let Some((_, sample)) = best {
            return Some((Tier::Endpoint, &sample.answer));
        }
        self.fallback.as_ref().map(|a| (Tier::Fallback, a))
    }
}

impl Index {
    /// Builds the index of an accepted snapshot, which it consumes.
    pub fn build(set: EgressSet) -> Index {
        let authorities = set
            .authorities
            .into_iter()
            .map(|(name, a)| (name, a.endpoints.into_iter().map(index_endpoint).collect()))
            .collect();
        Index { authorities }
    }

    pub fn endpoint_count(&self) -> usize {
        self.authorities.values().map(Vec::len).sum()
    }

    pub fn authority_count(&self) -> usize {
        self.authorities.len()
    }

    /// The local view of the snapshot (spec §7.7), the same as
    /// [`egress::summary`](crate::discovery::egress::summary) gives for the set it was built from.
    pub fn summary(&self, etag: &str, pulled: &str, mode: Mode, factor: u32) -> String {
        let authorities = self
            .authorities
            .iter()
            .map(|(name, endpoints)| {
                let endpoints = endpoints
                    .iter()
                    .map(|e| ViewEndpoint {
                        exchanges: e.statuses.values().fold(0, |n, c| n.saturating_add(*c)),
                        grpc: e.grpc,
                        method: &e.method,
                        statuses: &e.statuses,
                        template: &e.template,
                    })
                    .collect();
                (name.as_str(), ViewAuthority { endpoints })
            })
            .collect();
        render_summary(authorities, etag, pulled, mode, factor)
    }

    /// The answer to a request, or why there is none (spec §8, "Endpoint lookup" and "Tiers").
    pub fn lookup<'a>(&'a self, redactor: &Redactor, req: &Request<'_>) -> Result<Hit<'a>, Miss> {
        let signature = signature(redactor, req);
        let endpoints = self
            .authorities
            .get(signature.authority())
            .ok_or(Miss::Authority)?;
        // The most specific template: a literal beats a parameter at the first position where
        // two differ; the first of the set when they are the same.
        let mut chosen: Option<(Vec<bool>, &IndexEndpoint)> = None;
        for endpoint in endpoints {
            if let Some(specificity) = endpoint.takes(req.method, &signature)
                && chosen.as_ref().is_none_or(|(best, _)| specificity < *best)
            {
                chosen = Some((specificity, endpoint));
            }
        }
        let (_, endpoint) = chosen.ok_or(Miss::Endpoint)?;
        let (tier, answer) = endpoint.answer(&signature).ok_or(Miss::Endpoint)?;
        Ok(Hit {
            tier,
            answer: Arc::clone(answer),
            latency: &endpoint.latency,
            method: &endpoint.method,
            template: &endpoint.template,
        })
    }
}

/// The response to a request of `method` (spec §8, "Answer").
pub(super) fn respond(answer: &Arc<Answer>, tier: Tier, method: &str) -> Response {
    let bodiless = method == "HEAD" || matches!(answer.status, 204 | 304);
    let length = if bodiless { 0 } else { answer.body.len() };
    let mut headers: Vec<(String, String)> = answer
        .headers
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    // The length of a stream is not announced (spec §8).
    if bodiless || answer.steps.is_empty() {
        headers.push(("content-length".to_string(), length.to_string()));
    }
    headers.push(("x-mimic-match".to_string(), tier.as_str().to_string()));
    let trailers = answer
        .trailers
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    Response {
        status: answer.status,
        headers,
        trailers,
        body: (!bodiless).then(|| Arc::clone(answer)),
    }
}
