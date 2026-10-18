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

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::compact::body_key;
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
    /// Nouvel endpoint au-delà de la borne : pas d'exemple.
    Refused,
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

/// Exemples d'un workload, un par endpoint, dans l'ordre des endpoints.
#[derive(Debug, Clone, Default)]
pub struct Exemplars {
    max: usize,
    by_key: BTreeMap<String, Arc<Exchange>>,
    /// Échanges refusés faute de place (borne `max`).
    refused: u64,
    /// Change à chaque ajout ou remplacement.
    version: u64,
}

impl Exemplars {
    /// Exemples d'au plus `max_endpoints` endpoints.
    pub fn new(max_endpoints: usize) -> Self {
        Self {
            max: max_endpoints,
            ..Self::default()
        }
    }

    /// Propose un échange ; il n'est copié que s'il devient l'exemple de son endpoint.
    pub fn offer(&mut self, e: &Exchange) -> ExemplarOffer {
        self.offer_with(e, || Arc::new(e.clone()))
    }

    /// Comme `offer`, sans copie : l'échange partagé devient l'exemple tel quel.
    pub fn offer_shared(&mut self, e: Arc<Exchange>) -> ExemplarOffer {
        let shared = e.clone();
        self.offer_with(&e, move || shared)
    }

    fn offer_with(&mut self, e: &Exchange, keep: impl FnOnce() -> Arc<Exchange>) -> ExemplarOffer {
        if !is_exemplar(e) {
            return ExemplarOffer::Ignored;
        }
        let key = endpoint_key(e);
        let offer = match self.by_key.get(&key) {
            Some(current) if recency(e, current) != Ordering::Greater => {
                return ExemplarOffer::Ignored;
            }
            Some(_) => ExemplarOffer::Replaced,
            None if self.by_key.len() >= self.max => {
                self.refused += 1;
                return ExemplarOffer::Refused;
            }
            None => ExemplarOffer::Added,
        };
        self.by_key.insert(key, keep());
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

    /// Change à chaque ajout ou remplacement d'un exemple : sert à n'écrire que ce qui a changé.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Exemples, dans l'ordre des endpoints.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<Exchange>> {
        self.by_key.values()
    }

    /// Forme persistée : un exemple par ligne, dans l'ordre des endpoints.
    pub fn to_jsonl(&self) -> Vec<u8> {
        jsonl(self.by_key.values())
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
