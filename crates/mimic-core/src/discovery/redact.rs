//! Redaction (spec §6), by name and by value. The marker is a constant: no hash of a secret is
//! ever kept, so two secrets cannot be told apart.

use std::collections::BTreeSet;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

/// What a redacted value becomes.
pub const MARKER: &str = "<redacted>";

/// Deepest JSON nesting walked; anything below is redacted as a whole.
const MAX_DEPTH: usize = 128;

/// Names (normalised) that are redacted when they appear anywhere in a name.
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

/// The value detector that catches a path segment (stub).
pub fn segment_rule(s: &str) -> Option<Rule> {
    value_rule(s)
}

/// A path with the value detectors applied segment by segment; other segments are unchanged.
pub fn path(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            if value_rule(seg).is_some() {
                MARKER
            } else {
                seg
            }
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

/// Redaction by name (built-in plus `MIMIC_DISCOVERY_REDACT_NAMES`) and by value.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    extra: BTreeSet<String>,
}

impl Redactor {
    pub fn new<'a>(extra: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            // A name that normalises to nothing would match every name.
            extra: extra
                .into_iter()
                .map(normalise)
                .filter(|n| !n.is_empty())
                .collect(),
        }
    }

    /// Rules that redact parts of a header value (stub).
    pub fn header_rules(&self, _name: &str, _value: &str) -> Vec<Rule> {
        Vec::new()
    }

    /// True when a header name, query key, form field or JSON key is redacted by name.
    pub fn name_matches(&self, name: &str) -> bool {
        let n = normalise(name);
        NAMES.iter().any(|b| n.contains(b)) || self.extra.iter().any(|e| n.contains(e.as_str()))
    }

    /// A header value: `Authorization` keeps its scheme, `Cookie` and `Set-Cookie` keep the
    /// cookie names (and `Set-Cookie` its attributes).
    pub fn header(&self, name: &str, value: &str) -> String {
        match normalise(name).as_str() {
            "authorization" | "proxyauthorization" => match value.split_once(' ') {
                Some((scheme, _))
                    if !scheme.is_empty() && scheme.bytes().all(|c| c.is_ascii_alphanumeric()) =>
                {
                    format!("{scheme} {MARKER}")
                }
                _ => MARKER.to_string(),
            },
            "cookie" => value
                .split(';')
                .map(|pair| match pair.trim().split_once('=') {
                    Some((n, _)) => format!("{n}={MARKER}"),
                    None => MARKER.to_string(),
                })
                .collect::<Vec<_>>()
                .join("; "),
            "setcookie" => {
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
                let v = self.field(name, value);
                if v == MARKER { v } else { self.urls(&v) }
            }
        }
    }

    /// URLs inside a header value (`Location`, `Referer`, `Link: <u>; rel=next`, `Refresh`,
    /// ...): each one is redacted like a path plus query. Other text is unchanged.
    fn urls(&self, value: &str) -> String {
        const DELIMS: &[char] = &[' ', ';', ',', '<', '>', '"', '\''];
        let mut out = String::with_capacity(value.len());
        let mut rest = value;
        while !rest.is_empty() {
            let end = rest.find(DELIMS).unwrap_or(rest.len());
            let (token, tail) = rest.split_at(end);
            out.push_str(&self.url_token(token));
            let delim_end = tail.find(|c| !DELIMS.contains(&c)).unwrap_or(tail.len());
            out.push_str(&tail[..delim_end]);
            rest = &tail[delim_end..];
        }
        out
    }

    fn url_token(&self, token: &str) -> String {
        let start = match token.find("://") {
            Some(i) => token[..i]
                .rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
                .map_or(0, |j| j + 1),
            None if token.starts_with('/') => 0,
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
        let mut out = format!("{prefix}{origin}{}", path(p));
        if let Some(q) = query {
            out.push('?');
            out.push_str(&self.pairs(q));
        }
        if let Some(f) = frag {
            out.push('#');
            out.push_str(&self.pairs(f));
        }
        out
    }

    /// `k=v&k=v` with each pair through `field`; a bare item through the value detectors.
    fn pairs(&self, s: &str) -> String {
        s.split('&')
            .map(|pair| match pair.split_once('=') {
                Some((k, v)) => format!("{k}={}", self.field(k, v)),
                None if value_rule(pair).is_some() || self.name_matches(pair) => MARKER.to_string(),
                None => pair.to_string(),
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
