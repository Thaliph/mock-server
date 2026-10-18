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

/// Enregistrement indexé, avec ce que le score compare déjà calculé : une requête mockée
/// parcourt tous les candidats de son opération, sans relire leurs bodies JSON.
#[derive(Debug)]
struct Recorded {
    e: Exchange,
    segments: Vec<String>,
    query: BTreeSet<String>,
    /// Content-type JSON déclaré, si la requête enregistrée en porte un.
    json_type: Option<bool>,
    /// Body de requête lu comme JSON, s'il l'est.
    json: Option<Value>,
    /// Couples (clé de premier niveau, valeur) de ce body JSON.
    pairs: BTreeSet<String>,
}

/// Mêmes éléments, calculés une fois pour la requête à servir.
struct Probe {
    query: BTreeSet<String>,
    json_type: bool,
    json: Option<Value>,
    pairs: BTreeSet<String>,
}

/// Segments non vides d'un chemin.
fn segments(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn json_type(content_type: &str) -> bool {
    is_json(&media_type(content_type))
}

fn query_pairs(query: &[(String, String)]) -> BTreeSet<String> {
    query.iter().map(|(k, v)| format!("{k}={v}")).collect()
}

fn json_parts(body: &[u8]) -> (Option<Value>, BTreeSet<String>) {
    match serde_json::from_slice::<Value>(body) {
        Ok(v) => {
            let pairs = top_level_pairs(&v).collect();
            (Some(v), pairs)
        }
        Err(_) => (None, BTreeSet::new()),
    }
}

impl Recorded {
    fn new(e: Exchange) -> Self {
        let (json, pairs) = json_parts(e.req_body.bytes().unwrap_or_default());
        Self {
            segments: segments(&e.path),
            json_type: e.req_header("content-type").map(json_type),
            query: query_pairs(&e.query),
            json,
            pairs,
            e,
        }
    }
}

/// Index des enregistrements sortants servables.
#[derive(Debug)]
pub struct MockEngine {
    config: MockConfig,
    /// Enregistrements triés chronologiquement, par (authority, méthode).
    by_route: BTreeMap<(String, String), Vec<Recorded>>,
    cursors: BTreeMap<String, usize>,
    rng: SplitMix64,
    len: usize,
}

impl MockEngine {
    pub fn new(exchanges: Vec<Exchange>, config: MockConfig) -> Self {
        let mut by_route: BTreeMap<(String, String), Vec<Recorded>> = BTreeMap::new();
        let mut len = 0;
        for e in exchanges {
            if e.direction != Direction::Outbound || e.status == 0 {
                continue;
            }
            len += 1;
            by_route
                .entry((e.authority.clone(), e.method.to_ascii_uppercase()))
                .or_default()
                .push(Recorded::new(e));
        }
        for list in by_route.values_mut() {
            list.sort_by(|a, b| (a.e.started_at_ms, &a.e.id).cmp(&(b.e.started_at_ms, &b.e.id)));
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
        let requested = segments(&req.path);
        let same_op: Vec<(&Recorded, PathMatch)> = list
            .iter()
            .filter_map(|r| path_match(&r.segments, &requested).map(|m| (r, m)))
            .collect();
        let latencies: Vec<u64> = same_op.iter().map(|(r, _)| r.e.total_us).collect();
        let (json, pairs) = json_parts(&req.body);
        let probe = Probe {
            query: query_pairs(&req.query),
            json_type: find(&req.headers, "content-type").is_some_and(json_type),
            json,
            pairs,
        };
        let servable: Vec<(&Exchange, PathMatch, f64)> = same_op
            .into_iter()
            .filter(|(r, _)| r.e.resp_body.is_replayable())
            .map(|(r, m)| (&r.e, m, score(config, r, m, req, &probe)))
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

fn score(config: &MockConfig, r: &Recorded, m: PathMatch, req: &MockRequest, probe: &Probe) -> f64 {
    let e = &r.e;
    let path = match m {
        PathMatch::Exact => 1.0,
        PathMatch::Template => 0.5,
    };
    let query = jaccard(&r.query, &probe.query);
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
    let body = body_similarity(r, req, probe);
    W_PATH * path + W_QUERY * query + W_HEADERS * headers + W_BODY * body
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathMatch {
    Exact,
    Template,
}

/// Chemins égaux, ou de même forme : seuls diffèrent des segments identifiants de même nature.
fn path_match(a: &[String], b: &[String]) -> Option<PathMatch> {
    if a == b {
        return Some(PathMatch::Exact);
    }
    if a.len() != b.len() {
        return None;
    }
    let compatible = a
        .iter()
        .zip(b)
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

fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let common = a.intersection(b).count();
    common as f64 / (a.len() + b.len() - common) as f64
}

fn body_similarity(r: &Recorded, req: &MockRequest, probe: &Probe) -> f64 {
    let recorded = r.e.req_body.bytes().unwrap_or_default();
    if recorded.is_empty() && req.body.is_empty() {
        return 1.0;
    }
    let json = r.json_type.unwrap_or(probe.json_type);
    if json && let (Some(a), Some(b)) = (&r.json, &probe.json) {
        if a == b {
            return 1.0;
        }
        return jaccard(&r.pairs, &probe.pairs);
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
