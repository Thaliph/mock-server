//! Snapshots de mock (spec §5.6) : jeu figé d'échanges sortants, identique pour tous les pods
//! d'un workload et identifié par l'empreinte de son contenu.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use crate::model::{Direction, Exchange};

#[derive(Debug, Clone)]
pub struct SnapshotOptions {
    /// Exemples gardés par couple (authority, méthode, chemin, code), les plus récents.
    pub max_per_key: usize,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self { max_per_key: 50 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// 16 caractères hexadécimaux du sha256 du contenu JSONL.
    pub id: String,
    /// Échanges triés par date puis identifiant.
    pub exchanges: Vec<Exchange>,
}

/// Construit le snapshot : indépendant de l'ordre d'arrivée et des doublons.
pub fn build(exchanges: impl IntoIterator<Item = Exchange>, opts: &SnapshotOptions) -> Snapshot {
    // Tri total : date, identifiant, puis contenu sérialisé pour départager deux versions d'un même id.
    let mut keyed: Vec<(u64, String, String, Exchange)> = exchanges
        .into_iter()
        .filter(|e| e.direction == Direction::Outbound && e.status != 0)
        .map(|e| {
            let line = serde_json::to_string(&e).unwrap_or_default();
            (e.started_at_ms, e.id.clone(), line, e)
        })
        .collect();
    keyed.sort_by(|a, b| (a.0, &a.1, &a.2).cmp(&(b.0, &b.1, &b.2)));
    let mut seen = BTreeSet::new();
    let unique: Vec<Exchange> = keyed
        .into_iter()
        .filter(|(_, id, _, _)| seen.insert(id.clone()))
        .map(|(.., e)| e)
        .collect();

    // Plafond par clé, en partant des plus récents. Il est compté à part pour les réponses
    // rejouables : au-delà du plafond d'exemples du sidecar, les échanges récents d'un endpoint
    // très sollicité n'ont plus de body et ne doivent pas évincer ceux qui en ont un.
    let mut per_key: BTreeMap<(String, String, String, u16, bool), usize> = BTreeMap::new();
    let mut kept: Vec<Exchange> = unique
        .into_iter()
        .rev()
        .filter(|e| {
            let n = per_key
                .entry((
                    e.authority.clone(),
                    e.method.to_ascii_uppercase(),
                    e.path.clone(),
                    e.status,
                    e.resp_body.is_replayable(),
                ))
                .or_default();
            *n += 1;
            *n <= opts.max_per_key
        })
        .collect();
    kept.reverse();
    Snapshot::from_exchanges(kept)
}

impl Snapshot {
    fn from_exchanges(exchanges: Vec<Exchange>) -> Self {
        let id = content_id(&jsonl(&exchanges));
        Self { id, exchanges }
    }

    /// Forme canonique : un échange par ligne.
    pub fn to_jsonl(&self) -> Vec<u8> {
        jsonl(&self.exchanges)
    }

    /// Relit un snapshot et vérifie que son contenu correspond à l'identifiant attendu.
    pub fn from_jsonl(bytes: &[u8], expected_id: &str) -> Result<Self, String> {
        let mut exchanges = vec![];
        for (i, line) in bytes.split(|b| *b == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let e: Exchange =
                serde_json::from_slice(line).map_err(|e| format!("ligne {}: {e}", i + 1))?;
            exchanges.push(e);
        }
        let s = Self::from_exchanges(exchanges);
        if s.id != expected_id {
            return Err(format!(
                "contenu du snapshot {expected_id} altéré (empreinte {})",
                s.id
            ));
        }
        Ok(s)
    }
}

fn jsonl(exchanges: &[Exchange]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in exchanges {
        if let Ok(line) = serde_json::to_vec(e) {
            out.extend_from_slice(&line);
            out.push(b'\n');
        }
    }
    out
}

fn content_id(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}
