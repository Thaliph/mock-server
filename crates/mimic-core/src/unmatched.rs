//! Requêtes sans réponse du mock (spec §5.3) : forme affichée du chemin, sans valeur, et
//! relevé borné, agrégé par clé (authority, méthode, forme du chemin, raison).
//!
//! Fonctions pures : l'horodatage est fourni par l'appelant, les collections sont ordonnées
//! (même relevé, même sortie).

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::infer::path::{SegmentKind, is_identifier};
use crate::redact::{is_marker, is_sensitive_value};

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
/// Au-delà, un segment littéral est masqué (`{opaque}`).
const LONG_SEGMENT: usize = 32;
/// À partir de cette longueur, un segment qui mêle majuscules et minuscules est un jeton.
const TOKEN_SEGMENT: usize = 16;

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
/// L'ordre des champs est celui de la clé du relevé.
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
        Self {
            authority: truncate(authority, MAX_AUTHORITY),
            method: truncate(method, MAX_METHOD),
            path: path_template(path),
            reason,
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
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

impl UnmatchedEntry {
    fn key(&self) -> Unmatched {
        Unmatched {
            authority: self.authority.clone(),
            method: self.method.clone(),
            path: self.path.clone(),
            reason: self.reason,
        }
    }

    fn seen(&self) -> Seen {
        Seen {
            count: self.count,
            first_ms: self.first_ms,
            last_ms: self.last_ms,
        }
    }
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
        let mut total = 0u64;
        let mut merged: BTreeMap<Unmatched, Seen> = BTreeMap::new();
        for r in reports {
            total = total.saturating_add(r.total);
            for e in &r.entries {
                merged
                    .entry(e.key())
                    .and_modify(|s| s.absorb(e.seen()))
                    .or_insert_with(|| e.seen());
            }
        }
        UnmatchedReport {
            total,
            entries: sorted(&merged, limit),
        }
    }

    /// Les `limit` premières clés seulement : un relevé reçu reste borné.
    pub fn truncated(mut self, limit: usize) -> Self {
        self.entries.truncate(limit);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seen {
    count: u64,
    first_ms: u64,
    last_ms: u64,
}

impl Seen {
    fn absorb(&mut self, other: Seen) {
        self.count = self.count.saturating_add(other.count);
        self.first_ms = self.first_ms.min(other.first_ms);
        self.last_ms = self.last_ms.max(other.last_ms);
    }
}

/// Clés de la plus récemment vue à la plus ancienne, puis dans l'ordre de la clé.
fn sorted(entries: &BTreeMap<Unmatched, Seen>, limit: usize) -> Vec<UnmatchedEntry> {
    let mut list: Vec<(&Unmatched, &Seen)> = entries.iter().collect();
    // Tri stable sur un itérateur déjà ordonné par clé : à égalité, l'ordre de la clé.
    list.sort_by_key(|(_, s)| Reverse(s.last_ms));
    list.into_iter()
        .take(limit)
        .map(|(k, s)| UnmatchedEntry {
            authority: k.authority.clone(),
            method: k.method.clone(),
            path: k.path.clone(),
            reason: k.reason,
            count: s.count,
            first_ms: s.first_ms,
            last_ms: s.last_ms,
        })
        .collect()
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

    /// Compte une occurrence à `now_ms`. Une clé nouvelle, relevé plein, évince la clé vue
    /// le moins récemment (à égalité, la plus petite).
    pub fn record(&mut self, u: &Unmatched, now_ms: u64) {
        self.total = self.total.saturating_add(1);
        let seen = Seen {
            count: 1,
            first_ms: now_ms,
            last_ms: now_ms,
        };
        if let Some(s) = self.entries.get_mut(u) {
            s.absorb(seen);
            return;
        }
        if self.capacity == 0 {
            return;
        }
        if self.entries.len() >= self.capacity
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, s)| s.last_ms)
                .map(|(k, _)| k.clone())
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(u.clone(), seen);
    }

    /// Remise à zéro (nouveau tir).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.total = 0;
    }

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
        UnmatchedReport {
            total: self.total,
            entries: sorted(&self.entries, limit),
        }
    }
}

/// Forme affichée d'un chemin, sans query ni valeur (spec §5.3) : identifiants remplacés par
/// leur nature, valeurs sensibles et jetons masqués, longueur bornée.
pub fn path_template(path: &str) -> String {
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let mut out = String::with_capacity(path.len().min(MAX_PATH));
    for s in path.split('/').filter(|s| !s.is_empty()) {
        out.push('/');
        out.push_str(&segment(s));
        if out.chars().count() > MAX_PATH {
            break;
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    if out.chars().count() > MAX_PATH {
        // Coupe au dernier segment entier qui laisse la place de `/…`.
        let room = out
            .char_indices()
            .nth(MAX_PATH - 2)
            .map_or(out.len(), |(i, _)| i);
        let cut = out[..room].rfind('/').unwrap_or(0);
        out.truncate(cut);
        out.push_str("/…");
    }
    out
}

fn segment(s: &str) -> Cow<'static, str> {
    if is_identifier(s) {
        return Cow::Borrowed(match SegmentKind::of(s) {
            SegmentKind::Integer => "{int}",
            SegmentKind::Uuid => "{uuid}",
            _ => "{id}",
        });
    }
    let decoded = percent_decode(s);
    if is_marker(s) || is_marker(&decoded) || is_sensitive_value(&decoded) {
        return Cow::Borrowed("{redacted}");
    }
    let len = decoded.chars().count();
    let mixed_case = decoded.chars().any(|c| c.is_ascii_uppercase())
        && decoded.chars().any(|c| c.is_ascii_lowercase());
    if len > LONG_SEGMENT || (len >= TOKEN_SEGMENT && mixed_case) {
        return Cow::Borrowed("{opaque}");
    }
    Cow::Owned(s.to_string())
}

/// Décodage des `%XX` d'un segment, pour reconnaître une valeur encodée ; une séquence
/// invalide est gardée telle quelle.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// Date UTC lisible d'un horodatage en ms Unix : `2026-10-22T14:03:07Z`.
pub fn format_utc(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let (y, m, d) = crate::compact::civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        t / 3600,
        t % 3600 / 60,
        t % 60
    )
}
