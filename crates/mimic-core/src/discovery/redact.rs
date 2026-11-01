//! Redaction (spec §6), by name and by value. The marker is a constant: no hash of a secret is
//! ever kept, so two secrets cannot be told apart.

use std::borrow::Cow;
use std::collections::BTreeSet;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use percent_encoding::percent_decode_str;
use serde_json::Value;

/// What a redacted value becomes.
pub const MARKER: &str = "<redacted>";

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
        }
    }
}

/// The value detector that catches `s`, if any.
pub fn value_rule(s: &str) -> Option<Rule> {
    if is_jwt(s) {
        Some(Rule::Jwt)
    } else if s.contains("-----BEGIN ") {
        Some(Rule::Pem)
    } else if is_credential(s) {
        Some(Rule::Credential)
    } else if is_card(s) {
        Some(Rule::Card)
    } else if is_aws(s) {
        Some(Rule::Aws)
    } else if TOKEN_PREFIXES
        .iter()
        .any(|p| s.strip_prefix(p).is_some_and(|rest| !rest.is_empty()))
    {
        Some(Rule::TokenPrefix)
    } else if has_userinfo_password(s) {
        Some(Rule::Userinfo)
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
    (scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("basic"))
        && !rest.trim().is_empty()
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
    if let Some(rule) = value_rule(s) {
        return Some(rule);
    }
    match percent_decode_str(s).decode_utf8_lossy() {
        Cow::Owned(decoded) => value_rule(&decoded),
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

fn path_with(path: &str, rules: &mut Vec<Rule>) -> String {
    path.split('/')
        .map(|seg| match segment_rule(seg) {
            Some(rule) => {
                rules.push(rule);
                MARKER
            }
            None => seg,
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

/// Redaction by name (built-in plus `MIMIC_DISCOVERY_REDACT_NAMES`) and by value.
#[derive(Debug, Clone)]
pub struct Redactor {
    /// Built-in and extra names, normalised.
    names: BTreeSet<String>,
    /// Longest of `names`, in bytes.
    max_len: usize,
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
        Self { names, max_len }
    }

    /// True when a header name, query key, form field or JSON key is redacted by name: a run
    /// of its tokens, joined, is a listed name, or a listed name followed by `s`.
    pub fn name_matches(&self, name: &str) -> bool {
        let tokens = tokens(name);
        // A built-in name glued into a token (`JSESSIONID`, `csrftoken`), except the short
        // ones, which would match words like `company` or `author`.
        let glued = tokens.iter().any(|t| {
            NAMES
                .iter()
                .filter(|n| !WHOLE_TOKEN.contains(n))
                .any(|n| t.contains(n))
        });
        glued
            || (0..tokens.len()).any(|start| {
                let mut joined = String::new();
                for token in &tokens[start..] {
                    joined.push_str(token);
                    if joined.len() > self.max_len + 1 {
                        return false;
                    }
                    if self.names.contains(&joined)
                        || joined
                            .strip_suffix('s')
                            .is_some_and(|plural| self.names.contains(plural))
                    {
                        return true;
                    }
                }
                false
            })
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
                    Some((scheme, _))
                        if !scheme.is_empty()
                            && scheme.bytes().all(|c| c.is_ascii_alphanumeric()) =>
                    {
                        format!("{scheme} {MARKER}")
                    }
                    _ => MARKER.to_string(),
                }
            }
            "cookie" => {
                rules.push(Rule::Name);
                value
                    .split(';')
                    .map(|pair| match pair.trim().split_once('=') {
                        Some((n, _)) => format!("{n}={MARKER}"),
                        None => MARKER.to_string(),
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
                    Some((n, _)) => format!("{n}={MARKER}"),
                    None => MARKER.to_string(),
                };
                match attrs {
                    Some(a) => format!("{first};{a}"),
                    None => first,
                }
            }
            _ => {
                if self.name_matches(name) {
                    rules.push(Rule::Name);
                    MARKER.to_string()
                } else if let Some(rule) = value_rule(value) {
                    rules.push(rule);
                    MARKER.to_string()
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
            let end = rest.find(DELIMS).unwrap_or(rest.len());
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
                .rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
                .map_or(0, |j| j + 1),
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
                let by_name = || self.name_matches(&decoded_key).then_some(Rule::Name);
                let rule = match value {
                    Some(v) => by_name().or_else(|| value_rule(&decode_query(v))),
                    None => value_rule(&decoded_key).or_else(by_name),
                };
                match (rule, value) {
                    (Some(rule), Some(_)) => {
                        rules.push(rule);
                        format!("{key}={MARKER}")
                    }
                    (Some(rule), None) => {
                        rules.push(rule);
                        MARKER.to_string()
                    }
                    (None, _) => pair.to_string(),
                }
            })
            .collect::<Vec<_>>()
            .join("&")
    }

    /// A query value or form field: the marker when the name or the value is sensitive.
    pub fn field(&self, name: &str, value: &str) -> String {
        if self.name_matches(name) || value_rule(value).is_some() {
            MARKER.to_string()
        } else {
            value.to_string()
        }
    }

    /// Redacts a JSON document in place: by key (the whole subtree) and by value.
    pub fn json(&self, value: &mut Value) {
        self.walk(value, 0);
    }

    fn walk(&self, value: &mut Value, depth: usize) {
        if depth > MAX_DEPTH {
            *value = Value::String(MARKER.to_string());
            return;
        }
        match value {
            Value::String(s) => {
                if value_rule(s).is_some() {
                    *s = MARKER.to_string();
                }
            }
            Value::Number(n) => {
                if is_card(&n.to_string()) {
                    *value = Value::String(MARKER.to_string());
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|v| self.walk(v, depth + 1)),
            Value::Object(map) => {
                for (k, v) in map.iter_mut() {
                    if self.name_matches(k) {
                        *v = Value::String(MARKER.to_string());
                    } else {
                        self.walk(v, depth + 1);
                    }
                }
            }
            Value::Bool(_) | Value::Null => {}
        }
    }
}
