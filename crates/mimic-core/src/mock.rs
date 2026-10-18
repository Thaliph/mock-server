//! Moteur du mode mock : choisit la réponse enregistrée à servir pour une requête sortante
//! (spec §5.3). Pur et déterministe à graine égale ; l'attente et l'envoi sont faits par le proxy.
//!
//! Le moteur est immuable une fois construit et partageable entre threads (`respond(&self)`) :
//! l'aléa et les curseurs de `round-robin` et `sequence` avancent par pas atomiques, sans
//! verrou. Les enregistrements sont indexés par (authority, méthode, forme du chemin) : une
//! requête ne compare que les candidats rejouables de son opération.
//!
//! Les curseurs sont fixés à la construction : un par chemin enregistré, un par opération pour
//! les chemins jamais enregistrés. Des identifiants uniques dans les chemins demandés ne les
//! font donc pas grandir.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use serde_json::Value;

use crate::model::{Body, Direction, Exchange, Header, is_json, media_type};
use crate::sample::push_path_shape;
use crate::snapshot::{MockSnapshot, PathLatencies};

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
    /// Curseur de son chemin (`round-robin`, `sequence`), commun aux enregistrements du même
    /// chemin.
    cursor: usize,
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
            cursor: 0,
        }
    }
}

/// Clé d'une opération : authority, méthode en majuscules et forme du chemin (§5.2), séparées
/// par `\u{1}` (absent d'une authority, d'une méthode et d'une forme). Deux chemins sont égaux
/// ou de même template si et seulement s'ils ont la même forme : c'est la clé sous laquelle le
/// moteur range les enregistrements, et sous laquelle le proxy indexe ceux arrivés depuis.
pub fn op_key(authority: &str, method: &str, path: &str) -> String {
    let mut key = String::with_capacity(authority.len() + method.len() + path.len() + 8);
    key.push_str(authority);
    key.push('\u{1}');
    key.extend(method.chars().map(|c| c.to_ascii_uppercase()));
    key.push('\u{1}');
    push_path_shape(&mut key, path);
    key
}

/// Trie des enregistrements par date, puis identifiant.
fn sort_chronologically<R: Recording>(items: &mut [R]) {
    items.sort_by(|a, b| {
        let (a, b) = (a.exchange(), b.exchange());
        (a.started_at_ms, &a.id).cmp(&(b.started_at_ms, &b.id))
    });
}

/// Enregistrements d'une opération.
#[derive(Debug, Default)]
struct Group {
    /// Rangs des enregistrements rejouables, dans l'ordre chronologique.
    records: Vec<usize>,
    /// Durées de tous les enregistrements de l'opération, rejouables ou non, dans l'ordre
    /// chronologique : la distribution où la latence est tirée.
    latencies: Vec<u64>,
    /// Curseur des requêtes dont le chemin n'a jamais été enregistré.
    cursor: usize,
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
    /// Curseurs de `round-robin` et `sequence` : un par chemin enregistré et un par opération.
    cursors: Vec<AtomicUsize>,
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
        let ops = by_op.into_iter().map(|(key, mut items)| {
            sort_chronologically(&mut items);
            let latencies = items.iter().map(|r| r.exchange().total_us).collect();
            let replayable = items
                .into_iter()
                .filter(|r| r.exchange().resp_body.is_replayable())
                .map(Recording::into_shared)
                .collect();
            (key, latencies, replayable)
        });
        Self::assemble(ops, len, config)
    }

    /// Construit le moteur sur la forme de mock d'un snapshot (spec §5.6) : le même moteur
    /// que sur le snapshot complet. Les durées viennent des opérations de la forme ; les
    /// enregistrements sont ses échanges rejouables.
    pub fn from_snapshot(snapshot: &MockSnapshot, config: MockConfig) -> Self {
        type Op<'a> = (Vec<u64>, Vec<&'a Exchange>);
        let mut by_op: BTreeMap<String, Op<'_>> = BTreeMap::new();
        for ops in &snapshot.operations {
            for PathLatencies(path, latencies) in &ops.paths {
                by_op
                    .entry(op_key(&ops.authority, &ops.method, path))
                    .or_default()
                    .0
                    .extend(latencies);
            }
        }
        for e in &snapshot.records {
            if e.direction != Direction::Outbound || e.status < 200 || !e.resp_body.is_replayable()
            {
                continue;
            }
            by_op
                .entry(op_key(&e.authority, &e.method, &e.path))
                .or_default()
                .1
                .push(e);
        }
        let mut len = 0;
        let ops = by_op
            .into_iter()
            .map(|(key, (mut latencies, mut items))| {
                sort_chronologically(&mut items);
                // Forme incohérente (opération absente) : la distribution n'est jamais vide.
                if latencies.is_empty() {
                    latencies = items.iter().map(|e| e.total_us).collect();
                }
                len += latencies.len();
                let replayable = items.into_iter().map(Recording::into_shared).collect();
                (key, latencies, replayable)
            })
            .collect::<Vec<_>>();
        Self::assemble(ops, len, config)
    }

    /// Index des opérations : pour chacune, les durées de tous ses enregistrements et ses
    /// enregistrements rejouables, dans l'ordre chronologique, par clé croissante.
    fn assemble(
        ops: impl IntoIterator<Item = (String, Vec<u64>, Vec<Arc<Exchange>>)>,
        len: usize,
        config: MockConfig,
    ) -> Self {
        let mut records: Vec<Recorded> = vec![];
        let mut groups = BTreeMap::new();
        let mut authorities = BTreeSet::new();
        let mut cursors = 0;
        for (key, latencies, replayable) in ops {
            let mut g = Group {
                records: vec![],
                latencies,
                cursor: cursors,
            };
            cursors += 1;
            // Curseur de chaque chemin enregistré de l'opération.
            let mut paths: BTreeMap<Vec<String>, usize> = BTreeMap::new();
            for e in replayable {
                let mut rec = Recorded::new(e);
                rec.cursor = match paths.get(&rec.segments) {
                    Some(c) => *c,
                    None => {
                        paths.insert(rec.segments.clone(), cursors);
                        cursors += 1;
                        cursors - 1
                    }
                };
                g.records.push(records.len());
                records.push(rec);
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
            cursors: (0..cursors).map(|_| AtomicUsize::new(0)).collect(),
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

    /// Nombre de curseurs de `round-robin` et `sequence` : un par chemin enregistré et un par
    /// opération, fixé à la construction.
    pub fn cursors(&self) -> usize {
        self.cursors.len()
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
                let i = self.next_cursor(group, &servable);
                top[i % top.len()]
            }
            Strategy::Sequence => {
                let i = self.next_cursor(group, &servable);
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

    /// Avance le curseur de `round-robin` et `sequence` : celui du chemin demandé s'il a été
    /// enregistré, sinon celui de l'opération.
    fn next_cursor(&self, group: &Group, servable: &[(usize, PathMatch, f64)]) -> usize {
        let cursor = servable
            .iter()
            .find(|(_, m, _)| *m == PathMatch::Exact)
            .map_or(group.cursor, |(i, _, _)| self.records[*i].cursor);
        self.cursors[cursor].fetch_add(1, Ordering::Relaxed)
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
