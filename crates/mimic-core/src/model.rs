//! Modèle de données d'un échange HTTP capturé (spec §5.2).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Sens du trafic vu depuis l'application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Trafic reçu par l'application : décrit l'API du service.
    Inbound,
    /// Trafic émis par l'application : décrit ses dépendances.
    Outbound,
}

/// Un header HTTP. Les noms sont stockés en minuscules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

impl Header {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into().to_ascii_lowercase(),
            value: value.into(),
        }
    }
}

/// Body d'une requête ou d'une réponse.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Body {
    /// Aucun body.
    #[default]
    Empty,
    /// Body capturé intégralement.
    Captured {
        #[serde(with = "bytes_b64")]
        data: Vec<u8>,
    },
    /// Body trop gros : seul le préfixe est conservé.
    Truncated {
        size: u64,
        #[serde(with = "bytes_b64")]
        prefix: Vec<u8>,
    },
    /// Body non capturé (échantillonnage ou `capture.bodies: false`).
    Skipped { size: u64 },
}

impl Body {
    pub fn captured(data: impl Into<Vec<u8>>) -> Self {
        Body::Captured { data: data.into() }
    }

    /// Octets disponibles pour l'inférence, si le body est complet.
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Body::Captured { data } => Some(data),
            _ => None,
        }
    }

    /// Vrai si le mock peut rejouer ce body à l'identique : capturé en entier, ou vide.
    pub fn is_replayable(&self) -> bool {
        matches!(self, Body::Empty | Body::Captured { .. })
    }

    /// Octets conservés en mémoire (body complet ou préfixe).
    pub fn kept_bytes(&self) -> usize {
        match self {
            Body::Captured { data } => data.len(),
            Body::Truncated { prefix, .. } => prefix.len(),
            Body::Empty | Body::Skipped { .. } => 0,
        }
    }
}

/// Marque laissée à chaque valeur remplacée par la redaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionMark {
    /// Emplacement, par exemple `request.header.authorization` ou `response.body$.user.email`.
    pub location: String,
}

/// Un échange requête/réponse capturé par le proxy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exchange {
    /// Identifiant unique (ULID en pratique ; libre ici).
    pub id: String,
    pub direction: Direction,
    /// Début de l'échange, en millisecondes Unix.
    pub started_at_ms: u64,
    pub scheme: String,
    /// `hôte[:port]` de la destination.
    pub authority: String,
    /// Méthode HTTP, en majuscules.
    pub method: String,
    /// Chemin sans la query string.
    pub path: String,
    pub query: Vec<(String, String)>,
    pub req_headers: Vec<Header>,
    pub req_body: Body,
    pub status: u16,
    pub resp_headers: Vec<Header>,
    pub resp_body: Body,
    /// Trailers de la réponse (HTTP/2 : `grpc-status`, `grpc-message`), rejoués par le mock ;
    /// absents de la sérialisation s'il n'y en a pas (identifiants de snapshot inchangés).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resp_trailers: Vec<Header>,
    /// Temps jusqu'au premier octet de réponse, en microsecondes.
    pub ttfb_us: u64,
    /// Durée totale de l'échange, en microsecondes.
    pub total_us: u64,
    pub trace_id: Option<String>,
    #[serde(default)]
    pub redactions: Vec<RedactionMark>,
}

impl Exchange {
    /// Premier header de requête portant ce nom (insensible à la casse).
    pub fn req_header(&self, name: &str) -> Option<&str> {
        find_header(&self.req_headers, name)
    }

    /// Premier header de réponse portant ce nom (insensible à la casse).
    pub fn resp_header(&self, name: &str) -> Option<&str> {
        find_header(&self.resp_headers, name)
    }

    /// Taille estimée en mémoire, en octets : la structure, chaque chaîne et chaque body,
    /// avec un surcoût forfaitaire par allocation. Sert au budget du store (spec §5.2).
    pub fn approx_bytes(&self) -> usize {
        // Surcoût moyen de l'allocateur pour un bloc sur le tas.
        const ALLOC: usize = 16;
        let s = |v: &str| if v.is_empty() { 0 } else { v.len() + ALLOC };
        let headers = |h: &[Header]| -> usize {
            h.iter()
                .map(|h| size_of::<Header>() + s(&h.name) + s(&h.value))
                .sum::<usize>()
                + if h.is_empty() { 0 } else { ALLOC }
        };
        let body = |b: &Body| match b.kept_bytes() {
            0 => 0,
            n => n + ALLOC,
        };
        size_of::<Exchange>()
            + s(&self.id)
            + s(&self.scheme)
            + s(&self.authority)
            + s(&self.method)
            + s(&self.path)
            + self
                .query
                .iter()
                .map(|(k, v)| size_of::<(String, String)>() + s(k) + s(v))
                .sum::<usize>()
            + headers(&self.req_headers)
            + headers(&self.resp_headers)
            + body(&self.req_body)
            + body(&self.resp_body)
            + self.trace_id.as_deref().map_or(0, s)
            + self
                .redactions
                .iter()
                .map(|r| size_of::<RedactionMark>() + s(&r.location))
                .sum::<usize>()
    }
}

fn find_header<'a>(headers: &'a [Header], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

/// Type MIME sans paramètres (`application/json; charset=utf-8` → `application/json`).
pub fn media_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Vrai pour `application/json` et les variantes `+json` (`application/problem+json`).
pub fn is_json(media_type: &str) -> bool {
    media_type == "application/json" || media_type.ends_with("+json")
}

mod bytes_b64 {
    use super::*;

    pub fn serialize<S: Serializer>(data: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(data))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}
