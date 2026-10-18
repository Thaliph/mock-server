//! Exemples par endpoint (spec §5.5.3) : pour chaque endpoint d'un workload, le contrôleur
//! garde au moins un échange rejouable, hors de l'historique (rétention, compaction, plafonds).
//!
//! - L'endpoint est le couple (opération, code), l'opération étant (sens, authority, méthode,
//!   forme du chemin), comme pour le plafond de bodies (§5.2).
//! - L'exemple est le plus récent des échanges rejouables reçus (date, puis identifiant) ; il
//!   n'est remplacé que par un échange rejouable plus récent du même endpoint. Sous la borne,
//!   le résultat ne dépend donc pas de l'ordre d'arrivée.
//! - Les échanges proposés sont déjà redactés (segments publiés par les sidecars) : rien n'est
//!   ajouté, un exemple est une copie conforme.
//! - Borne en octets : la taille sérialisée des exemples reste sous `max_bytes`, et sous la
//!   place que laisse la borne globale (`room`). Un nouvel endpoint qui ne tient pas garde un
//!   exemple sans body (toujours gardé, son poids est celui des métadonnées) ; un remplacement
//!   qui ne tient pas n'a pas lieu.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;

use crate::compact::{body_key, strip_bodies};
use crate::model::{Direction, Exchange};

/// Borne par défaut du nombre d'endpoints gardés avec un exemple, par workload.
pub const DEFAULT_MAX_ENDPOINTS: usize = 10_000;

/// Sort d'un échange proposé aux exemples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExemplarOffer {
    /// Premier exemple de son endpoint.
    Added,
    /// Remplace l'exemple, plus ancien, de son endpoint.
    Replaced,
    /// Pas rejouable, ou pas plus récent que l'exemple en place.
    Ignored,
    /// Nouvel endpoint au-delà de la borne en nombre : pas d'exemple.
    Refused,
    /// Nouvel endpoint au-delà d'une borne en octets : exemple gardé sans body, visible mais
    /// pas rejouable.
    AddedWithoutBody,
    /// Plus récent que l'exemple en place, mais au-delà d'une borne en octets : l'ancien reste.
    KeptOlder,
}

/// Taille sérialisée d'un échange : sa ligne dans `exemplars.jsonl`, saut de ligne compris.
pub fn serialized_len(e: &Exchange) -> u64 {
    /// Compte les octets écrits sans les garder.
    struct Counter(u64);
    impl Write for Counter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len() as u64;
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut c = Counter(0);
    match serde_json::to_writer(&mut c, e) {
        Ok(()) => c.0 + 1,
        Err(_) => 0,
    }
}

/// Endpoint d'un échange : (sens, authority, méthode, forme du chemin, code).
pub fn endpoint_key(e: &Exchange) -> String {
    body_key(e)
}

/// Vrai si l'échange peut servir d'exemple : terminé, hors `1xx` (poignée de main WebSocket,
/// jamais rejouée), et rejouable dans son sens : la réponse en sortie (mock), la requête en
/// entrée (rejeu, §5.7).
pub fn is_exemplar(e: &Exchange) -> bool {
    if e.status < 200 {
        return false;
    }
    match e.direction {
        Direction::Outbound => e.resp_body.is_replayable(),
        Direction::Inbound => e.req_body.is_replayable(),
    }
}

/// Ordre de récence : date de début, identifiant, puis contenu pour départager deux versions
/// d'un même identifiant (cas rare, sérialisation seulement alors).
fn recency(a: &Exchange, b: &Exchange) -> Ordering {
    (a.started_at_ms, &a.id)
        .cmp(&(b.started_at_ms, &b.id))
        .then_with(|| {
            let line = |e: &Exchange| serde_json::to_vec(e).unwrap_or_default();
            line(a).cmp(&line(b))
        })
}

/// Exemple gardé, avec sa taille sérialisée.
#[derive(Debug, Clone)]
struct Kept {
    e: Arc<Exchange>,
    size: u64,
    /// Faux pour un exemple gardé sans body (borne en octets).
    full: bool,
}

/// Exemples d'un workload, un par endpoint, dans l'ordre des endpoints.
#[derive(Debug, Clone)]
pub struct Exemplars {
    max: usize,
    max_bytes: u64,
    by_key: BTreeMap<String, Kept>,
    /// Taille sérialisée des exemples gardés, ceux sans body compris.
    bytes: u64,
    /// Échanges refusés faute de place (borne `max`).
    refused: u64,
    /// Exemples gardés sans body et remplacements refusés (bornes en octets).
    refused_bytes: u64,
    /// Change à chaque ajout ou remplacement.
    version: u64,
}

impl Default for Exemplars {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_ENDPOINTS)
    }
}

impl Exemplars {
    /// Exemples d'au plus `max_endpoints` endpoints, sans borne en octets.
    pub fn new(max_endpoints: usize) -> Self {
        Self::with_limits(max_endpoints, u64::MAX)
    }

    /// Exemples d'au plus `max_endpoints` endpoints et `max_bytes` octets sérialisés.
    pub fn with_limits(max_endpoints: usize, max_bytes: u64) -> Self {
        Self {
            max: max_endpoints,
            max_bytes,
            by_key: BTreeMap::new(),
            bytes: 0,
            refused: 0,
            refused_bytes: 0,
            version: 0,
        }
    }

    /// Propose un échange ; il n'est copié que s'il devient l'exemple de son endpoint.
    pub fn offer(&mut self, e: &Exchange) -> ExemplarOffer {
        self.offer_within(e, u64::MAX)
    }

    /// Comme `offer`, avec au plus `room` octets de plus (place laissée par la borne globale).
    pub fn offer_within(&mut self, e: &Exchange, room: u64) -> ExemplarOffer {
        self.offer_with(e, room, false, || Arc::new(e.clone()))
    }

    /// Comme `offer`, sans copie : l'échange partagé devient l'exemple tel quel.
    pub fn offer_shared(&mut self, e: Arc<Exchange>) -> ExemplarOffer {
        self.offer_shared_within(e, u64::MAX)
    }

    /// Comme `offer_within`, sans copie.
    pub fn offer_shared_within(&mut self, e: Arc<Exchange>, room: u64) -> ExemplarOffer {
        let shared = e.clone();
        self.offer_with(&e, room, false, move || shared)
    }

    /// Reprend un exemple relu de `exemplars.jsonl` : comme `offer_shared_within`, mais un
    /// exemple déjà gardé sans body est repris tel quel, sans être compté une seconde fois.
    pub fn restore_within(&mut self, e: Arc<Exchange>, room: u64) -> ExemplarOffer {
        let shared = e.clone();
        self.offer_with(&e, room, true, move || shared)
    }

    fn offer_with(
        &mut self,
        e: &Exchange,
        room: u64,
        restore: bool,
        keep: impl FnOnce() -> Arc<Exchange>,
    ) -> ExemplarOffer {
        let full = is_exemplar(e);
        // Un exemple sans body n'est repris que du support, où il a déjà été compté.
        if !full && !(restore && e.status >= 200) {
            return ExemplarOffer::Ignored;
        }
        let key = endpoint_key(e);
        let room = room.min(self.max_bytes.saturating_sub(self.bytes));
        if let Some(current) = self.by_key.get(&key) {
            // Le même échange, avec son body, peut remplacer sa version sans body.
            let same = (e.started_at_ms, &e.id) == (current.e.started_at_ms, &current.e.id);
            let upgrade = same && full && !current.full;
            if !upgrade && recency(e, &current.e) != Ordering::Greater {
                return ExemplarOffer::Ignored;
            }
            let size = serialized_len(e);
            if full && size.saturating_sub(current.size) > room {
                if upgrade {
                    return ExemplarOffer::Ignored;
                }
                self.refused_bytes += 1;
                return ExemplarOffer::KeptOlder;
            }
            self.bytes = self.bytes - current.size + size;
            self.by_key.insert(
                key,
                Kept {
                    e: keep(),
                    size,
                    full,
                },
            );
            self.version += 1;
            return ExemplarOffer::Replaced;
        }
        if self.by_key.len() >= self.max {
            self.refused += 1;
            return ExemplarOffer::Refused;
        }
        let size = serialized_len(e);
        let kept = if !full || size <= room {
            Kept {
                e: keep(),
                size,
                full,
            }
        } else {
            // Ne tient pas : gardé sans body, pour la découverte et la latence.
            let mut bare = e.clone();
            strip_bodies(&mut bare);
            self.refused_bytes += 1;
            Kept {
                size: serialized_len(&bare),
                full: false,
                e: Arc::new(bare),
            }
        };
        let offer = if kept.full {
            ExemplarOffer::Added
        } else {
            ExemplarOffer::AddedWithoutBody
        };
        self.bytes += kept.size;
        self.by_key.insert(key, kept);
        self.version += 1;
        offer
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    /// Échanges refusés faute de place depuis la création.
    pub fn refused(&self) -> u64 {
        self.refused
    }

    /// Taille sérialisée des exemples gardés, ceux sans body compris.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Exemples gardés sans body et remplacements refusés (bornes en octets) depuis la création.
    pub fn refused_bytes(&self) -> u64 {
        self.refused_bytes
    }

    /// Change à chaque ajout ou remplacement d'un exemple : sert à n'écrire que ce qui a changé.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Exemples, dans l'ordre des endpoints.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<Exchange>> {
        self.by_key.values().map(|k| &k.e)
    }

    /// Forme persistée : un exemple par ligne, dans l'ordre des endpoints.
    pub fn to_jsonl(&self) -> Vec<u8> {
        jsonl(self.iter())
    }

    /// Relit des lignes JSONL ; une ligne illisible est ignorée.
    pub fn parse(bytes: &[u8]) -> Vec<Exchange> {
        bytes
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .filter_map(|l| serde_json::from_slice(l).ok())
            .collect()
    }
}

/// Un échange par ligne.
pub fn jsonl<'a>(list: impl IntoIterator<Item = &'a Arc<Exchange>>) -> Vec<u8> {
    let mut out = Vec::new();
    for e in list {
        if let Ok(line) = serde_json::to_vec(e.as_ref()) {
            out.extend_from_slice(&line);
            out.push(b'\n');
        }
    }
    out
}

/// Meilleur candidat de chaque endpoint parmi `list`, sans copie : ce qu'un lot d'échanges
/// apporterait aux exemples. Sert à ne copier, hors de tout verrou, que les gagnants.
pub fn best_per_endpoint<'a>(
    list: impl IntoIterator<Item = &'a Exchange>,
) -> BTreeMap<String, &'a Exchange> {
    let mut best: BTreeMap<String, &Exchange> = BTreeMap::new();
    for e in list {
        if !is_exemplar(e) {
            continue;
        }
        let slot = best.entry(endpoint_key(e)).or_insert(e);
        if recency(e, slot) == Ordering::Greater {
            *slot = e;
        }
    }
    best
}
