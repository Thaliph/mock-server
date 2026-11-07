//! Request building (spec §7.8 "Requests"): one request from a plan, a seed and the time.

use std::collections::BTreeMap;
use std::net::IpAddr;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Number, Value};

use super::plan::{Body, Endpoint, Plan, Sample};
use crate::discovery::redact::MARKER;

/// What the caller supplies besides the plan. `Debug` prints the Secret's key names only.
#[derive(Clone)]
pub struct Context<'a> {
    /// The value of the `mimic-load-run` header.
    pub marker: &'a str,
    /// The address of the target pod: the `host` of an endpoint without host.
    pub ip: IpAddr,
    /// Unix time, in milliseconds, for the `date-time` and `date` volatile fields.
    pub now_ms: u64,
    /// The values of the credentials Secret, by key.
    pub secrets: &'a BTreeMap<String, String>,
}

impl std::fmt::Debug for Context<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("ip", &self.ip)
            .field("now_ms", &self.now_ms)
            .field("secret_keys", &self.secrets.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// A request to send. `Debug` prints the method, the indices, the header names and the sizes:
/// the target, the header values and the body may hold credentials.
#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    /// Index in `plan.endpoints`.
    pub endpoint: usize,
    /// Index in that endpoint's `samples`.
    pub sample: usize,
    pub method: String,
    /// Path and query.
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("endpoint", &self.endpoint)
            .field("sample", &self.sample)
            .field("method", &self.method)
            .field("target_len", &self.target.len())
            .field(
                "headers",
                &self.headers.iter().map(|(n, _)| n).collect::<Vec<_>>(),
            )
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// Why no request is built. Never carries a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The plan has no endpoint.
    Empty,
    /// A credential names a key the Secret does not hold.
    MissingSecret(String),
    /// A part of the request is not valid HTTP/1.1 (`what` names it).
    Invalid(&'static str),
    /// The request would carry the redaction marker.
    Redacted,
}

/// Headers the generator sets itself or must not repeat (hop-by-hop, and the body's framing).
fn dropped(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "content-length"
            | "content-encoding"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "te"
            | "trailer"
            | "upgrade"
            | "expect"
            | "mimic-load-run"
    ) || name.starts_with("proxy-")
}

/// The random stream of a seed (SplitMix64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n > 0`).
    fn below(&mut self, n: u64) -> u64 {
        // The high half of a 128-bit product: no modulo bias worth the name.
        u64::try_from((u128::from(self.next()) * u128::from(n)) >> 64).unwrap_or(0)
    }

    fn digit(&mut self, nonzero: bool) -> char {
        let d = if nonzero {
            1 + self.below(9)
        } else {
            self.below(10)
        };
        char::from(b'0' + u8::try_from(d).unwrap_or(0))
    }

    fn letter(&mut self, upper: bool, hex: bool) -> char {
        let n = if hex { 6 } else { 26 };
        let c = b'a' + u8::try_from(self.below(n)).unwrap_or(0);
        char::from(if upper { c.to_ascii_uppercase() } else { c })
    }
}

/// Days since 1970-01-01 to (year, month, day).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn all_digits(b: &[u8]) -> bool {
    !b.is_empty() && b.iter().all(u8::is_ascii_digit)
}

/// `YYYY-MM-DD`.
fn is_date(b: &[u8]) -> bool {
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && all_digits(&b[..4])
        && all_digits(&b[5..7])
        && all_digits(&b[8..])
}

/// An RFC 3339 date-time; the number of fraction digits.
fn datetime_fraction(b: &[u8]) -> Option<usize> {
    if b.len() < 20
        || !is_date(&b[..10])
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
        || !all_digits(&b[11..13])
        || !all_digits(&b[14..16])
        || !all_digits(&b[17..19])
    {
        return None;
    }
    let mut at = 19;
    let mut fraction = 0;
    if b[at] == b'.' {
        let n = b[at + 1..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        if n == 0 {
            return None;
        }
        fraction = n;
        at += 1 + n;
    }
    let zone = &b[at..];
    let ok = matches!(zone, b"Z" | b"z")
        || (zone.len() == 6
            && matches!(zone[0], b'+' | b'-')
            && all_digits(&zone[1..3])
            && zone[3] == b':'
            && all_digits(&zone[4..]));
    ok.then_some(fraction)
}

/// The time as `YYYY-MM-DD`, and as the time of day.
fn clock(now_ms: u64) -> (String, u64, u64) {
    let days = i64::try_from(now_ms / 86_400_000).unwrap_or(0);
    let (y, m, d) = civil(days);
    let in_day = now_ms % 86_400_000;
    (
        format!("{y:04}-{m:02}-{d:02}"),
        in_day / 1000,
        in_day % 1000,
    )
}

/// A fresh value shaped like `value`.
fn fresh_text(value: &str, rng: &mut Rng, now_ms: u64) -> String {
    let b = value.as_bytes();
    let (date, secs, ms) = clock(now_ms);
    if let Some(fraction) = datetime_fraction(b) {
        let mut out = format!(
            "{date}T{:02}:{:02}:{:02}",
            secs / 3600,
            secs / 60 % 60,
            secs % 60
        );
        if fraction > 0 {
            let digits = format!("{ms:03}{}", "0".repeat(fraction.saturating_sub(3)));
            out.push('.');
            out.push_str(&digits[..fraction]);
        }
        out.push('Z');
        return out;
    }
    if is_date(b) {
        return date;
    }
    // Hex letters only when every letter of the value is a hex digit.
    let hex = value
        .chars()
        .filter(char::is_ascii_alphabetic)
        .all(|c| c.is_ascii_hexdigit());
    let integer = all_digits(b);
    let mut first = true;
    value
        .chars()
        .map(|c| {
            let out = if c.is_ascii_digit() {
                // An integer keeps its digit count: no leading zero.
                rng.digit(integer && first && b.len() > 1)
            } else if c.is_ascii_alphabetic() {
                rng.letter(c.is_ascii_uppercase(), hex)
            } else {
                c
            };
            first = false;
            out
        })
        .collect()
}

fn fresh_value(value: &mut Value, rng: &mut Rng, now_ms: u64) {
    match value {
        Value::String(s) => *s = fresh_text(s, rng, now_ms),
        Value::Number(n) => {
            let text = n.to_string();
            // Longer integers would not keep their digit count as a number.
            if text.len() > 18 {
                return;
            }
            let mut first = true;
            let fresh: String = text
                .chars()
                .map(|c| {
                    if c.is_ascii_digit() {
                        let d = rng.digit(first);
                        first = false;
                        d
                    } else {
                        c
                    }
                })
                .collect();
            if let Ok(number) = fresh.parse::<Number>() {
                *n = number;
            }
        }
        _ => {}
    }
}

/// Calls `f` on every value a pointer reaches; `*` stands for any array item.
fn visit(value: &mut Value, tokens: &[String], f: &mut impl FnMut(&mut Value)) {
    let Some((head, rest)) = tokens.split_first() else {
        f(value);
        return;
    };
    match value {
        Value::Object(map) => {
            if let Some(v) = map.get_mut(head) {
                visit(v, rest, f);
            }
        }
        Value::Array(items) if head == "*" => {
            for v in items {
                visit(v, rest, f);
            }
        }
        Value::Array(items) => {
            if let Some(v) = head.parse::<usize>().ok().and_then(|i| items.get_mut(i)) {
                visit(v, rest, f);
            }
        }
        _ => {}
    }
}

fn tokens(pointer: &str) -> Vec<String> {
    pointer
        .split('/')
        .skip(1)
        .map(|t| t.replace("~1", "/").replace("~0", "~"))
        .collect()
}

/// A volatile field of the plan.
enum Volatile<'a> {
    Query(&'a str),
    Header(&'a str),
    /// The media type and the pointer (JSON) or `/<field>` (form).
    Body(&'a str, &'a str),
}

impl<'a> Volatile<'a> {
    fn parse(field: &'a str) -> Option<Self> {
        if let Some(key) = field.strip_prefix("query/") {
            return Some(Self::Query(key));
        }
        if let Some(name) = field.strip_prefix("request.header/") {
            return Some(Self::Header(name));
        }
        let rest = field.strip_prefix("request.body/")?;
        let slash = rest.find('/')?;
        let end = rest[slash + 1..]
            .find('/')
            .map_or(rest.len(), |at| slash + 1 + at);
        Some(Self::Body(&rest[..end], &rest[end..]))
    }
}

/// The value of a Secret key, without its trailing newline.
fn secret(ctx: &Context<'_>, key: &str) -> Result<String, Error> {
    let value = ctx
        .secrets
        .get(key)
        .ok_or_else(|| Error::MissingSecret(key.to_string()))?;
    let value = value.strip_suffix('\n').unwrap_or(value);
    Ok(value.strip_suffix('\r').unwrap_or(value).to_string())
}

/// The parts of a sample a request is made of.
struct Parts {
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Option<Body>,
}

/// A body as values the volatile and credential fields can change.
enum Content {
    Json(String, Value),
    Form(String, Vec<(String, String)>),
}

fn volatile(
    endpoint: &Endpoint,
    parts: &mut Parts,
    content: &mut Option<Content>,
    rng: &mut Rng,
    now_ms: u64,
) {
    for field in &endpoint.volatile {
        match Volatile::parse(field) {
            Some(Volatile::Query(key)) => {
                for (_, v) in parts.query.iter_mut().filter(|(k, _)| k == key) {
                    *v = fresh_text(v, rng, now_ms);
                }
            }
            Some(Volatile::Header(name)) => {
                for (_, v) in parts.headers.iter_mut().filter(|(n, _)| n == name) {
                    *v = fresh_text(v, rng, now_ms);
                }
            }
            Some(Volatile::Body(media, pointer)) => match content {
                Some(Content::Json(m, value)) if m == media => {
                    visit(value, &tokens(pointer), &mut |v| {
                        fresh_value(v, rng, now_ms)
                    });
                }
                Some(Content::Form(m, pairs)) if m == media => {
                    let key = pointer.strip_prefix('/').unwrap_or(pointer);
                    for (_, v) in pairs.iter_mut().filter(|(k, _)| k == key) {
                        *v = fresh_text(v, rng, now_ms);
                    }
                }
                _ => {}
            },
            None => {}
        }
    }
}

fn credentials(
    plan: &Plan,
    ctx: &Context<'_>,
    parts: &mut Parts,
    content: &mut Option<Content>,
) -> Result<(), Error> {
    for c in &plan.credentials {
        let field = c.field.as_str();
        if let Some(name) = field.strip_prefix("header/") {
            let value = parts.headers.iter_mut().filter(|(n, _)| n == name);
            for (_, v) in value {
                *v = secret(ctx, &c.key)?;
            }
        } else if let Some(key) = field.strip_prefix("query/") {
            for (_, v) in parts.query.iter_mut().filter(|(k, _)| k == key) {
                *v = secret(ctx, &c.key)?;
            }
        } else if let Some(pointer) = field.strip_prefix("body") {
            if !(pointer.is_empty() || pointer.starts_with('/')) {
                continue;
            }
            match content {
                Some(Content::Json(_, value)) => {
                    if let Some(v) = value.pointer_mut(pointer) {
                        *v = Value::String(secret(ctx, &c.key)?);
                    }
                }
                Some(Content::Form(_, pairs)) => {
                    let key = pointer.strip_prefix('/').unwrap_or(pointer);
                    for (_, v) in pairs.iter_mut().filter(|(k, _)| k == key) {
                        *v = secret(ctx, &c.key)?;
                    }
                }
                None => {}
            }
        }
    }
    Ok(())
}

const ENCODE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

fn encode_pairs(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                utf8_percent_encode(k, ENCODE),
                utf8_percent_encode(v, ENCODE)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn token_ok(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn value_ok(s: &str) -> bool {
    s.bytes().all(|b| b == b'\t' || (b >= 0x20 && b != 0x7f))
}

/// Path and query, headers and body of a request, before the framing headers.
struct Built {
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn build(
    plan: &Plan,
    endpoint: &Endpoint,
    sample: &Sample,
    ctx: &Context<'_>,
    rng: &mut Rng,
) -> Result<Built, Error> {
    let mut parts = Parts {
        query: sample.query.clone(),
        headers: sample.headers.clone(),
        body: sample.body.clone(),
    };
    let mut content = match parts.body.take() {
        Some(Body::Json { media, text }) => Some(Content::Json(
            media,
            serde_json::from_str(&text).map_err(|_| Error::Invalid("body"))?,
        )),
        Some(Body::Form { media, pairs }) => Some(Content::Form(media, pairs)),
        None => None,
    };
    volatile(endpoint, &mut parts, &mut content, rng, ctx.now_ms);
    credentials(plan, ctx, &mut parts, &mut content)?;
    let body = match &content {
        Some(Content::Json(_, value)) => {
            serde_json::to_vec(value).map_err(|_| Error::Invalid("body"))?
        }
        Some(Content::Form(_, pairs)) => encode_pairs(pairs).into_bytes(),
        None => Vec::new(),
    };
    let mut target = sample.path.clone();
    if !parts.query.is_empty() {
        target.push('?');
        target.push_str(&encode_pairs(&parts.query));
    }
    Ok(Built {
        target,
        headers: parts.headers,
        body,
    })
}

/// The request for `seed`: an endpoint drawn by weight, then one of its samples.
pub fn request(plan: &Plan, ctx: &Context<'_>, seed: u64) -> Result<Request, Error> {
    let mut rng = Rng(seed);
    let total: u64 = plan
        .endpoints
        .iter()
        .fold(0u64, |n, e| n.saturating_add(e.weight));
    if plan.endpoints.is_empty() || total == 0 {
        return Err(Error::Empty);
    }
    let mut draw = rng.below(total);
    let index = plan
        .endpoints
        .iter()
        .position(|e| {
            if draw < e.weight {
                true
            } else {
                draw -= e.weight;
                false
            }
        })
        .unwrap_or(0);
    let endpoint = &plan.endpoints[index];
    if endpoint.samples.is_empty() {
        return Err(Error::Empty);
    }
    let pick = usize::try_from(rng.below(endpoint.samples.len() as u64)).unwrap_or(0);
    let sample = &endpoint.samples[pick];
    let Built {
        target,
        headers,
        body,
    } = build(plan, endpoint, sample, ctx, &mut rng)?;

    let host = match &endpoint.host {
        Some(host) => host.clone(),
        None => match ctx.ip {
            IpAddr::V4(ip) => format!("{ip}:{}", endpoint.port),
            IpAddr::V6(ip) => format!("[{ip}]:{}", endpoint.port),
        },
    };
    let mut out = vec![("host".to_string(), host)];
    out.extend(headers.into_iter().filter(|(n, _)| !dropped(n)));
    if !body.is_empty() || matches!(endpoint.method.as_str(), "POST" | "PUT" | "PATCH") {
        out.push(("content-length".to_string(), body.len().to_string()));
    }
    out.push(("mimic-load-run".to_string(), ctx.marker.to_string()));

    let marked = target.contains(MARKER)
        || endpoint.method.contains(MARKER)
        || out
            .iter()
            .any(|(n, v)| n.contains(MARKER) || v.contains(MARKER))
        || String::from_utf8_lossy(&body).contains(MARKER);
    if marked {
        return Err(Error::Redacted);
    }
    if !token_ok(&endpoint.method) {
        return Err(Error::Invalid("method"));
    }
    let path = sample.path.as_bytes();
    if path.first() != Some(&b'/')
        || !path
            .iter()
            .all(|b| b.is_ascii_graphic() && !b"?#".contains(b))
    {
        return Err(Error::Invalid("path"));
    }
    if out.iter().any(|(n, _)| !token_ok(n)) {
        return Err(Error::Invalid("header name"));
    }
    if out.iter().any(|(_, v)| !value_ok(v)) {
        return Err(Error::Invalid("header value"));
    }
    Ok(Request {
        endpoint: index,
        sample: pick,
        method: endpoint.method.clone(),
        target,
        headers: out,
        body,
    })
}
