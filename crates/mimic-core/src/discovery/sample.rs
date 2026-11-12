//! Samples (spec §6): at most 20 redacted exchanges per endpoint. The choice is a function of
//! the set of candidates, so any arrival order, and any partition merged later, gives the
//! same samples.

use crate::compact::{Name, SortedMap, SortedSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;
use xxhash_rust::xxh3::xxh3_64;

use crate::grpc::record::{MAX_MESSAGES, bucketed, is_protobuf_media, single_message, steps};
use crate::grpc::redact::{Direction, FieldNames, GrpcRedactor};
use crate::grpc::wire::Message;

use super::check::Violation;
use super::observed::{
    Content, MAX_BODY, Parsed, Seen, body_shape, grpc_redacted, observe_leaves, observe_pairs,
    parsed_media,
};
use super::placeholder::Placeholder;
use super::redact::{self, Redactor};
use super::report::Exchange;
use super::schema::{Pair, header_pairs, query_pairs};
use super::volatile::Volatility;

/// Samples kept per endpoint.
pub const MAX_SAMPLES: usize = 20;
/// Trailers kept per sample (spec §6.4).
pub const MAX_TRAILERS: usize = 64;
/// Stream steps kept per sample: the messages after the first (spec §6.4).
pub const MAX_STEPS: usize = MAX_MESSAGES - 1;

/// A JSON value kept as its canonical text (compact, object keys sorted: what `serde_json`
/// writes a `Value` as), which costs the length of the text instead of a tree of maps and
/// strings (#339, SHR-22). Read again from any text, it is canonicalised; the redaction, the
/// shape and the examples parse it for the time they need it.
#[derive(Debug, Clone)]
struct Text(Box<RawValue>);

impl Text {
    fn new(value: &Value) -> Self {
        // Plain data: serialisation cannot fail, and its output is valid JSON.
        let text = serde_json::to_string(value).unwrap_or_default();
        Self(RawValue::from_string(text).unwrap_or_else(|_| null()))
    }

    fn value(&self) -> Value {
        serde_json::from_str(self.0.get()).unwrap_or(Value::Null)
    }

    /// The length of the text, which is the serialised length of the value.
    fn len(&self) -> usize {
        self.0.get().len()
    }
}

fn null() -> Box<RawValue> {
    RawValue::NULL.to_owned()
}

impl PartialEq for Text {
    fn eq(&self, other: &Self) -> bool {
        self.0.get() == other.0.get()
    }
}

impl Eq for Text {}

impl Serialize for Text {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Value::deserialize(deserializer).map(|value| Self::new(&value))
    }
}

/// The pairs of a form value, an array of `[key, value]`.
fn form_pairs(value: &Value) -> Vec<Pair> {
    let items = value.as_array().map_or(&[][..], Vec::as_slice);
    items
        .iter()
        .filter_map(|item| match item.as_array()?.as_slice() {
            [Value::String(key), Value::String(value)] => Some(Pair {
                key: key.clone(),
                value: value.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// A body as a mock replays it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload<'a> {
    /// The canonical (compact) text of the JSON value.
    Json(&'a str),
    /// The pairs of a form, in order.
    Form(Vec<(String, String)>),
}

/// A parsed body. Opaque and oversized bodies are not kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    media: String,
    value: Text,
}

impl Body {
    /// The pairs of a form body, which is stored as an array of `[key, value]`.
    fn form(&self) -> Option<Vec<Pair>> {
        if self.media != "application/x-www-form-urlencoded" {
            return None;
        }
        Some(form_pairs(&self.value.value()))
    }

    fn payload(&self) -> (&str, Payload<'_>) {
        let content = match self.form() {
            Some(pairs) => Payload::Form(pairs.into_iter().map(|p| (p.key, p.value)).collect()),
            None => Payload::Json(self.value.0.get()),
        };
        (&self.media, content)
    }

    /// The media type and the value as an example: a form is an object of field to value, a
    /// repeated field an array of values.
    fn example(&self) -> (&str, Value) {
        let Some(pairs) = self.form() else {
            return (&self.media, self.value.value());
        };
        let mut object = serde_json::Map::new();
        for Pair { key, value } in pairs {
            let value = Value::String(value);
            match object.remove(&key) {
                None => object.insert(key, value),
                Some(Value::Array(mut values)) => {
                    values.push(value);
                    object.insert(key, Value::Array(values))
                }
                Some(first) => object.insert(key, Value::Array(vec![first, value])),
            };
        }
        (&self.media, Value::Object(object))
    }

    /// The body is parsed for the redaction only, and kept as text again.
    fn redact(&mut self, redactor: &Redactor) {
        let mut value = self.value.value();
        if is_protobuf_media(&self.media) {
            // A message again (spec §7.3): what is not one is not kept.
            let redacted = Message::from_json(&value).ok().and_then(|message| {
                GrpcRedactor::new(redactor.clone(), Vec::new())
                    .message("", Direction::Request, &message, &FieldNames::new())
                    .ok()
            });
            self.value = Text::new(&redacted.map_or_else(
                || Value::String(Placeholder::Secret.to_string()),
                |m| m.to_json(),
            ));
            return;
        }
        if self.media != "application/x-www-form-urlencoded" {
            redactor.json(&mut value);
            self.value = Text::new(&value);
            return;
        }
        let pairs = form_pairs(&value);
        // A form that does not have the shape the recorder gives it is not kept.
        if value.as_array().is_none_or(|a| a.len() != pairs.len()) {
            self.value = Text::new(&Value::String(Placeholder::Secret.to_string()));
            return;
        }
        self.value = Text::new(&Value::Array(
            pairs
                .into_iter()
                .map(|p| {
                    let value = redactor.field(&p.key, &p.value);
                    Value::Array(vec![p.key.into(), value.into()])
                })
                .collect(),
        ));
    }

    fn shape(&self) -> u64 {
        let value = self.value.value();
        let form = (self.media == "application/x-www-form-urlencoded").then(|| form_pairs(&value));
        body_shape(&self.media, &value, form)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Option<Body>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Option<Body>,
    /// HTTP/2 trailers, redacted as gRPC metadata; written only when there are some (spec §6.4).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    trailers: Vec<(String, String)>,
    /// The gRPC messages after the first, with their bucketed delays (spec §6.4); written only
    /// when there are some.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    stream: Vec<Step>,
}

/// A response message after the first of a gRPC stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    /// The time since the message before it, at the upper bound of its latency bucket.
    delay_ms: u64,
    message: Text,
}

/// A gRPC message as its canonical JSON read again and redacted (spec §6.3); what is not a
/// message is the secret placeholder.
fn redact_message(redactor: &Redactor, message: &Text) -> Text {
    let redacted = Message::from_json(&message.value())
        .ok()
        .and_then(|m| grpc_redacted(redactor, &m));
    Text::new(&redacted.unwrap_or_else(|| Value::String(Placeholder::Secret.to_string())))
}

/// The steps of a gRPC stream (spec §6.4): the messages after the first of the response, read
/// and redacted as the first is, within 256 KiB of canonical JSON with it.
fn stream_steps(
    redactor: &Redactor,
    o: &super::observed::Observation<'_>,
    first: Option<&Body>,
) -> Vec<Step> {
    let (Some(first), Some(request), Some(response)) = (first, o.req_body, o.resp_body) else {
        return Vec::new();
    };
    if !is_protobuf_media(&first.media) || !single_message(request) {
        return Vec::new();
    }
    let encoding = o
        .resp_headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("grpc-encoding"))
        .map(|(_, v)| *v);
    let mut size = first.value.len();
    let mut out = Vec::new();
    for step in steps(encoding, response, o.resp_marks) {
        let Some(value) = grpc_redacted(redactor, &step.message) else {
            break;
        };
        let message = Text::new(&value);
        size = size.saturating_add(message.len());
        if size > MAX_BODY {
            break;
        }
        out.push(Step {
            delay_ms: step.delay_ms,
            message,
        });
    }
    out
}

/// One redacted exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sample {
    request: Request,
    response: Response,
    /// Time to the response head, in milliseconds. Not part of the content hash.
    head_ms: u64,
    request_shape: u64,
    response_shape: u64,
    /// Serialised size, which the sample budget counts.
    bytes: usize,
    /// The UTC day of the last push that held the sample (spec §7.3, stamping).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_seen: Option<u64>,
}

fn pairs(pairs: Vec<Pair>) -> Vec<(String, String)> {
    pairs.into_iter().map(|p| (p.key, p.value)).collect()
}

/// A `-bin` metadata value is redacted whatever its name (spec §6.3).
fn bin_values(redactor: &Redactor, list: &mut [(String, String)]) {
    let grpc = GrpcRedactor::new(redactor.clone(), Vec::new());
    for (name, value) in list.iter_mut().filter(|(n, _)| n.ends_with("-bin")) {
        *value = grpc.metadata(name, value);
    }
}

/// A call whose `content-type` is `application/grpc*`.
fn grpc_call(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .any(|(n, v)| n == "content-type" && v.starts_with("application/grpc"))
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
        value: Text::new(&value),
    })
}

impl Sample {
    /// The sample of an exchange and the XXH3 hash of its redacted content (latency aside).
    pub(super) fn new(
        request_redactor: &Redactor,
        response_redactor: &Redactor,
        exchange: &Exchange<'_>,
        request_body: Option<Parsed>,
        response_body: Option<Parsed>,
    ) -> (u64, Self) {
        let o = &exchange.observation;
        let request_shape = request_body.as_ref().map_or(0, |p| p.shape);
        let response_shape = response_body.as_ref().map_or(0, |p| p.shape);
        let mut request = Request {
            method: exchange.method.to_string(),
            path: redact::path(exchange.path),
            query: pairs(query_pairs(request_redactor, o.query)),
            headers: pairs(header_pairs(request_redactor, o.req_headers)),
            body: body(request_body),
        };
        let mut response = Response {
            status: exchange.status,
            headers: pairs(header_pairs(response_redactor, o.resp_headers)),
            body: body(response_body),
            trailers: pairs(header_pairs(response_redactor, o.resp_trailers)),
            stream: Vec::new(),
        };
        if grpc_call(&request.headers) {
            bin_values(request_redactor, &mut request.headers);
            bin_values(response_redactor, &mut response.headers);
            bin_values(response_redactor, &mut response.trailers);
            response.stream = stream_steps(response_redactor, o, response.body.as_ref());
        } else {
            response.trailers.clear();
        }
        response.trailers.truncate(MAX_TRAILERS);
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
            last_seen: None,
        };
        sample.bytes = serde_json::to_vec(&sample).map_or(0, |v| v.len());
        sample.head_ms = exchange.head_ms;
        (hash, sample)
    }

    /// The sample without the controller's stamp.
    pub(super) fn unstamped(&self) -> Sample {
        Sample {
            last_seen: None,
            ..self.clone()
        }
    }

    pub fn status(&self) -> u16 {
        self.response.status
    }

    /// The concrete request path, redacted.
    pub fn path(&self) -> &str {
        &self.request.path
    }

    /// The decoded query pairs, redacted, in request order.
    pub fn query(&self) -> &[(String, String)] {
        &self.request.query
    }

    /// Request headers (lower-case names), redacted.
    pub fn request_headers(&self) -> &[(String, String)] {
        &self.request.headers
    }

    /// Response headers (lower-case names), redacted.
    pub fn response_headers(&self) -> &[(String, String)] {
        &self.response.headers
    }

    /// The media type and value of the parsed request body: the JSON value, or a form as an
    /// object of field to value (an array of values for a repeated field).
    pub fn request_body(&self) -> Option<(&str, Value)> {
        self.request.body.as_ref().map(Body::example)
    }

    /// As `request_body`, for the response.
    pub fn response_body(&self) -> Option<(&str, Value)> {
        self.response.body.as_ref().map(Body::example)
    }

    /// The media type and content of the parsed request body.
    pub fn request_payload(&self) -> Option<(&str, Payload<'_>)> {
        // A gRPC message is read through `request_message` (spec §6.4).
        let body = self.request.body.as_ref();
        body.filter(|b| !is_protobuf_media(&b.media))
            .map(Body::payload)
    }

    /// As `request_payload`, for the response.
    pub fn response_payload(&self) -> Option<(&str, Payload<'_>)> {
        let body = self.response.body.as_ref();
        body.filter(|b| !is_protobuf_media(&b.media))
            .map(Body::payload)
    }

    /// The media type and canonical JSON of the first gRPC request message (spec §6.4).
    pub fn request_message(&self) -> Option<(&str, Value)> {
        let body = self.request.body.as_ref();
        body.filter(|b| is_protobuf_media(&b.media))
            .map(|b| (b.media.as_str(), b.value.value()))
    }

    /// As `request_message`, for the response.
    pub fn response_message(&self) -> Option<(&str, Value)> {
        let body = self.response.body.as_ref();
        body.filter(|b| is_protobuf_media(&b.media))
            .map(|b| (b.media.as_str(), b.value.value()))
    }

    /// The gRPC response messages after the first: the bucketed delay since the message before
    /// and the canonical JSON (spec §6.4).
    pub fn response_stream(&self) -> Vec<(u64, Value)> {
        let step = |s: &Step| (s.delay_ms, s.message.value());
        self.response.stream.iter().map(step).collect()
    }

    /// The response trailers, redacted (HTTP/2, gRPC).
    pub fn trailers(&self) -> &[(String, String)] {
        &self.response.trailers
    }

    /// The sample holds a gRPC message or trailers, which a sidecar or controller up to F13
    /// does not know (spec §6.4): the report that holds it is version 3.
    pub fn is_grpc(&self) -> bool {
        !self.response.trailers.is_empty()
            || !self.response.stream.is_empty()
            || [&self.request.body, &self.response.body]
                .into_iter()
                .flatten()
                .any(|b| is_protobuf_media(&b.media))
    }

    /// The sample as a sidecar up to F13 reads it: no trailers and no gRPC body, size and
    /// shapes from what is left. With its content hash.
    pub(super) fn without_grpc(&self) -> (u64, Sample) {
        let mut sample = self.clone();
        sample.response.trailers.clear();
        sample.response.stream.clear();
        for body in [&mut sample.request.body, &mut sample.response.body] {
            if body.as_ref().is_some_and(|b| is_protobuf_media(&b.media)) {
                *body = None;
            }
        }
        sample.request_shape = sample.request.body.as_ref().map_or(0, Body::shape);
        sample.response_shape = sample.response.body.as_ref().map_or(0, Body::shape);
        let hash = sample.reseal();
        (hash, sample)
    }

    /// A status in 100..=599; bodies of a parsed media type and at most `MAX_BODY` bytes.
    pub(super) fn check(&self) -> Result<(), Violation> {
        if !(100..=599).contains(&self.response.status) {
            return Err(Violation::Status);
        }
        let bodies = [&self.request.body, &self.response.body];
        for body in bodies.into_iter().flatten() {
            if !parsed_media(&body.media) {
                return Err(Violation::BodyMedia);
            }
            if body.value.len() > MAX_BODY {
                return Err(Violation::BodySize);
            }
        }
        if self.response.trailers.len() > MAX_TRAILERS {
            return Err(Violation::Malformed);
        }
        self.check_stream()?;
        // `bytes` is what the sample budget counts: it must be the size of the content.
        let total = serde_json::to_vec(self).map_or(usize::MAX, |v| v.len());
        let digits = |n: String| n.len();
        let content = total
            .saturating_sub(digits(self.bytes.to_string()))
            .saturating_sub(digits(self.head_ms.to_string()))
            // The stamp (`,"last_seen":N`) is not part of the content.
            .saturating_sub(
                self.last_seen
                    .map_or(0, |day| ",\"last_seen\":".len() + digits(day.to_string())),
            )
            .saturating_add(2);
        if content != self.bytes {
            return Err(Violation::SampleBytes);
        }
        Ok(())
    }

    /// The steps of a stream (spec §7.3): at most 63, after a gRPC response message, at bucket
    /// delays, and within 256 KiB with the first message.
    fn check_stream(&self) -> Result<(), Violation> {
        let stream = &self.response.stream;
        if stream.is_empty() {
            return Ok(());
        }
        let first = self
            .response
            .body
            .as_ref()
            .filter(|b| is_protobuf_media(&b.media));
        let Some(first) = first else {
            return Err(Violation::Malformed);
        };
        if stream.len() > MAX_STEPS || stream.iter().any(|s| bucketed(s.delay_ms) != s.delay_ms) {
            return Err(Violation::Malformed);
        }
        let size = stream
            .iter()
            .fold(first.value.len(), |n, s| n.saturating_add(s.message.len()));
        if size > MAX_BODY {
            return Err(Violation::BodySize);
        }
        Ok(())
    }

    /// The §6 redaction again on the path, query, headers and bodies. True when something
    /// changed.
    pub(super) fn redact(
        &mut self,
        request_redactor: &Redactor,
        response_redactor: &Redactor,
    ) -> bool {
        let before = (self.request.clone(), self.response.clone());
        self.request.path = redact::path(&self.request.path);
        for (key, value) in &mut self.request.query {
            *value = request_redactor.field(key, value);
        }
        for (name, value) in &mut self.request.headers {
            *value = request_redactor.header(name, value);
        }
        for (name, value) in &mut self.response.headers {
            *value = response_redactor.header(name, value);
        }
        // Trailers make it a gRPC sample whatever the request says (a faulty sidecar may not tell).
        if grpc_call(&self.request.headers) || !self.response.trailers.is_empty() {
            bin_values(request_redactor, &mut self.request.headers);
            bin_values(response_redactor, &mut self.response.headers);
        }
        if !self.response.trailers.is_empty() {
            let grpc = GrpcRedactor::new(response_redactor.clone(), Vec::new());
            for (name, value) in &mut self.response.trailers {
                *value = grpc.metadata(name, value);
            }
        }
        if let Some(body) = &mut self.request.body {
            body.redact(request_redactor);
        }
        if let Some(body) = &mut self.response.body {
            body.redact(response_redactor);
        }
        for step in &mut self.response.stream {
            step.message = redact_message(response_redactor, &step.message);
        }
        let changed = before != (self.request.clone(), self.response.clone());
        if changed {
            self.request_shape = self.request.body.as_ref().map_or(0, Body::shape);
            self.response_shape = self.response.body.as_ref().map_or(0, Body::shape);
        }
        changed
    }

    /// Size and content hash from the content, as `new` computes them: nothing a pod declares
    /// is trusted.
    pub(super) fn reseal(&mut self) -> u64 {
        // Plain data: serialisation cannot fail.
        let content = serde_json::to_vec(&(&self.request, &self.response)).unwrap_or_default();
        let head_ms = std::mem::take(&mut self.head_ms);
        let last_seen = self.last_seen.take();
        self.bytes = 0;
        self.bytes = serde_json::to_vec(&*self).map_or(0, |v| v.len());
        self.head_ms = head_ms;
        self.last_seen = last_seen;
        xxh3_64(&content)
    }

    /// The volatile fields this exchange feeds, as the recorder names them.
    fn volatility(&self) -> Volatility {
        let mut fields = Volatility::default();
        let mut seen = Seen::new();
        let pairs = |p: &[(String, String)]| -> Vec<Pair> {
            p.iter()
                .map(|(key, value)| Pair {
                    key: key.clone(),
                    value: value.clone(),
                })
                .collect()
        };
        observe_pairs(&mut fields, &mut seen, "query", &pairs(&self.request.query));
        let req = pairs(&self.request.headers);
        observe_pairs(&mut fields, &mut seen, "request.header", &req);
        let resp = pairs(&self.response.headers);
        observe_pairs(&mut fields, &mut seen, "response.header", &resp);
        for (prefix, body) in [
            ("request.body", &self.request.body),
            ("response.body", &self.response.body),
        ] {
            let Some(body) = body else { continue };
            let prefix = format!("{prefix}/{}", body.media);
            match body.form() {
                Some(form) => observe_pairs(&mut fields, &mut seen, &prefix, &form),
                None => observe_leaves(&mut fields, &mut seen, &prefix, "", &body.value.value(), 1),
            }
        }
        fields
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

    pub fn last_seen(&self) -> Option<u64> {
        self.last_seen
    }

    fn class(&self) -> (u16, u64, u64) {
        (self.status(), self.request_shape, self.response_shape)
    }
}

/// The samples of one endpoint, by content hash.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Samples(SortedMap<u64, Sample>);

impl Samples {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// At most `MAX_SAMPLES`, each within its bounds.
    pub(super) fn check(&self) -> Result<(), Violation> {
        if self.0.len() > MAX_SAMPLES {
            return Err(Violation::Samples);
        }
        self.0.values().try_for_each(Sample::check)
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
        self.0
            .values()
            .fold(0, |n, s| usize::saturating_add(n, s.bytes()))
    }

    /// Sets `last_seen` on every sample.
    pub(super) fn stamp(&mut self, day: u64) {
        for sample in self.0.values_mut() {
            sample.last_seen = Some(day);
        }
    }

    /// Removes the samples that `keep` rejects; how many.
    pub(super) fn retain_samples(&mut self, mut keep: impl FnMut(&Sample) -> bool) -> u64 {
        let before = self.0.len();
        self.0.retain(|_, sample| keep(sample));
        (before - self.0.len()) as u64
    }

    /// Redacts every sample again and keys it by the hash of its new content (samples that
    /// become the same are one). Returns the volatile fields that a sample's change reaches.
    pub(super) fn re_redact(
        &mut self,
        request_redactor: &Redactor,
        response_redactor: &Redactor,
    ) -> SortedSet<Name> {
        let mut fields = SortedSet::new();
        for (_, mut sample) in std::mem::take(&mut self.0) {
            let original = sample.clone();
            if sample.redact(request_redactor, response_redactor) {
                fields.extend(original.volatility().changed(&sample.volatility()));
            }
            // A body that grows past the limit is not kept: the sample goes with it.
            if sample.check().is_err_and(|v| v == Violation::BodySize) {
                continue;
            }
            let hash = sample.reseal();
            self.absorb(hash, sample);
        }
        fields
    }

    /// The samples that hold a gRPC message or trailers.
    pub(super) fn grpc_only(&self) -> Samples {
        Samples(
            self.0
                .iter()
                .filter(|(_, s)| s.is_grpc())
                .map(|(h, s)| (*h, s.clone()))
                .collect(),
        )
    }

    /// Puts the full samples of `overlay` in place of the samples that `without_grpc` makes of
    /// them (spec §7.3, `:grpc`); a sample that has no counterpart joins while there is room.
    pub(super) fn apply(&mut self, overlay: Samples) {
        for (hash, sample) in overlay.0 {
            let (stripped, _) = sample.without_grpc();
            if self.0.remove(&stripped).is_some() || self.0.len() < MAX_SAMPLES {
                self.0.insert(hash, sample);
            }
        }
    }

    /// Whether a sample holds a gRPC message or trailers.
    pub(super) fn any_grpc(&self) -> bool {
        self.0.values().any(Sample::is_grpc)
    }

    /// The samples without their gRPC parts, keyed by their new hash (those that become the
    /// same are one).
    pub(super) fn without_grpc(&mut self) {
        for (hash, sample) in std::mem::take(&mut self.0) {
            let (hash, sample) = if sample.is_grpc() {
                sample.without_grpc()
            } else {
                (hash, sample)
            };
            self.absorb(hash, sample);
        }
    }

    fn absorb(&mut self, hash: u64, sample: Sample) {
        match self.0.get_mut(&hash) {
            Some(mine) => {
                mine.head_ms = mine.head_ms.min(sample.head_ms);
                mine.last_seen = mine.last_seen.max(sample.last_seen);
            }
            None => {
                self.0.insert(hash, sample);
            }
        }
    }

    /// Keeps the samples whose hash `keep` accepts.
    pub(super) fn retain(&mut self, mut keep: impl FnMut(u64) -> bool) {
        self.0.retain(|hash, _| keep(*hash));
    }

    pub(super) fn remove(&mut self, hash: u64) -> Option<Sample> {
        self.0.remove(&hash)
    }

    pub(super) fn insert(&mut self, hash: u64, sample: Sample) {
        self.0.insert(hash, sample);
    }

    pub(super) fn single(hash: u64, sample: Sample) -> Self {
        Samples(SortedMap::from([(hash, sample)]))
    }

    /// The union, then the selection. The same content seen twice keeps the lowest latency.
    pub fn merge(mut self, other: Self) -> Self {
        for (hash, sample) in other.0 {
            self.absorb(hash, sample);
        }
        self.0 = select(self.0);
        self
    }
}

/// At most `MAX_SAMPLES`: one per status code (lowest status first), then one per class
/// (status, request shape, response shape) not yet represented, then the rest; in each pass
/// the lowest content hash wins.
fn select(all: SortedMap<u64, Sample>) -> SortedMap<u64, Sample> {
    if all.len() <= MAX_SAMPLES {
        return all;
    }
    let mut by_status: SortedMap<u16, u64> = SortedMap::new();
    let mut by_class: SortedMap<(u16, u64, u64), u64> = SortedMap::new();
    // Ascending hashes: the first of each key is its lowest.
    for (hash, s) in &all {
        by_status.entry(s.status()).or_insert(*hash);
        by_class.entry(s.class()).or_insert(*hash);
    }
    let mut keep: SortedSet<u64> = SortedSet::new();
    let mut classes = SortedSet::new();
    let mut take = |hash: u64, classes: &mut SortedSet<(u16, u64, u64)>| {
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
