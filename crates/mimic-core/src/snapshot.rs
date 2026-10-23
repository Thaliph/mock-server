//! Snapshots de mock (spec §5.6) : jeu figé d'échanges sortants, identique pour tous les pods
//! d'un workload et identifié par l'empreinte de son contenu, et sa forme de mock, réduite à ce
//! que le moteur du sidecar en garde.

use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::mock::op_key;
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

/// Construit le snapshot : indépendant de l'ordre d'arrivée et des doublons. Accepte des
/// échanges ou des références ; seuls les échanges gardés sont copiés.
pub fn build<E: Borrow<Exchange>>(
    exchanges: impl IntoIterator<Item = E>,
    opts: &SnapshotOptions,
) -> Snapshot {
    // Tri total : date, identifiant, puis, pour départager deux versions d'un même id, la
    // version rejouable d'abord (un exemple par endpoint garde son body quand la mémoire l'a
    // retiré, §5.5.3), enfin le contenu sérialisé. La sérialisation n'est faite qu'en cas
    // d'égalité, rare. Le premier de chaque id est gardé.
    let mut keyed: Vec<E> = exchanges
        .into_iter()
        .filter(|e| {
            let e = e.borrow();
            e.direction == Direction::Outbound && e.status != 0
        })
        .collect();
    keyed.sort_by(|a, b| {
        let (a, b) = (a.borrow(), b.borrow());
        (a.started_at_ms, &a.id)
            .cmp(&(b.started_at_ms, &b.id))
            .then_with(|| {
                b.resp_body
                    .is_replayable()
                    .cmp(&a.resp_body.is_replayable())
            })
            .then_with(|| {
                let line = |e: &Exchange| serde_json::to_string(e).unwrap_or_default();
                line(a).cmp(&line(b))
            })
    });
    let mut seen = BTreeSet::new();
    let unique: Vec<&Exchange> = keyed
        .iter()
        .map(Borrow::borrow)
        .filter(|e: &&Exchange| seen.insert(e.id.as_str()))
        .collect();

    // Plafond par clé, en partant des plus récents. Il est compté à part pour les réponses
    // rejouables : au-delà du plafond d'exemples du sidecar, les échanges récents d'un endpoint
    // très sollicité n'ont plus de body et ne doivent pas évincer ceux qui en ont un.
    let mut per_key: BTreeMap<(&str, String, &str, u16, bool), usize> = BTreeMap::new();
    let mut kept: Vec<Exchange> = unique
        .into_iter()
        .rev()
        .filter(|e| {
            let n = per_key
                .entry((
                    e.authority.as_str(),
                    e.method.to_ascii_uppercase(),
                    e.path.as_str(),
                    e.status,
                    e.resp_body.is_replayable(),
                ))
                .or_default();
            *n += 1;
            *n <= opts.max_per_key
        })
        .cloned()
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

    /// Relit un contenu JSONL sans identifiant attendu (import, SNP-11) : canonicalise
    /// l'ordre et recalcule l'identifiant à partir du contenu. Une ligne invalide est
    /// refusée avec son numéro.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let mut exchanges = vec![];
        for (i, line) in bytes.split(|b| *b == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let e: Exchange =
                serde_json::from_slice(line).map_err(|e| format!("ligne {}: {e}", i + 1))?;
            exchanges.push(e);
        }
        Ok(Self::from_exchanges(exchanges))
    }

    /// Relit un snapshot et vérifie que son contenu correspond à l'identifiant attendu.
    pub fn from_jsonl(bytes: &[u8], expected_id: &str) -> Result<Self, String> {
        let s = Self::parse(bytes)?;
        if s.id != expected_id {
            return Err(format!(
                "contenu du snapshot {expected_id} altéré (empreinte {})",
                s.id
            ));
        }
        Ok(s)
    }
}

/// Version du format de la forme de mock.
pub const MOCK_FORMAT: u32 = 1;

/// Forme de mock d'un snapshot (spec §5.6) : ce que le moteur du sidecar en garde. Un moteur
/// construit dessus (`MockEngine::from_snapshot`) est le même que sur le snapshot complet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MockSnapshot {
    /// Version du format ([`MOCK_FORMAT`]).
    pub format: u32,
    /// Identifiant du snapshot dont elle est tirée.
    pub id: String,
    /// Nombre d'échanges du snapshot.
    pub count: usize,
    /// Durées des opérations, par (authority, méthode) puis forme du chemin croissantes.
    pub operations: Vec<Operations>,
    /// Échanges rejouables de code ≥ 200, tels quels, dans l'ordre du snapshot.
    pub records: Vec<Exchange>,
}

/// Opérations d'une authority et d'une méthode (en majuscules).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operations {
    pub authority: String,
    pub method: String,
    /// Une entrée par forme de chemin.
    pub paths: Vec<PathLatencies>,
}

/// Durées d'une opération, désignée par le chemin de son premier échange : la durée totale de
/// chacun de ses échanges de code ≥ 200, rejouables ou non, dans l'ordre chronologique. C'est
/// la distribution où le moteur tire la latence. Sérialisée en `["/chemin", [durées]]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathLatencies(pub String, pub Vec<u64>);

impl MockSnapshot {
    /// Déduit la forme de mock du snapshot ; ne dépend que de son contenu.
    pub fn from_snapshot(snapshot: &Snapshot) -> Self {
        // Mêmes échanges que ceux que le moteur indexe : sortants, de réponse finale.
        let mut by_op: BTreeMap<String, (&Exchange, Vec<u64>)> = BTreeMap::new();
        let mut records = vec![];
        for e in &snapshot.exchanges {
            if e.direction != Direction::Outbound || e.status < 200 {
                continue;
            }
            // Le snapshot est trié par date puis identifiant, sans doublon : l'ordre
            // chronologique d'une opération est celui du moteur.
            by_op
                .entry(op_key(&e.authority, &e.method, &e.path))
                .or_insert_with(|| (e, vec![]))
                .1
                .push(e.total_us);
            if e.resp_body.is_replayable() {
                records.push(e.clone());
            }
        }
        // La clé commence par l'authority et la méthode : leurs opérations se suivent.
        let mut operations: Vec<Operations> = vec![];
        for (first, latencies) in by_op.into_values() {
            let method = first.method.to_ascii_uppercase();
            let path = PathLatencies(first.path.clone(), latencies);
            match operations.last_mut() {
                Some(o) if o.authority == first.authority && o.method == method => {
                    o.paths.push(path)
                }
                _ => operations.push(Operations {
                    authority: first.authority.clone(),
                    method,
                    paths: vec![path],
                }),
            }
        }
        Self {
            format: MOCK_FORMAT,
            id: snapshot.id.clone(),
            count: snapshot.exchanges.len(),
            operations,
            records,
        }
    }

    /// Forme canonique : un document JSON, mêmes octets pour un même snapshot.
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// Relit une forme de mock et vérifie qu'elle est celle du snapshot attendu.
    pub fn from_json(bytes: &[u8], expected_id: &str) -> Result<Self, String> {
        let s: Self =
            serde_json::from_slice(bytes).map_err(|e| format!("forme de mock illisible: {e}"))?;
        if s.format != MOCK_FORMAT {
            return Err(format!("format de forme de mock inconnu: {}", s.format));
        }
        if s.id != expected_id {
            return Err(format!(
                "forme de mock du snapshot {} au lieu de {expected_id}",
                s.id
            ));
        }
        Ok(s)
    }
}

/// Empreinte d'un contenu : 16 caractères hexadécimaux de son sha256 (identifiant d'un
/// snapshot, `ETag` de sa forme de mock).
pub fn digest(bytes: &[u8]) -> String {
    content_id(bytes)
}

/// Version du format des métadonnées d'un snapshot.
pub const SNAPSHOT_META_FORMAT: u32 = 1;

/// Métadonnées d'un snapshot (spec §5.6, cycle de vie) : écrites une seule fois à sa
/// construction ou à son import, à côté du contenu immuable (`<id>.jsonl`). `pinned` est la
/// seule à changer ensuite (`mimicctl snapshot pin`), ce qui le protège de la purge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotMeta {
    /// Version du format ([`SNAPSHOT_META_FORMAT`]).
    pub format: u32,
    /// Date de construction ou d'import, en millisecondes Unix.
    pub created_ms: i64,
    /// Nombre d'échanges du snapshot.
    pub exchanges: usize,
    /// Protégé de la purge (CTL-44), quel que soit son âge ou sa référence.
    pub pinned: bool,
}

impl SnapshotMeta {
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// Refuse un format inconnu ou un champ qui ne correspond pas aux métadonnées
    /// attendues, plutôt que de l'accepter silencieusement.
    pub fn from_json(bytes: &[u8]) -> Result<Self, String> {
        let m: Self =
            serde_json::from_slice(bytes).map_err(|e| format!("métadonnées illisibles: {e}"))?;
        if m.format != SNAPSHOT_META_FORMAT {
            return Err(format!(
                "format de métadonnées inconnu: {} (attendu {SNAPSHOT_META_FORMAT})",
                m.format
            ));
        }
        Ok(m)
    }
}

/// Un snapshot candidat à la purge, avec ce qu'il faut pour en décider (spec §5.6, cycle de
/// vie) : son identifiant, sa date (construction, import, ou dernier accès pour un cache de
/// sidecar) et s'il est épinglé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotEntry {
    pub id: String,
    pub created_ms: i64,
    pub pinned: bool,
}

/// Identifiants à purger (CTL-44, SNP-12) : jamais un `referenced` ni un `pinned`, jamais
/// l'un des `keep_last` plus récents (par `created_ms` puis `id`, pour départager une égalité
/// de date). Déterministe (INF-08) : indépendant de l'ordre de `candidates`.
pub fn to_purge(
    candidates: &[SnapshotEntry],
    referenced: &BTreeSet<String>,
    keep_last: usize,
) -> BTreeSet<String> {
    let mut sorted: Vec<&SnapshotEntry> = candidates.iter().collect();
    sorted.sort_by(|a, b| {
        b.created_ms
            .cmp(&a.created_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    let kept_recent: BTreeSet<&str> = sorted
        .into_iter()
        .take(keep_last)
        .map(|e| e.id.as_str())
        .collect();
    candidates
        .iter()
        .filter(|e| {
            !e.pinned && !referenced.contains(&e.id) && !kept_recent.contains(e.id.as_str())
        })
        .map(|e| e.id.clone())
        .collect()
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
