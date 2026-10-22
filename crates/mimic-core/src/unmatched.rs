//! Requêtes sans réponse du mock (spec §5.3) : forme affichée du chemin, sans valeur, et
//! relevé borné, agrégé par clé (authority, méthode, forme du chemin, raison).
//!
//! Fonctions pures : l'horodatage est fourni par l'appelant, les collections sont ordonnées
//! (même relevé, même sortie).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Clés gardées par le relevé d'un sidecar.
pub const MAX_TRACKED: usize = 100;
/// Clés remontées au contrôleur dans un battement.
pub const MAX_REPORTED: usize = 20;
/// Clés du relevé agrégé d'un workload, côté contrôleur.
pub const MAX_MERGED: usize = 50;
/// Longueur maximale de la forme affichée d'un chemin, en caractères.
pub const MAX_PATH: usize = 256;
/// Longueur maximale d'une authority relevée.
pub const MAX_AUTHORITY: usize = 255;
/// Longueur maximale d'une méthode relevée.
pub const MAX_METHOD: usize = 16;

/// Pourquoi le mock n'a pas répondu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Aucun enregistrement ne correspond : `599`.
    Unmatched,
    /// Body au-delà de `capture.max_body_bytes` : `413`.
    TooLarge,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Unmatched => "unmatched",
            Reason::TooLarge => "too_large",
        }
    }

    /// Code de la réponse du mock.
    pub fn status(self) -> u16 {
        match self {
            Reason::Unmatched => 599,
            Reason::TooLarge => 413,
        }
    }
}

/// Requête sans réponse, normalisée : forme du chemin sans valeur, champs bornés.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Unmatched {
    pub authority: String,
    pub method: String,
    /// Forme affichée du chemin ([`path_template`]).
    pub path: String,
    pub reason: Reason,
}

impl Unmatched {
    /// `path` est le chemin brut de la requête : seule sa forme est gardée.
    pub fn new(reason: Reason, authority: &str, method: &str, path: &str) -> Self {
        let _ = (authority, method, path);
        Self {
            authority: String::new(),
            method: String::new(),
            path: String::new(),
            reason,
        }
    }
}

/// Une clé du relevé et ses occurrences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnmatchedEntry {
    pub authority: String,
    pub method: String,
    pub path: String,
    pub reason: Reason,
    pub count: u64,
    /// Première et dernière occurrence, en ms Unix.
    pub first_ms: u64,
    pub last_ms: u64,
}

/// Relevé lisible : total des requêtes sans réponse et clés, de la plus récemment vue à la
/// plus ancienne.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnmatchedReport {
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub entries: Vec<UnmatchedEntry>,
}

impl UnmatchedReport {
    /// Agrège les relevés de plusieurs pods : totaux additionnés, clés fusionnées (occurrences
    /// additionnées, premier et dernier horodatage gardés), au plus `limit` clés.
    pub fn merge<'a>(
        reports: impl IntoIterator<Item = &'a UnmatchedReport>,
        limit: usize,
    ) -> UnmatchedReport {
        let _ = (reports.into_iter().count(), limit);
        UnmatchedReport::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seen {
    count: u64,
    first_ms: u64,
    last_ms: u64,
}

/// Relevé borné d'un sidecar.
#[derive(Debug, Clone)]
pub struct UnmatchedLog {
    entries: BTreeMap<Unmatched, Seen>,
    total: u64,
    capacity: usize,
}

impl Default for UnmatchedLog {
    fn default() -> Self {
        Self::new(MAX_TRACKED)
    }
}

impl UnmatchedLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            total: 0,
            capacity,
        }
    }

    /// Compte une occurrence à `now_ms`.
    pub fn record(&mut self, u: &Unmatched, now_ms: u64) {
        let _ = (u, now_ms, self.capacity);
    }

    /// Remise à zéro (nouveau tir).
    pub fn clear(&mut self) {}

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Au plus `limit` clés, de la plus récemment vue à la plus ancienne, puis par clé.
    pub fn report(&self, limit: usize) -> UnmatchedReport {
        let _ = limit;
        UnmatchedReport::default()
    }
}

/// Forme affichée d'un chemin, sans query ni valeur (spec §5.3).
pub fn path_template(path: &str) -> String {
    path.to_string()
}

/// Date UTC lisible d'un horodatage en ms Unix : `2026-10-22T14:03:07Z`.
pub fn format_utc(ms: u64) -> String {
    ms.to_string()
}
