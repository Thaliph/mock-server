//! Redaction (spec §6), by name and by value. What a redacted value becomes is a placeholder,
//! a constant of the type of the rule that fired: no byte, length or hash of a secret is ever
//! kept, so two secrets cannot be told apart.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use percent_encoding::percent_decode_str;
use serde_json::Value;

use super::placeholder::{PLACEHOLDERS, Placeholder, upgrade_text};

/// The legacy marker of up to F12 (spec §6): read and upgraded, and still the host of a redacted
/// outbound authority. A redacted value is a [`Placeholder`].
pub const MARKER: &str = "<redacted>";

/// HTTP authentication schemes kept by `Authorization` and `Proxy-Authorization`, in their
/// registered spelling: the IANA registry, plus `NTLM` and `AWS4-HMAC-SHA256`.
pub const AUTH_SCHEMES: &[&str] = &[
    "Basic",
    "Bearer",
    "Digest",
    "DPoP",
    "GNAP",
    "HOBA",
    "Mutual",
    "Negotiate",
    "OAuth",
    "PrivateToken",
    "SCRAM-SHA-1",
    "SCRAM-SHA-256",
    "vapid",
    "Concealed",
    "NTLM",
    "AWS4-HMAC-SHA256",
];

/// The registered spelling of `scheme` when it is in `AUTH_SCHEMES` (any case).
pub fn auth_scheme(scheme: &str) -> Option<&'static str> {
    AUTH_SCHEMES
        .iter()
        .copied()
        .find(|known| known.eq_ignore_ascii_case(scheme))
}

/// Request header values an outbound request keeps (spec §6.1): the negotiation and conditional
/// headers of RFC 9110, `user-agent` and the gRPC headers. Every other value is redacted.
const OUTBOUND_HEADERS: &[&str] = &[
    "accept",
    "accept-charset",
    "accept-encoding",
    "accept-language",
    "cache-control",
    "content-encoding",
    "content-language",
    "content-type",
    "expect",
    "if-match",
    "if-modified-since",
    "if-none-match",
    "if-range",
    "if-unmodified-since",
    "pragma",
    "range",
    "user-agent",
    "grpc-accept-encoding",
    "grpc-encoding",
    "grpc-timeout",
];

fn outbound_header_kept(name: &str) -> bool {
    OUTBOUND_HEADERS
        .iter()
        .any(|kept| kept.eq_ignore_ascii_case(name))
}

/// Deepest JSON nesting walked; anything below is redacted as a whole.
const MAX_DEPTH: usize = 128;

/// Built-in names that only match a whole token (a run of tokens, joined).
const WHOLE_TOKEN: &[&str] = &["auth", "pan", "iban", "ssn", "cvv", "cvc"];

/// Names (normalised) that are redacted when a run of tokens of a name spells one.
const NAMES: &[&str] = &[
    "authorization",
    "proxyauthorization",
    "cookie",
    "setcookie",
    "password",
    "passwd",
    "secret",
    "token",
    "apikey",
    "auth",
    "session",
    "signature",
    "credential",
    "privatekey",
    "cvv",
    "cvc",
    "cardnumber",
    "pan",
    "iban",
    "ssn",
];

const TOKEN_PREFIXES: &[&str] = &[
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxa-",
    "xoxb-",
    "xoxp-",
    "xoxr-",
    "xoxs-",
    "sk_live_",
    "rk_live_",
    "sk-",
];

/// Why a value was redacted (the `rule` label of `mimic_discovery_redactions_total`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    Name,
    Jwt,
    Pem,
    Credential,
    Card,
    Aws,
    TokenPrefix,
    Userinfo,
    /// An outbound request header value outside the allowlist (spec §6.1).
    OutboundHeader,
}

impl Rule {
    pub fn as_str(self) -> &'static str {
        match self {
            Rule::Name => "name",
            Rule::Jwt => "jwt",
            Rule::Pem => "pem",
            Rule::Credential => "credential",
            Rule::Card => "card",
            Rule::Aws => "aws",
            Rule::TokenPrefix => "token_prefix",
            Rule::Userinfo => "userinfo",
            Rule::OutboundHeader => "outbound_header",
        }
    }
}

/// The value detector that catches `s`, if any.
pub fn value_rule(s: &str) -> Option<Rule> {
    value_hit(s).map(|(rule, _)| rule)
}

/// The placeholder of the value detector that catches `s`, if any.
pub fn value_placeholder(s: &str) -> Option<Placeholder> {
    value_hit(s).map(|(_, placeholder)| placeholder)
}

/// The prefixes that give `mimic-api-key` rather than `mimic-token`.
const API_KEY_PREFIXES: &[&str] = &["sk_live_", "rk_live_", "sk-"];

/// The value detector that catches `s` and the placeholder it gives.
fn value_hit(s: &str) -> Option<(Rule, Placeholder)> {
    if is_jwt(s) {
        Some((Rule::Jwt, Placeholder::Jwt))
    } else if s.contains("-----BEGIN ") {
        Some((Rule::Pem, Placeholder::PrivateKey))
    } else if is_credential(s) {
        Some((Rule::Credential, Placeholder::Auth))
    } else if is_card(s) {
        Some((Rule::Card, Placeholder::Card))
    } else if is_aws(s) {
        Some((Rule::Aws, Placeholder::ApiKey))
    } else if is_google_key(s) {
        Some((Rule::TokenPrefix, Placeholder::ApiKey))
    } else if let Some(prefix) = TOKEN_PREFIXES
        .iter()
        .find(|p| s.strip_prefix(**p).is_some_and(|rest| !rest.is_empty()))
    {
        let placeholder = if API_KEY_PREFIXES.contains(prefix) {
            Placeholder::ApiKey
        } else {
            Placeholder::Token
        };
        Some((Rule::TokenPrefix, placeholder))
    } else if has_userinfo_password(s) {
        Some((Rule::Userinfo, Placeholder::Password))
    } else {
        None
    }
}

/// Three base64url parts whose first decodes to a JSON object with `alg`.
fn is_jwt(s: &str) -> bool {
    let mut parts = s.split('.');
    let (Some(head), Some(body), Some(sig), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let b64 = |p: &str| {
        !p.is_empty()
            && p.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    };
    if !(b64(head) && b64(body) && b64(sig)) {
        return false;
    }
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(head) else {
        return false;
    };
    matches!(serde_json::from_slice::<Value>(&bytes), Ok(Value::Object(o)) if o.contains_key("alg"))
}

/// `Bearer <x>` or `Basic <x>`, scheme in any case.
fn is_credential(s: &str) -> bool {
    let Some((scheme, rest)) = s.split_once(' ') else {
        return false;
    };
    // A placeholder after the scheme is what a redaction left: the value is redacted already.
    let rest = rest.trim();
    (scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("basic"))
        && !rest.is_empty()
        && rest != MARKER
        && !PLACEHOLDERS.contains(&rest)
}

/// 13 to 19 digits, single spaces or dashes allowed between them, passing Luhn.
fn is_card(s: &str) -> bool {
    let b = s.as_bytes();
    let mut digits = Vec::new();
    for (i, c) in b.iter().enumerate() {
        match c {
            b'0'..=b'9' => digits.push(u32::from(c - b'0')),
            b' ' | b'-' => {
                let between = i > 0
                    && b[i - 1].is_ascii_digit()
                    && b.get(i + 1).is_some_and(u8::is_ascii_digit);
                if !between {
                    return false;
                }
            }
            _ => return false,
        }
    }
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, d)| {
            if i % 2 == 1 {
                let x = d * 2;
                if x > 9 { x - 9 } else { x }
            } else {
                *d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// `AKIA` or `ASIA` followed by 16 upper-case letters or digits.
fn is_aws(s: &str) -> bool {
    s.len() == 20
        && (s.starts_with("AKIA") || s.starts_with("ASIA"))
        && s.bytes()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// `AIza` followed by 35 characters of `A-Za-z0-9_-`: a Google API key.
fn is_google_key(s: &str) -> bool {
    s.len() == 39
        && s.starts_with("AIza")
        && s.bytes()
            .skip(4)
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// A URL whose userinfo has a password: `scheme://user:password@host`.
fn has_userinfo_password(s: &str) -> bool {
    let Some((_, rest)) = s.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let Some((userinfo, _)) = authority.rsplit_once('@') else {
        return false;
    };
    matches!(userinfo.split_once(':'), Some((_, password)) if !password.is_empty())
}

/// The value detector that catches a path segment, which is percent-decoded first (the
/// segment itself is kept as received).
pub fn segment_rule(s: &str) -> Option<Rule> {
    segment_hit(s).map(|(rule, _)| rule)
}

fn segment_hit(s: &str) -> Option<(Rule, Placeholder)> {
    if let Some(hit) = value_hit(s) {
        return Some(hit);
    }
    match percent_decode_str(s).decode_utf8_lossy() {
        Cow::Owned(decoded) => value_hit(&decoded),
        Cow::Borrowed(_) => None,
    }
}

/// A query value as the detectors see it: `+` is a space, percent escapes are decoded.
fn decode_query(s: &str) -> String {
    percent_decode_str(&s.replace('+', " "))
        .decode_utf8_lossy()
        .into_owned()
}

/// A path with the value detectors applied segment by segment; other segments are unchanged.
pub fn path(path: &str) -> String {
    path_with(path, &mut Vec::new())
}

/// A path segment with the value detectors applied.
pub fn segment(seg: &str) -> String {
    match segment_hit(seg) {
        Some((_, placeholder)) => placeholder.to_string(),
        None => upgrade_text(seg),
    }
}

fn path_with(path: &str, rules: &mut Vec<Rule>) -> String {
    path.split('/')
        .map(|seg| match segment_hit(seg) {
            Some((rule, placeholder)) => {
                rules.push(rule);
                placeholder.to_string()
            }
            None => upgrade_text(seg),
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Lower case with `-` and `_` removed.
fn normalise(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '-' | '_'))
        .flat_map(char::to_lowercase)
        .collect()
}

/// The lower-case tokens of a name: it is cut at every character that is not a letter or a
/// digit, between a letter and a digit, at a lower-to-upper case change (`refreshToken`) and
/// before the last capital of a run followed by a lower-case letter (`APIKey`).
fn tokens(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut out = Vec::new();
    let mut current = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            flush(&mut out, &mut current);
            continue;
        }
        let boundary = i
            .checked_sub(1)
            .and_then(|j| chars.get(j))
            .is_some_and(|&p| {
                p.is_alphanumeric()
                    && (p.is_numeric() != c.is_numeric()
                        || (p.is_lowercase() && c.is_uppercase())
                        || (p.is_uppercase()
                            && c.is_uppercase()
                            && chars.get(i + 1).is_some_and(|n| n.is_lowercase())))
            });
        if boundary {
            flush(&mut out, &mut current);
        }
        current.extend(c.to_lowercase());
    }
    flush(&mut out, &mut current);
    out
}

fn flush(out: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        out.push(std::mem::take(current));
    }
}

/// The end of the token at the start of `s`: the first delimiter that is not part of a
/// marker, so that redacting a redacted URL again leaves it as it is.
fn token_end(s: &str, delims: &[char]) -> usize {
    let mut i = 0;
    while i < s.len() {
        if s[i..].starts_with(MARKER) {
            i += MARKER.len();
            continue;
        }
        let Some(c) = s[i..].chars().next() else {
            break;
        };
        if delims.contains(&c) {
            return i;
        }
        i += c.len_utf8();
    }
    s.len()
}

/// Redaction by name (built-in plus `MIMIC_DISCOVERY_REDACT_NAMES`) and by value.
#[derive(Debug, Clone)]
pub struct Redactor {
    /// Built-in and extra names, normalised.
    names: Arc<BTreeSet<String>>,
    /// Longest of `names`, in bytes.
    max_len: usize,
    /// The view of an outbound request (spec §6.1): header values outside an allowlist, and
    /// `key` and `sig`, are redacted.
    outbound_request: bool,
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new([])
    }
}

impl Redactor {
    pub fn new<'a>(extra: impl IntoIterator<Item = &'a str>) -> Self {
        // A name that normalises to nothing would match every name.
        let names: BTreeSet<String> = NAMES
            .iter()
            .map(|n| (*n).to_string())
            .chain(extra.into_iter().map(normalise).filter(|n| !n.is_empty()))
            .collect();
        let max_len = names.iter().map(String::len).max().unwrap_or(0);
        Self {
            names: Arc::new(names),
            max_len,
            outbound_request: false,
        }
    }

    /// The same redaction with the additional rules of an outbound request (spec §6.1).
    pub fn outbound_request(&self) -> Redactor {
        Redactor {
            outbound_request: true,
            ..self.clone()
        }
    }

    /// True when the value of header `name` is redacted whatever it holds: a sensitive name,
    /// or in the outbound request view a name outside the allowlist.
    pub fn header_name_matches(&self, name: &str) -> bool {
        self.name_matches(name) || (self.outbound_request && !outbound_header_kept(name))
    }

    /// True when a header name, query key, form field or JSON key is redacted by name.
    pub fn name_matches(&self, name: &str) -> bool {
        self.name_placeholder(name).is_some()
    }

    /// The placeholder of the name rule that fires on `name`, if any: a run of its tokens,
    /// joined, is a listed name, or a listed name followed by `s`, or a built-in name is glued
    /// into one token. When several match, the one that ends last in the name wins, then the
    /// longest (`session_token` is a token, `token_secret` a secret).
    pub fn name_placeholder(&self, name: &str) -> Option<Placeholder> {
        let tokens = tokens(name);
        // (end, length, placeholder) of the best match.
        let mut best: Option<(usize, usize, Placeholder)> = None;
        let mut consider = |end: usize, len: usize, placeholder: Placeholder| {
            if best.is_none_or(|(e, l, _)| (end, len) > (e, l)) {
                best = Some((end, len, placeholder));
            }
        };
        let mut starts = Vec::with_capacity(tokens.len());
        let mut offset = 0;
        for token in &tokens {
            starts.push(offset);
            let end = offset + token.len();
            if self.outbound_request && token == "key" {
                consider(end, 3, Placeholder::ApiKey);
            }
            if self.outbound_request && token == "sig" {
                consider(end, 3, Placeholder::Signature);
            }
            // A built-in name glued into a token (`JSESSIONID`, `csrftoken`), except the short
            // ones, which would match words like `company` or `author`.
            for n in NAMES.iter().filter(|n| !WHOLE_TOKEN.contains(n)) {
                if let Some(at) = token.rfind(n) {
                    consider(offset + at + n.len(), n.len(), self.type_of(n));
                }
            }
            offset = end;
        }
        for i in 0..tokens.len() {
            let mut joined = String::new();
            for (token, start) in tokens[i..].iter().zip(&starts[i..]) {
                joined.push_str(token);
                if joined.len() > self.max_len + 1 {
                    break;
                }
                let end = start + token.len();
                if self.names.contains(&joined) {
                    consider(end, joined.len(), self.type_of(&joined));
                } else if let Some(plural) = joined.strip_suffix('s')
                    && self.names.contains(plural)
                {
                    consider(end - 1, plural.len(), self.type_of(plural));
                }
            }
        }
        best.map(|(_, _, placeholder)| placeholder)
    }

    /// The placeholder of a listed name: the type of a built-in one, `mimic-secret` for the
    /// names of `MIMIC_DISCOVERY_REDACT_NAMES`.
    fn type_of(&self, name: &str) -> Placeholder {
        match name {
            "authorization" | "proxyauthorization" | "auth" | "credential" => Placeholder::Auth,
            "cookie" | "setcookie" => Placeholder::Cookie,
            "password" | "passwd" => Placeholder::Password,
            "token" => Placeholder::Token,
            "apikey" => Placeholder::ApiKey,
            "session" => Placeholder::Session,
            "signature" => Placeholder::Signature,
            "privatekey" => Placeholder::PrivateKey,
            "cvv" | "cvc" | "cardnumber" | "pan" => Placeholder::Card,
            "iban" => Placeholder::Iban,
            "ssn" => Placeholder::Ssn,
            _ => Placeholder::Secret,
        }
    }

    /// A header value: `Authorization` keeps its scheme, `Cookie` and `Set-Cookie` keep the
    /// cookie names (and `Set-Cookie` its attributes).
    pub fn header(&self, name: &str, value: &str) -> String {
        self.header_with(name, value, &mut Vec::new())
    }

    /// The rules that redact `value` of header `name`, in the order they apply: one `name`
    /// for a sensitive header, else the value detector, else one per redacted part of a URL
    /// in the value (path segments, then query, then fragment).
    pub fn header_rules(&self, name: &str, value: &str) -> Vec<Rule> {
        let mut rules = Vec::new();
        self.header_with(name, value, &mut rules);
        rules
    }

    fn header_with(&self, name: &str, value: &str, rules: &mut Vec<Rule>) -> String {
        match normalise(name).as_str() {
            "authorization" | "proxyauthorization" => {
                rules.push(Rule::Name);
                match value.split_once(' ') {
                    Some((scheme, _)) => match auth_scheme(scheme) {
                        Some("Bearer") => format!("Bearer {}", Placeholder::Token),
                        Some(known) => format!("{known} {}", Placeholder::Auth),
                        None => Placeholder::Auth.to_string(),
                    },
                    None => Placeholder::Auth.to_string(),
                }
            }
            "cookie" => {
                rules.push(Rule::Name);
                value
                    .split(';')
                    .map(|pair| match pair.trim().split_once('=') {
                        Some((n, _)) => format!("{n}={}", Placeholder::Cookie),
                        None => Placeholder::Cookie.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            }
            "setcookie" => {
                rules.push(Rule::Name);
                let (pair, attrs) = match value.split_once(';') {
                    Some((p, a)) => (p, Some(a)),
                    None => (value, None),
                };
                let first = match pair.split_once('=') {
                    Some((n, _)) => format!("{n}={}", Placeholder::Cookie),
                    None => Placeholder::Cookie.to_string(),
                };
                match attrs {
                    Some(a) => format!("{first};{a}"),
                    None => first,
                }
            }
            _ => {
                if self.outbound_request && !outbound_header_kept(name) {
                    rules.push(Rule::OutboundHeader);
                    Placeholder::Secret.to_string()
                } else if let Some(placeholder) = self.name_placeholder(name) {
                    rules.push(Rule::Name);
                    placeholder.to_string()
                } else if let Some((rule, placeholder)) = value_hit(value) {
                    rules.push(rule);
                    placeholder.to_string()
                } else if value == MARKER {
                    Placeholder::Secret.to_string()
                } else {
                    self.urls(value, rules)
                }
            }
        }
    }

    /// URLs inside a header value (`Location`, `Referer`, `Link: <u>; rel=next`, `Refresh`,
    /// ...): each one is redacted like a path plus query. Other text is unchanged.
    fn urls(&self, value: &str, rules: &mut Vec<Rule>) -> String {
        const DELIMS: &[char] = &[' ', ';', ',', '<', '>', '"', '\''];
        let mut out = String::with_capacity(value.len());
        let mut rest = value;
        while !rest.is_empty() {
            let end = token_end(rest, DELIMS);
            let (token, tail) = rest.split_at(end);
            out.push_str(&self.url_token(token, rules));
            let delim_end = tail.find(|c| !DELIMS.contains(&c)).unwrap_or(tail.len());
            out.push_str(&tail[..delim_end]);
            rest = &tail[delim_end..];
        }
        out
    }

    /// An absolute URL, or a relative reference (a token with a `/`, a `?` or a `#`).
    fn url_token(&self, token: &str, rules: &mut Vec<Rule>) -> String {
        let start = match token.find("://") {
            Some(i) => token[..i]
                .char_indices()
                .rfind(|&(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
                .map_or(0, |(j, c)| j + c.len_utf8()),
            None if token.contains(['/', '?', '#']) => 0,
            None => return token.to_string(),
        };
        let (prefix, url) = token.split_at(start);
        let (before_frag, frag) = match url.split_once('#') {
            Some((u, f)) => (u, Some(f)),
            None => (url, None),
        };
        let (base, query) = match before_frag.split_once('?') {
            Some((b, q)) => (b, Some(q)),
            None => (before_frag, None),
        };
        // Scheme and authority are kept; only what follows is a path.
        let (origin, p) = match base.split_once("://") {
            Some((scheme, rest)) => match rest.find('/') {
                Some(i) => (format!("{scheme}://{}", &rest[..i]), &rest[i..]),
                None => (base.to_string(), ""),
            },
            None => (String::new(), base),
        };
        let mut out = format!("{prefix}{origin}{}", path_with(p, rules));
        if let Some(q) = query {
            out.push('?');
            out.push_str(&self.pairs(q, rules));
        }
        if let Some(f) = frag {
            out.push('#');
            out.push_str(&self.pairs(f, rules));
        }
        out
    }

    /// `k=v&k=v` with each pair redacted by name or by (decoded) value; the pairs that are
    /// kept stay as received. A bare item goes through the value detectors, then the names.
    fn pairs(&self, s: &str, rules: &mut Vec<Rule>) -> String {
        s.split('&')
            .map(|pair| {
                let (key, value) = match pair.split_once('=') {
                    Some((k, v)) => (k, Some(v)),
                    None => (pair, None),
                };
                let decoded_key = decode_query(key);
                let by_name = || {
                    self.name_placeholder(&decoded_key)
                        .map(|placeholder| (Rule::Name, placeholder))
                };
                let hit = match value {
                    Some(v) => by_name().or_else(|| value_hit(&decode_query(v))),
                    None => value_hit(&decoded_key).or_else(by_name),
                };
                match (hit, value) {
                    (Some((rule, placeholder)), Some(_)) => {
                        rules.push(rule);
                        format!("{key}={placeholder}")
                    }
                    (Some((rule, placeholder)), None) => {
                        rules.push(rule);
                        placeholder.to_string()
                    }
                    (None, _) => upgrade_text(pair),
                }
            })
            .collect::<Vec<_>>()
            .join("&")
    }

    /// A query value or form field: the placeholder when the name or the value is sensitive.
    pub fn field(&self, name: &str, value: &str) -> String {
        if let Some(placeholder) = self.name_placeholder(name) {
            placeholder.to_string()
        } else if let Some((_, placeholder)) = value_hit(value) {
            placeholder.to_string()
        } else {
            upgrade_text(value)
        }
    }

    /// Redacts a JSON document in place: by key (the whole subtree) and by value.
    pub fn json(&self, value: &mut Value) {
        self.walk(value, 0);
    }

    fn walk(&self, value: &mut Value, depth: usize) {
        if depth > MAX_DEPTH {
            *value = Value::String(Placeholder::Secret.to_string());
            return;
        }
        match value {
            Value::String(s) => {
                if let Some((_, placeholder)) = value_hit(s) {
                    *s = placeholder.to_string();
                } else if s.contains(MARKER) {
                    *s = upgrade_text(s);
                }
            }
            Value::Number(n) => {
                if is_card(&n.to_string()) {
                    *value = Value::String(Placeholder::Card.to_string());
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|v| self.walk(v, depth + 1)),
            Value::Object(map) => {
                for (k, v) in map.iter_mut() {
                    if let Some(placeholder) = self.name_placeholder(k) {
                        *v = Value::String(placeholder.to_string());
                    } else {
                        self.walk(v, depth + 1);
                    }
                }
            }
            Value::Bool(_) | Value::Null => {}
        }
    }
}
