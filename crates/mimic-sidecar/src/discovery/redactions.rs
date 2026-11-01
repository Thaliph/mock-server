//! Counts what the redaction of an exchange replaces, by location and rule
//! (`mimic_discovery_redactions_total`, spec §9). The report itself is redacted by
//! `mimic-core`; this walks the same inputs with the same detectors, on the recorder thread.

use std::collections::BTreeMap;

use mimic_core::discovery::redact::{Redactor, value_rule};
use percent_encoding::percent_decode_str;
use serde_json::Value;

/// Deepest JSON nesting walked.
const MAX_DEPTH: usize = 64;

pub(super) type Counts = BTreeMap<(&'static str, &'static str), u64>;

fn add(counts: &mut Counts, location: &'static str, rule: &'static str) {
    *counts.entry((location, rule)).or_default() += 1;
}

pub(super) fn path(counts: &mut Counts, path: &str) {
    for segment in path.split('/') {
        if let Some(rule) = value_rule(segment) {
            add(counts, "path", rule.as_str());
        }
    }
}

fn decode(s: &str) -> String {
    percent_decode_str(&s.replace('+', " "))
        .decode_utf8_lossy()
        .into_owned()
}

/// Query string or form body.
pub(super) fn pairs(counts: &mut Counts, redactor: &Redactor, location: &'static str, s: &str) {
    for pair in s.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        field(counts, redactor, location, &decode(key), &decode(value));
    }
}

pub(super) fn headers(counts: &mut Counts, redactor: &Redactor, headers: &[(String, String)]) {
    for (name, value) in headers {
        field(counts, redactor, "header", name, value);
    }
}

fn field(
    counts: &mut Counts,
    redactor: &Redactor,
    location: &'static str,
    name: &str,
    value: &str,
) {
    if redactor.name_matches(name) {
        add(counts, location, "name");
    } else if let Some(rule) = value_rule(value) {
        add(counts, location, rule.as_str());
    }
}

pub(super) fn json(counts: &mut Counts, redactor: &Redactor, body: &[u8]) {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        walk(counts, redactor, &value, 0);
    }
}

fn walk(counts: &mut Counts, redactor: &Redactor, value: &Value, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if redactor.name_matches(key) {
                    add(counts, "body", "name");
                } else {
                    walk(counts, redactor, value, depth + 1);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(counts, redactor, item, depth + 1);
            }
        }
        Value::String(s) => {
            if let Some(rule) = value_rule(s) {
                add(counts, "body", rule.as_str());
            }
        }
        Value::Number(n) => {
            if let Some(rule) = value_rule(&n.to_string()) {
                add(counts, "body", rule.as_str());
            }
        }
        Value::Bool(_) | Value::Null => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2ln";

    fn header(name: &str, value: &str) -> (String, String) {
        (name.to_string(), value.to_string())
    }

    #[test]
    fn dsc_18_url_redactions_in_header_values_are_counted() {
        let mut counts = Counts::new();
        let location = format!("https://h/reset/{JWT}?token=T&page=2");
        let headers = [header("Location", &location), header("Link", "<cb?code=1>")];
        super::headers(&mut counts, &Redactor::default(), &headers);
        assert_eq!(counts.get(&("header", "jwt")), Some(&1), "{counts:?}");
        assert_eq!(counts.get(&("header", "name")), Some(&1), "{counts:?}");
    }

    #[test]
    fn dsc_18_only_the_64_kept_headers_are_counted() {
        let mut headers: Vec<_> = (0..100)
            .map(|i| header(&format!("x-api-key-{i:03}"), "k"))
            .collect();
        headers.push(header("Connection", "close"));
        let mut counts = Counts::new();
        super::headers(&mut counts, &Redactor::default(), &headers);
        assert_eq!(counts.get(&("header", "name")), Some(&64), "{counts:?}");
    }

    #[test]
    fn dsc_18_percent_encoded_path_segments_are_counted() {
        let mut counts = Counts::new();
        path(&mut counts, &format!("/reset/%65{}", &JWT[1..]));
        assert_eq!(counts.get(&("path", "jwt")), Some(&1), "{counts:?}");
    }
}
