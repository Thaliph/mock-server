//! Moteur du mode mock : choisit la réponse enregistrée à servir pour une requête sortante
//! (spec §5.3). Pur et déterministe à graine égale ; l'attente et l'envoi sont faits par le proxy.
//!
//! Le moteur est immuable une fois construit et partageable entre threads (`respond(&self)`) :
//! l'aléa avance par un pas atomique, seuls les curseurs de `round-robin` et `sequence` ont un
//! verrou. Les enregistrements sont indexés par (authority, méthode, forme du chemin) : une
//! requête ne compare que les candidats rejouables de son opération.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::model::{Body, Direction, Exchange, Header, is_json, media_type};
use crate::sample::push_path_shape;

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

/// Pas de SplitMix64.
const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

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
    /// Latence à appliquer avant de répondre, en microsecondes.
    pub latency_us: u64,
    pub kind: MatchKind,
    pub score: f64,
    /// Identifiant de l'échange rejoué.
    pub exchange_id: String,
    /// Rang de l'enregistrement rejoué parmi les rejouables du moteur (`0..replayable()`) :
    /// permet au proxy de préparer la réponse une fois par enregistrement.
    pub record: usize,
    /// Échange rejoué, partagé avec le moteur (aucune copie du body).
    pub exchange: Arc<Exchange>,
}

impl MockResponse {
    /// Headers à rejouer : ceux de la réponse enregistrée, hors headers propres à la connexion.
    pub fn headers(&self) -> impl Iterator<Item = &Header> {
        self.exchange
            .resp_headers
            .iter()
            .filter(|h| !HOP_BY_HOP.contains(&h.name.as_str()))
    }

    /// Body enregistré (décodé ; l'encodage d'origine est dans `content-encoding`).
    pub fn body(&self) -> &Body {
        &self.exchange.resp_body
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MockDecision {
    Matched(MockResponse),
    Unmatched,
}

/// Source d'enregistrements pour le moteur : un échange, une référence ou un `Arc`. Seuls les
/// enregistrements rejouables sont gardés (`into_shared`) ; un `Arc` n'est pas copié.
pub trait Recording {
    fn exchange(&self) -> &Exchange;
    fn into_shared(self) -> Arc<Exchange>;
}

impl Recording for Exchange {
    fn exchange(&self) -> &Exchange {
        self
    }
    fn into_shared(self) -> Arc<Exchange> {
        Arc::new(self)
    }
}

impl Recording for Arc<Exchange> {
    fn exchange(&self) -> &Exchange {
        self
    }
    fn into_shared(self) -> Arc<Exchange> {
        self
    }
}

impl Recording for &Exchange {
    fn exchange(&self) -> &Exchange {
        self
    }
    fn into_shared(self) -> Arc<Exchange> {
        Arc::new(self.clone())
    }
}

/// Enregistrement rejouable indexé, avec ce que le score compare déjà calculé : une requête
/// mockée parcourt les candidats de son opération sans relire leurs bodies JSON.
#[derive(Debug)]
struct Recorded {
    e: Arc<Exchange>,
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
    fn new(e: Arc<Exchange>) -> Self {
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

/// Clé d'une opération : authority, méthode en majuscules et forme du chemin (§5.2), séparées
/// par `\u{1}` (absent d'une authority, d'une méthode et d'une forme). Deux chemins sont égaux
/// ou de même template si et seulement s'ils ont la même forme.
fn op_key(authority: &str, method: &str, path: &str) -> String {
    let mut key = String::with_capacity(authority.len() + method.len() + path.len() + 8);
    key.push_str(authority);
    key.push('\u{1}');
    key.extend(method.chars().map(|c| c.to_ascii_uppercase()));
    key.push('\u{1}');
    push_path_shape(&mut key, path);
    key
}

/// Enregistrements d'une opération.
#[derive(Debug, Default)]
struct Group {
    /// Rangs des enregistrements rejouables, dans l'ordre chronologique.
    records: Vec<usize>,
    /// Durées de tous les enregistrements de l'opération, rejouables ou non, dans l'ordre
    /// chronologique : la distribution où la latence est tirée.
    latencies: Vec<u64>,
}

/// Index des enregistrements sortants servables, immuable et partageable entre threads.
#[derive(Debug)]
pub struct MockEngine {
    config: MockConfig,
    /// Enregistrements rejouables ; `MockResponse::record` en est le rang.
    records: Vec<Recorded>,
    /// Par clé d'opération (`op_key`).
    groups: BTreeMap<String, Group>,
    authorities: BTreeSet<String>,
    cursors: Mutex<BTreeMap<String, usize>>,
    /// État de SplitMix64, avancé d'un pas atomique par réponse.
    rng: AtomicU64,
    len: usize,
}

impl MockEngine {
    /// Construit le moteur ; les échanges entrants ou sans réponse sont ignorés, et seuls les
    /// rejouables sont gardés (les autres ne laissent que leur durée).
    pub fn new<R: Recording>(exchanges: impl IntoIterator<Item = R>, config: MockConfig) -> Self {
        let mut by_op: BTreeMap<String, Vec<R>> = BTreeMap::new();
        let mut len = 0;
        for r in exchanges {
            let e = r.exchange();
            // Un `1xx` n'est pas une réponse finale : une poignée de main WebSocket (`101`) n'a
            // pas de réponse rejouable et sa durée est celle de la connexion (spec §5.3).
            if e.direction != Direction::Outbound || e.status < 200 {
                continue;
            }
            len += 1;
            let key = op_key(&e.authority, &e.method, &e.path);
            match by_op.get_mut(&key) {
                Some(list) => list.push(r),
                None => {
                    by_op.insert(key, vec![r]);
                }
            }
        }
        let mut records = vec![];
        let mut groups = BTreeMap::new();
        let mut authorities = BTreeSet::new();
        for (key, mut items) in by_op {
            items.sort_by(|a, b| {
                let (a, b) = (a.exchange(), b.exchange());
                (a.started_at_ms, &a.id).cmp(&(b.started_at_ms, &b.id))
            });
            let mut g = Group {
                records: vec![],
                latencies: items.iter().map(|r| r.exchange().total_us).collect(),
            };
            for r in items {
                if r.exchange().resp_body.is_replayable() {
                    g.records.push(records.len());
                    records.push(Recorded::new(r.into_shared()));
                }
            }
            if let Some((authority, _)) = key.split_once('\u{1}')
                && !authorities.contains(authority)
            {
                authorities.insert(authority.to_string());
            }
            groups.insert(key, g);
        }
        Self {
            rng: AtomicU64::new(config.seed),
            config,
            records,
            groups,
            authorities,
            cursors: Mutex::new(BTreeMap::new()),
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

    /// Nombre d'enregistrements rejouables : les valeurs possibles de `MockResponse::record`.
    pub fn replayable(&self) -> usize {
        self.records.len()
    }

    /// Authorities pour lesquelles des enregistrements existent.
    pub fn authorities(&self) -> Vec<String> {
        self.authorities.iter().cloned().collect()
    }

    fn group(&self, req: &MockRequest) -> Option<&Group> {
        self.groups
            .get(&op_key(&req.authority, &req.method, &req.path))
    }

    /// Nombre d'enregistrements comparés à cette requête : les rejouables de son opération.
    pub fn candidates(&self, req: &MockRequest) -> usize {
        self.group(req).map_or(0, |g| g.records.len())
    }

    /// Deux tirages indépendants issus d'un seul pas du générateur : sous concurrence, chaque
    /// réponse reçoit un état distinct, et l'ensemble des états est celui d'une exécution en
    /// série de même graine.
    fn draw(&self) -> (u64, u64) {
        let s = self
            .rng
            .fetch_add(GAMMA, Ordering::Relaxed)
            .wrapping_add(GAMMA);
        (mix(s), mix(s ^ 0xD6E8_FEB8_6659_FD93))
    }

    pub fn respond(&self, req: &MockRequest) -> MockDecision {
        let config = &self.config;
        let Some(group) = self.group(req).filter(|g| !g.records.is_empty()) else {
            return MockDecision::Unmatched;
        };
        let requested = segments(&req.path);
        let (json, pairs) = json_parts(&req.body);
        let probe = Probe {
            query: query_pairs(&req.query),
            json_type: find(&req.headers, "content-type").is_some_and(json_type),
            json,
            pairs,
        };
        // Même forme de chemin : égal, ou seulement de même template.
        let servable: Vec<(usize, PathMatch, f64)> = group
            .records
            .iter()
            .map(|&i| {
                let r = &self.records[i];
                let m = if r.segments == requested {
                    PathMatch::Exact
                } else {
                    PathMatch::Template
                };
                (i, m, score(config, r, m, req, &probe))
            })
            .collect();
        let best = servable.iter().map(|(_, _, s)| *s).fold(f64::MIN, f64::max);
        let top: Vec<&(usize, PathMatch, f64)> =
            servable.iter().filter(|(_, _, s)| *s == best).collect();
        let (pick, latency_draw) = self.draw();

        let chosen = match config.strategy {
            Strategy::Best => self.most_frequent(&top),
            Strategy::RoundRobin => {
                let i = self.next_cursor(req);
                top[i % top.len()]
            }
            Strategy::Sequence => {
                let i = self.next_cursor(req);
                &servable[i % servable.len()]
            }
            Strategy::WeightedStatus => top[below(pick, top.len())],
        };
        let (record, m, score) = *chosen;
        let kind = if m == PathMatch::Template {
            MatchKind::Template
        } else if score >= 1.0 {
            MatchKind::Exact
        } else {
            MatchKind::Fuzzy
        };

        let sampled = group.latencies[below(latency_draw, group.latencies.len())];
        let latency_us = (sampled as f64 * config.latency_factor.max(0.0)).round() as u64;
        let e = &self.records[record].e;
        MockDecision::Matched(MockResponse {
            status: e.status,
            latency_us,
            kind,
            score: (score * 100.0).round() / 100.0,
            exchange_id: e.id.clone(),
            record,
            exchange: e.clone(),
        })
    }

    /// Réponse la plus fréquente parmi des candidats triés chronologiquement ; à égalité,
    /// celle dont l'exemplaire est le plus récent. Renvoie l'exemplaire le plus récent.
    fn most_frequent<'a, T>(&self, top: &[&'a (usize, T, f64)]) -> &'a (usize, T, f64) {
        // Par réponse (code, body) : nombre d'exemplaires et index du plus récent.
        let mut groups: BTreeMap<(u16, &[u8]), (usize, usize)> = BTreeMap::new();
        for (i, c) in top.iter().enumerate() {
            let e = &self.records[c.0].e;
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

    /// Curseur de `round-robin` et `sequence`, par chemin demandé ; seul verrou du moteur.
    fn next_cursor(&self, req: &MockRequest) -> usize {
        let key = format!(
            "{}|{}|{}",
            req.authority,
            req.method.to_ascii_uppercase(),
            req.path
        );
        let mut cursors = self.cursors.lock().unwrap_or_else(|e| e.into_inner());
        let c = cursors.entry(key).or_default();
        let i = *c;
        *c += 1;
        i
    }
}

/// Finaliseur de SplitMix64 : rapide, reproductible, suffisant pour du tirage non
/// cryptographique.
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Rang dans `0..n` tiré de `v`.
fn below(v: u64, n: usize) -> usize {
    if n <= 1 { 0 } else { (v % n as u64) as usize }
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
