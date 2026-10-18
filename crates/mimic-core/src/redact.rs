//! Redaction des données sensibles avant toute écriture (spec §7).
//!
//! Chaque valeur sensible est remplacée par `<redacted:sha256:xxxxxxxx>`, où `xxxxxxxx`
//! sont les 8 premiers caractères hexadécimaux de `sha256(sel || valeur)`. Le sel est propre
//! à une installation : deux requêtes portant le même secret restent distinguables des autres
//! sans que la valeur soit retrouvable par force brute sur un dictionnaire public.

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::model::{Body, Exchange, Header, RedactionMark, is_json, media_type};

const MARKER_PREFIX: &str = "<redacted:sha256:";

/// Configuration de la redaction.
#[derive(Debug, Clone)]
pub struct RedactConfig {
    /// Headers toujours redactés (noms en minuscules).
    pub headers: Vec<String>,
    /// Sous-chaînes qui rendent sensible un nom de header, de paramètre ou de clé JSON.
    pub name_patterns: Vec<String>,
    /// Chemins JSON supplémentaires, forme `$.a.b` (sans index de tableau).
    pub extra_json_paths: Vec<String>,
    /// Détection des valeurs sensibles (e-mail, carte, IBAN, JWT) quel que soit le nom du champ.
    pub detect_values: bool,
}

impl Default for RedactConfig {
    fn default() -> Self {
        Self {
            headers: [
                "authorization",
                "proxy-authorization",
                "cookie",
                "set-cookie",
            ]
            .map(String::from)
            .to_vec(),
            name_patterns: ["token", "secret", "key", "session", "password", "passwd"]
                .map(String::from)
                .to_vec(),
            extra_json_paths: vec![],
            detect_values: true,
        }
    }
}

/// Applique la redaction à un échange.
#[derive(Debug, Clone)]
pub struct Redactor {
    config: RedactConfig,
    salt: Vec<u8>,
    extra_paths: Vec<Vec<String>>,
}

impl Redactor {
    pub fn new(config: RedactConfig, salt: Vec<u8>) -> Self {
        let extra_paths = config
            .extra_json_paths
            .iter()
            .map(|p| {
                p.trim_start_matches('$')
                    .split('.')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .collect();
        Self {
            config,
            salt,
            extra_paths,
        }
    }

    /// Redacte l'échange en place et ajoute une [`RedactionMark`] par valeur remplacée.
    /// L'opération est idempotente.
    pub fn redact(&self, e: &mut Exchange) {
        let mut marks = vec![];
        self.redact_headers(&mut e.req_headers, "request", &mut marks);
        self.redact_headers(&mut e.resp_headers, "response", &mut marks);
        for (name, value) in &mut e.query {
            if self.is_sensitive_name(name) && !is_marker(value) {
                *value = self.marker(value.as_bytes());
                marks.push(mark(format!("request.query.{name}")));
            }
        }
        let req_ct = e.req_header("content-type").map(media_type);
        let resp_ct = e.resp_header("content-type").map(media_type);
        self.redact_body(&mut e.req_body, req_ct.as_deref(), "request", &mut marks);
        self.redact_body(&mut e.resp_body, resp_ct.as_deref(), "response", &mut marks);
        e.redactions.extend(marks);
    }

    fn redact_headers(&self, headers: &mut [Header], side: &str, marks: &mut Vec<RedactionMark>) {
        for h in headers {
            let sensitive =
                self.config.headers.iter().any(|n| n == &h.name) || self.is_sensitive_name(&h.name);
            if sensitive && !is_marker(&h.value) {
                h.value = self.marker(h.value.as_bytes());
                marks.push(mark(format!("{side}.header.{}", h.name)));
            }
        }
    }

    fn redact_body(
        &self,
        body: &mut Body,
        media: Option<&str>,
        side: &str,
        marks: &mut Vec<RedactionMark>,
    ) {
        let Some(media) = media else { return };
        let textual = is_json(media) || media == "application/x-www-form-urlencoded";
        if !textual {
            return;
        }
        match body {
            Body::Captured { data } => {
                let before = marks.len();
                let redacted = if is_json(media) {
                    self.redact_json_bytes(data, side, marks)
                } else {
                    Some(self.redact_form(data, side, marks))
                };
                match redacted {
                    // Rien de remplacé : octets d'origine gardés (ordre des clés, espaces,
                    // écriture des nombres), le mock les rejoue à l'identique (spec §7, O5).
                    Some(_) if marks.len() == before => {}
                    Some(bytes) => *data = bytes,
                    // Body illisible : impossible de garantir qu'il ne contient pas de secret.
                    None => {
                        *body = Body::Skipped {
                            size: data.len() as u64,
                        }
                    }
                }
            }
            // Un préfixe ne se parse pas : il pourrait contenir un secret, on ne garde que la taille.
            Body::Truncated { size, .. } => *body = Body::Skipped { size: *size },
            Body::Empty | Body::Skipped { .. } => {}
        }
    }

    fn redact_json_bytes(
        &self,
        data: &[u8],
        side: &str,
        marks: &mut Vec<RedactionMark>,
    ) -> Option<Vec<u8>> {
        if data.is_empty() {
            return Some(vec![]);
        }
        let mut value: Value = serde_json::from_slice(data).ok()?;
        let mut path = vec![];
        let before = marks.len();
        self.redact_json(&mut value, &mut path, &format!("{side}.body"), marks);
        if marks.len() == before {
            // Rien à réécrire : l'appelant garde les octets d'origine.
            return Some(vec![]);
        }
        serde_json::to_vec(&value).ok()
    }

    fn redact_json(
        &self,
        value: &mut Value,
        path: &mut Vec<String>,
        prefix: &str,
        marks: &mut Vec<RedactionMark>,
    ) {
        let location = |path: &[String]| format!("{prefix}$.{}", path.join("."));
        match value {
            Value::Object(map) => {
                for (k, v) in map.iter_mut() {
                    path.push(k.clone());
                    let by_name =
                        self.is_sensitive_name(k) || self.extra_paths.iter().any(|p| p == path);
                    let already = v.as_str().is_some_and(is_marker);
                    if by_name && !already {
                        let raw = serde_json::to_vec(v).unwrap_or_default();
                        *v = Value::String(self.marker(&raw));
                        marks.push(mark(location(path)));
                    } else {
                        self.redact_json(v, path, prefix, marks);
                    }
                    path.pop();
                }
            }
            Value::Array(items) => {
                for (i, v) in items.iter_mut().enumerate() {
                    path.push(i.to_string());
                    self.redact_json(v, path, prefix, marks);
                    path.pop();
                }
            }
            Value::String(s)
                if self.config.detect_values && !is_marker(s) && is_sensitive_value(s) =>
            {
                *s = self.marker(s.as_bytes());
                marks.push(mark(location(path)));
            }
            _ => {}
        }
    }

    fn redact_form(&self, data: &[u8], side: &str, marks: &mut Vec<RedactionMark>) -> Vec<u8> {
        let text = String::from_utf8_lossy(data);
        let pairs: Vec<String> = text
            .split('&')
            .map(|pair| {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                let name = form_decode(k);
                let value = form_decode(v);
                let sensitive = self.is_sensitive_name(&name)
                    || (self.config.detect_values && is_sensitive_value(&value));
                if sensitive && !is_marker(&value) {
                    marks.push(mark(format!("{side}.body.{name}")));
                    format!("{k}={}", form_encode(&self.marker(value.as_bytes())))
                } else {
                    pair.to_string()
                }
            })
            .collect();
        pairs.join("&").into_bytes()
    }

    fn is_sensitive_name(&self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        self.config
            .name_patterns
            .iter()
            .any(|p| name.contains(p.as_str()))
    }

    fn marker(&self, value: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(&self.salt);
        h.update(value);
        let digest = h.finalize();
        let hex: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
        format!("{MARKER_PREFIX}{hex}>")
    }
}

fn mark(location: String) -> RedactionMark {
    RedactionMark { location }
}

/// Vrai si la valeur est déjà un marqueur de redaction.
pub fn is_marker(v: &str) -> bool {
    v.len() == MARKER_PREFIX.len() + 9
        && v.starts_with(MARKER_PREFIX)
        && v.ends_with('>')
        && v[MARKER_PREFIX.len()..v.len() - 1]
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
}

/// Détection des valeurs sensibles indépendamment du nom du champ.
pub fn is_sensitive_value(s: &str) -> bool {
    let s = s.trim();
    is_email(s) || is_card_number(s) || is_jwt(s) || is_iban(s)
}

pub(crate) fn is_email(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !s.contains(char::is_whitespace)
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}

/// 13 à 19 chiffres (espaces et tirets tolérés) passant le contrôle de Luhn.
fn is_card_number(s: &str) -> bool {
    if !s
        .chars()
        .all(|c| c.is_ascii_digit() || c == ' ' || c == '-')
    {
        return false;
    }
    let digits: Vec<u32> = s.chars().filter_map(|c| c.to_digit(10)).collect();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| {
            if i % 2 == 1 {
                let d = d * 2;
                if d > 9 { d - 9 } else { d }
            } else {
                d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

fn is_jwt(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3
        && s.starts_with("eyJ")
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

/// Pays (2 lettres), clé (2 chiffres), BBAN alphanumérique ; contrôle modulo 97.
fn is_iban(s: &str) -> bool {
    let s: String = s.chars().filter(|c| *c != ' ').collect();
    let b = s.as_bytes();
    if !(15..=34).contains(&b.len())
        || !b[..2].iter().all(u8::is_ascii_uppercase)
        || !b[2..4].iter().all(u8::is_ascii_digit)
        || !b[4..]
            .iter()
            .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
    {
        return false;
    }
    let rearranged = b[4..].iter().chain(&b[..4]);
    let mut rem: u32 = 0;
    for &c in rearranged {
        let v = if c.is_ascii_digit() {
            (c - b'0') as u32
        } else {
            (c - b'A') as u32 + 10
        };
        rem = if v >= 10 {
            (rem * 100 + v) % 97
        } else {
            (rem * 10 + v) % 97
        };
    }
    rem == 1
}

fn form_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h << 4 | l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

fn form_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => {
                (b as char).to_string()
            }
            b' ' => "+".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luhn() {
        assert!(is_card_number("4111111111111111"));
        assert!(is_card_number("4242-4242-4242-4242"));
        assert!(!is_card_number("4111111111111112"));
        assert!(!is_card_number("1234"));
    }

    #[test]
    fn iban() {
        assert!(is_iban("FR7630006000011234567890189"));
        assert!(is_iban("DE89 3704 0044 0532 0130 00"));
        assert!(!is_iban("FR7630006000011234567890188"));
    }

    #[test]
    fn form_roundtrip() {
        assert_eq!(form_decode("card%5Bnumber%5D"), "card[number]");
        assert_eq!(form_decode("a+b%2"), "a b%2");
        assert_eq!(form_decode(&form_encode("<redacted:x>")), "<redacted:x>");
    }

    #[test]
    fn marker_shape() {
        let r = Redactor::new(RedactConfig::default(), vec![]);
        assert!(is_marker(&r.marker(b"x")));
        assert!(!is_marker("<redacted:sha256:zzzzzzzz>"));
    }
}
