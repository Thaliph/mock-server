//! Outils communs aux cibles de fuzzing (voir `README.md`).

use mimic_core::model::{Body, Direction, Exchange, Header};
use mimic_core::redact::{RedactConfig, Redactor};

/// Secret injecté par les cibles de redaction : il ne doit jamais ressortir (RED-04).
pub const SECRET: &str = "fuzz-s3cr3t-9d1b7e2a";

/// Redacteur de la configuration par défaut, sel fixe.
pub fn redactor() -> Redactor {
    Redactor::new(RedactConfig::default(), b"fuzz-salt".to_vec())
}

/// Échange sortant minimal dont la requête porte `body` sous le type `content_type`.
pub fn exchange(content_type: &str, body: Vec<u8>) -> Exchange {
    Exchange {
        id: "01FUZZ".into(),
        direction: Direction::Outbound,
        started_at_ms: 0,
        scheme: "http".into(),
        authority: "api.example.com".into(),
        method: "POST".into(),
        path: "/v1/fuzz".into(),
        query: vec![],
        req_headers: vec![Header {
            name: "content-type".into(),
            value: content_type.into(),
        }],
        req_body: Body::Captured { data: body },
        status: 200,
        resp_headers: vec![],
        resp_body: Body::Empty,
        resp_trailers: vec![],
        ttfb_us: 0,
        total_us: 0,
        trace_id: None,
        redactions: vec![],
    }
}

/// Vérifie qu'une seconde redaction ne change rien, et rend l'échange redacté.
pub fn redact_idempotent(r: &Redactor, mut e: Exchange) -> Exchange {
    r.redact(&mut e);
    let mut again = e.clone();
    r.redact(&mut again);
    assert_eq!(again, e, "redaction non idempotente");
    e
}

/// Octets du body de requête gardés après redaction.
pub fn req_bytes(e: &Exchange) -> &[u8] {
    match &e.req_body {
        Body::Captured { data } => data,
        Body::Truncated { prefix, .. } => prefix,
        Body::Empty | Body::Skipped { .. } => &[],
    }
}

/// Vrai si `needle` apparaît dans `hay`.
pub fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}
