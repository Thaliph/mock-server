//! Moteur du mode mock : choisit la réponse enregistrée à servir pour une requête sortante
//! (spec §5.3). Pur et déterministe à graine égale ; l'attente et l'envoi sont faits par le proxy.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::infer::path::is_identifier;
use crate::model::{Body, Direction, Exchange, Header, is_json, media_type};

/// Headers à ne jamais rejouer : propres à une connexion, ou recalculés par le proxy.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "content-length",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

const W_PATH: f64 = 0.4;
const W_QUERY: f64 = 0.3;
const W_HEADERS: f64 = 0.1;
const W_BODY: f64 = 0.2;

/// Choix parmi plusieurs enregistrements candidats.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Strategy {
    /// Meilleur score, puis la réponse la plus fréquente, puis la plus récente.
    #[default]
    Best,
    /// Rotation sur les candidats de meilleur score.
    RoundRobin,
    /// Rejeu dans l'ordre d'enregistrement de tous les candidats, quel que soit le score.
    Sequence,
    /// Tirage uniforme parmi les candidats de meilleur score : reproduit la répartition des codes.
    WeightedStatus,
}

#[derive(Debug, Clone)]
pub struct MockConfig {
    pub strategy: Strategy,
    /// Multiplicateur de la latence rejouée ; 0 la désactive.
    pub latency_factor: f64,
    /// Headers de requête qui départagent les candidats (noms insensibles à la casse).
    pub match_headers: Vec<String>,
    /// Graine du générateur pseudo-aléatoire.
    pub seed: u64,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            strategy: Strategy::Best,
            latency_factor: 1.0,
            match_headers: vec![],
            seed: 0x5EED,
        }
    }
}

/// Requête sortante à servir.
#[derive(Debug, Clone, Default)]
pub struct MockRequest {
    pub authority: String,
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<Header>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchKind {
    Exact,
    Template,
    Fuzzy,
}

impl MatchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MatchKind::Exact => "exact",
            MatchKind::Template => "template",
            MatchKind::Fuzzy => "fuzzy",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MockResponse {
    pub status: u16,
    pub headers: Vec<Header>,
    pub body: Body,
    /// Latence à appliquer avant de répondre, en microsecondes.
    pub latency_us: u64,
    pub kind: MatchKind,
    pub score: f64,
    /// Identifiant de l'échange rejoué.
    pub exchange_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MockDecision {
    Matched(MockResponse),
    Unmatched,
}

/// Index des enregistrements sortants servables.
#[derive(Debug)]
pub struct MockEngine {
    config: MockConfig,
    /// Enregistrements triés chronologiquement, par (authority, méthode).
    by_route: BTreeMap<(String, String), Vec<Exchange>>,
    cursors: BTreeMap<String, usize>,
    rng: SplitMix64,
    len: usize,
}

impl MockEngine {
    pub fn new(exchanges: Vec<Exchange>, config: MockConfig) -> Self {
        let mut by_route: BTreeMap<(String, String), Vec<Exchange>> = BTreeMap::new();
        let mut len = 0;
        for e in exchanges {
            if e.direction != Direction::Outbound || e.status == 0 {
                continue;
            }
            len += 1;
            by_route
                .entry((e.authority.clone(), e.method.to_ascii_uppercase()))
                .or_default()
                .push(e);
        }
        for list in by_route.values_mut() {
            list.sort_by(|a, b| (a.started_at_ms, &a.id).cmp(&(b.started_at_ms, &b.id)));
        }
        let rng = SplitMix64(config.seed);
        Self {
            config,
            by_route,
            cursors: BTreeMap::new(),
            rng,
            len,
        }
    }

    /// Nombre d'enregistrements indexés.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Authorities pour lesquelles des enregistrements existent.
    pub fn authorities(&self) -> Vec<String> {
        let set: BTreeSet<&String> = self.by_route.keys().map(|(a, _)| a).collect();
        set.into_iter().cloned().collect()
    }

    pub fn respond(&mut self, req: &MockRequest) -> MockDecision {
        let Self {
            config,
            by_route,
            cursors,
            rng,
            ..
        } = self;
        let key = (req.authority.clone(), req.method.to_ascii_uppercase());
        let Some(list) = by_route.get(&key) else {
            return MockDecision::Unmatched;
        };
        // Candidats de même opération : chemin égal ou de même template.
        let same_op: Vec<(&Exchange, PathMatch)> = list
            .iter()
            .filter_map(|e| path_match(&e.path, &req.path).map(|m| (e, m)))
            .collect();
        let latencies: Vec<u64> = same_op.iter().map(|(e, _)| e.total_us).collect();
        let servable: Vec<(&Exchange, PathMatch, f64)> = same_op
            .into_iter()
            .filter(|(e, _)| e.resp_body.is_replayable())
            .map(|(e, m)| {
                let s = score(config, e, m, req);
                (e, m, s)
            })
            .collect();
        if servable.is_empty() {
            return MockDecision::Unmatched;
        }
        let best = servable.iter().map(|(_, _, s)| *s).fold(f64::MIN, f64::max);
        let top: Vec<&(&Exchange, PathMatch, f64)> =
            servable.iter().filter(|(_, _, s)| *s == best).collect();
        let cursor_key = format!("{}|{}|{}", key.0, key.1, req.path);

        let chosen = match config.strategy {
            Strategy::Best => most_frequent(&top),
            Strategy::RoundRobin => {
                let i = next_cursor(cursors, cursor_key);
                top[i % top.len()]
            }
            Strategy::Sequence => {
                let i = next_cursor(cursors, cursor_key);
                &servable[i % servable.len()]
            }
            Strategy::WeightedStatus => {
                let i = rng.below(top.len() as u64) as usize;
                top[i]
            }
        };
        let (e, m, score) = (chosen.0, chosen.1, chosen.2);
        let kind = if m == PathMatch::Template {
            MatchKind::Template
        } else if score >= 1.0 {
            MatchKind::Exact
        } else {
            MatchKind::Fuzzy
        };

        let sampled = latencies[rng.below(latencies.len() as u64) as usize];
        let latency_us = (sampled as f64 * config.latency_factor.max(0.0)).round() as u64;

        MockDecision::Matched(MockResponse {
            status: e.status,
            headers: e
                .resp_headers
                .iter()
                .filter(|h| !HOP_BY_HOP.contains(&h.name.as_str()))
                .cloned()
                .collect(),
            body: e.resp_body.clone(),
            latency_us,
            kind,
            score: (score * 100.0).round() / 100.0,
            exchange_id: e.id.clone(),
        })
    }
}

/// Réponse la plus fréquente parmi des candidats triés chronologiquement ; à égalité,
/// celle dont l'exemplaire est le plus récent. Renvoie l'exemplaire le plus récent.
fn most_frequent<'a, T>(top: &[&'a (&'a Exchange, T, f64)]) -> &'a (&'a Exchange, T, f64) {
    // Par réponse (code, body) : nombre d'exemplaires et index du plus récent.
    let mut groups: BTreeMap<(u16, &[u8]), (usize, usize)> = BTreeMap::new();
    for (i, c) in top.iter().enumerate() {
        let e = c.0;
        let g = groups
            .entry((e.status, e.resp_body.bytes().unwrap_or_default()))
            .or_default();
        g.0 += 1;
        g.1 = i;
    }
    let (_, last) = groups
        .values()
        .max_by_key(|(count, last)| (*count, *last))
        .copied()
        .expect("top non vide");
    top[last]
}

fn next_cursor(cursors: &mut BTreeMap<String, usize>, key: String) -> usize {
    let c = cursors.entry(key).or_default();
    let i = *c;
    *c += 1;
    i
}

fn score(config: &MockConfig, e: &Exchange, m: PathMatch, req: &MockRequest) -> f64 {
    let path = match m {
        PathMatch::Exact => 1.0,
        PathMatch::Template => 0.5,
    };
    let query = jaccard(
        e.query.iter().map(|(k, v)| format!("{k}={v}")),
        req.query.iter().map(|(k, v)| format!("{k}={v}")),
    );
    let headers = if config.match_headers.is_empty() {
        1.0
    } else {
        let same = config
            .match_headers
            .iter()
            .filter(|n| e.req_header(n) == find(&req.headers, n))
            .count();
        same as f64 / config.match_headers.len() as f64
    };
    let body = body_similarity(e, req);
    W_PATH * path + W_QUERY * query + W_HEADERS * headers + W_BODY * body
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathMatch {
    Exact,
    Template,
}

/// Chemins égaux, ou de même forme : seuls diffèrent des segments identifiants de même nature.
fn path_match(recorded: &str, requested: &str) -> Option<PathMatch> {
    let a: Vec<&str> = recorded.split('/').filter(|s| !s.is_empty()).collect();
    let b: Vec<&str> = requested.split('/').filter(|s| !s.is_empty()).collect();
    if a == b {
        return Some(PathMatch::Exact);
    }
    if a.len() != b.len() {
        return None;
    }
    let compatible = a
        .iter()
        .zip(&b)
        .all(|(x, y)| x == y || (is_identifier(x) && is_identifier(y) && same_shape(x, y)));
    compatible.then_some(PathMatch::Template)
}

fn same_shape(a: &str, b: &str) -> bool {
    use crate::infer::path::SegmentKind;
    SegmentKind::of(a) == SegmentKind::of(b)
}

fn find<'a>(headers: &'a [Header], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

fn jaccard(a: impl Iterator<Item = String>, b: impl Iterator<Item = String>) -> f64 {
    let a: BTreeSet<String> = a.collect();
    let b: BTreeSet<String> = b.collect();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    a.intersection(&b).count() as f64 / a.union(&b).count() as f64
}

fn body_similarity(e: &Exchange, req: &MockRequest) -> f64 {
    let recorded = e.req_body.bytes().unwrap_or_default();
    if recorded.is_empty() && req.body.is_empty() {
        return 1.0;
    }
    let json = e
        .req_header("content-type")
        .or_else(|| find(&req.headers, "content-type"))
        .map(|ct| is_json(&media_type(ct)))
        .unwrap_or(false);
    if json
        && let (Ok(a), Ok(b)) = (
            serde_json::from_slice::<Value>(recorded),
            serde_json::from_slice::<Value>(&req.body),
        )
    {
        if a == b {
            return 1.0;
        }
        return jaccard(top_level_pairs(&a), top_level_pairs(&b));
    }
    if recorded == req.body.as_slice() {
        1.0
    } else {
        0.0
    }
}

fn top_level_pairs(v: &Value) -> impl Iterator<Item = String> + '_ {
    v.as_object()
        .into_iter()
        .flat_map(|m| m.iter().map(|(k, v)| format!("{k}={v}")))
}

/// Générateur SplitMix64 : rapide, reproductible, suffisant pour du tirage non cryptographique.
#[derive(Debug, Clone)]
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n <= 1 { 0 } else { self.next() % n }
    }
}
