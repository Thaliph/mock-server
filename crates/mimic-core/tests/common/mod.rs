//! Outils partagés par les tests d'intégration de mimic-core.
#![allow(dead_code)]

use mimic_core::model::{Body, Direction, Exchange, Header};
use serde_json::Value;

/// Construit un échange entrant minimal : `GET /path?query` → 200 sans body.
pub fn exchange(method: &str, path_and_query: &str) -> Exchange {
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, q),
        None => (path_and_query, ""),
    };
    let query = query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (kv.to_string(), String::new()),
        })
        .collect();
    Exchange {
        id: String::new(),
        direction: Direction::Inbound,
        started_at_ms: 0,
        scheme: "http".into(),
        authority: "svc.local:8080".into(),
        method: method.into(),
        path: path.into(),
        query,
        req_headers: vec![],
        req_body: Body::Empty,
        status: 200,
        resp_headers: vec![],
        resp_body: Body::Empty,
        ttfb_us: 1_000,
        total_us: 1_000,
        trace_id: None,
        resp_trailers: vec![],
        redactions: vec![],
    }
}

pub trait ExchangeExt {
    fn status(self, status: u16) -> Self;
    fn req_json(self, body: Value) -> Self;
    fn resp_json(self, body: Value) -> Self;
    fn with_req_header(self, name: &str, value: &str) -> Self;
    fn with_resp_header(self, name: &str, value: &str) -> Self;
    fn at(self, started_at_ms: u64, id: &str) -> Self;
    fn latency_ms(self, ms: u64) -> Self;
}

impl ExchangeExt for Exchange {
    fn status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }
    fn req_json(mut self, body: Value) -> Self {
        self.req_headers
            .push(Header::new("content-type", "application/json"));
        self.req_body = Body::captured(serde_json::to_vec(&body).unwrap());
        self
    }
    fn resp_json(mut self, body: Value) -> Self {
        self.resp_headers
            .push(Header::new("content-type", "application/json"));
        self.resp_body = Body::captured(serde_json::to_vec(&body).unwrap());
        self
    }
    fn with_req_header(mut self, name: &str, value: &str) -> Self {
        self.req_headers.push(Header::new(name, value));
        self
    }
    fn with_resp_header(mut self, name: &str, value: &str) -> Self {
        self.resp_headers.push(Header::new(name, value));
        self
    }
    fn at(mut self, started_at_ms: u64, id: &str) -> Self {
        self.started_at_ms = started_at_ms;
        self.id = id.into();
        self
    }
    fn latency_ms(mut self, ms: u64) -> Self {
        self.ttfb_us = ms * 1_000;
        self.total_us = ms * 1_000;
        self
    }
}

/// Numérote les échanges pour leur donner un ordre stable.
pub fn sequence(exchanges: Vec<Exchange>) -> Vec<Exchange> {
    exchanges
        .into_iter()
        .enumerate()
        .map(|(i, e)| e.at(1_700_000_000_000 + i as u64 * 100, &format!("{i:06}")))
        .collect()
}

/// Vérifications structurelles d'un document OpenAPI 3.1 produit par mimic (INF-07).
/// Renvoie la liste des violations, vide si le document est valide.
pub fn openapi_violations(doc: &Value) -> Vec<String> {
    let mut errs = vec![];
    if doc["openapi"] != "3.1.0" {
        errs.push(format!("openapi = {}", doc["openapi"]));
    }
    if !doc["info"]["title"].is_string() || !doc["info"]["version"].is_string() {
        errs.push("info.title et info.version requis".into());
    }
    let Some(paths) = doc["paths"].as_object() else {
        errs.push("paths doit être un objet".into());
        return errs;
    };
    let methods = [
        "get", "put", "post", "delete", "options", "head", "patch", "trace",
    ];
    for (path, item) in paths {
        if !path.starts_with('/') {
            errs.push(format!("{path}: doit commencer par /"));
        }
        let templated: Vec<&str> = path
            .split('/')
            .filter_map(|s| s.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
            .collect();
        let mut seen = std::collections::BTreeSet::new();
        for name in &templated {
            if !seen.insert(*name) {
                errs.push(format!("{path}: paramètre {name} dupliqué"));
            }
        }
        let Some(item) = item.as_object() else {
            errs.push(format!("{path}: path item doit être un objet"));
            continue;
        };
        for (method, op) in item {
            if !methods.contains(&method.as_str()) {
                errs.push(format!("{path}: méthode inconnue {method}"));
                continue;
            }
            let params = op["parameters"].as_array().cloned().unwrap_or_default();
            for name in &templated {
                let declared = params
                    .iter()
                    .any(|p| p["in"] == "path" && p["name"] == *name && p["required"] == true);
                if !declared {
                    errs.push(format!(
                        "{method} {path}: paramètre de chemin {name} non déclaré"
                    ));
                }
            }
            for p in &params {
                let loc = p["in"].as_str().unwrap_or("");
                if !["path", "query", "header", "cookie"].contains(&loc) {
                    errs.push(format!("{method} {path}: in = {loc}"));
                }
                if !p["name"].is_string() || !p["schema"].is_object() {
                    errs.push(format!("{method} {path}: paramètre sans name ou schema"));
                }
            }
            let Some(responses) = op["responses"].as_object() else {
                errs.push(format!("{method} {path}: responses manquant"));
                continue;
            };
            if responses.is_empty() {
                errs.push(format!("{method} {path}: responses vide"));
            }
            for (code, resp) in responses {
                let ok = code.len() == 3 && code.chars().all(|c| c.is_ascii_digit());
                if !ok {
                    errs.push(format!("{method} {path}: code {code} invalide"));
                }
                if !resp["description"].is_string() {
                    errs.push(format!("{method} {path} {code}: description requise"));
                }
            }
        }
    }
    errs
}
