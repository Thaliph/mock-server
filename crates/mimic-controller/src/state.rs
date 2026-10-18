//! État du contrôleur : enregistrements par workload, état souhaité, pods, snapshots.
//!
//! Tenue en charge (spec §5.5.2) :
//! - l'état court d'un workload (`Live` : pods, état souhaité, ressource), ses échanges et
//!   l'index de ses segments ont chacun un verrou `std`, tenu quelques microsecondes et jamais
//!   pendant une I/O ou un calcul : un battement n'attend ni le support ni une inférence ;
//! - les échanges sont rangés par segment dans des tranches immuables partagées : un calcul en
//!   prend une vue figée sans les copier et tourne hors des threads du serveur
//!   (`spawn_blocking`), pendant que l'ingestion continue ;
//! - le contrôleur est le seul écrivant de son support : l'index des segments est construit par
//!   une seule liste, puis tenu à jour à chaque ingestion, compaction ou suppression ;
//! - chaque workload garde un exemple rejouable par endpoint (spec §5.5.3), hors des plafonds
//!   et de la rétention : un segment n'est supprimé qu'une fois ses échanges proposés aux
//!   exemples et ceux-ci écrits sur le support (`exemplars.jsonl`).

use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::future::{BoxFuture, FutureExt, Shared};
use mimic_core::compact::{BodySamples, Compactor, hour_of, hour_start};
use mimic_core::exemplar::{Exemplars, best_per_endpoint, jsonl};
use mimic_core::infer::{InferOptions, infer, infer_outbound};
use mimic_core::model::{Direction, Exchange};
use mimic_core::snapshot::{Snapshot, SnapshotOptions, build};
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::OnceCell;

use crate::auth::Authenticator;
use crate::config::ControllerConfig;
use crate::metrics::{Metrics, StateLabel};
use crate::reconcile::{
    Action, Desired, MimicWorkloadSpec, PodReport, PodState, Summary, desired_for_pod, desired_of,
    guard_pod, is_production, pod_state, reconcile_guarded, selects, summarize,
};

pub type Key = (String, String);

/// Pseudo-instance des segments horaires (`exchanges/_hour/<AAAAMMJJHH>.jsonl`).
pub const COMPACTED: &str = "_hour";
/// Une heure révolue depuis ce délai est compactée.
const COMPACT_GRACE_MS: i64 = 10 * 60 * 1000;
/// Octets de segments lus avant chaque analyse JSON lors d'un chargement.
const LOAD_BATCH: usize = 8 * 1024 * 1024;

pub struct Ctl {
    pub config: ControllerConfig,
    pub store: Arc<dyn ObjectStore>,
    pub auth: Authenticator,
    pub metrics: Metrics,
    workloads: Mutex<BTreeMap<Key, Arc<Entry>>>,
    /// Le support n'est listé qu'une fois : ensuite, tout nouveau workload passe par ce
    /// processus (battement ou segment) et entre dans `workloads`.
    scanned: OnceCell<()>,
    /// Échanges en mémoire, tous workloads confondus.
    total: AtomicUsize,
    /// Taille des exemples, tous workloads confondus (borne `max_exemplar_bytes_total`).
    exemplar_room: Room,
    /// Namespaces labellisés `env=production` : le mock y est refusé (spec §5.5.1).
    production: Mutex<BTreeSet<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Pod {
    pub instance: String,
    pub report: PodReport,
    pub last_seen_ms: u64,
    /// Labels du pod lus dans l'API à l'authentification (spec §7.1).
    pub labels: BTreeMap<String, String>,
}

/// Documents OpenAPI d'un workload.
pub struct Discovery {
    pub inbound: Value,
    pub outbound: BTreeMap<String, Value>,
}

/// État court d'un workload : battements, état souhaité et ressource.
#[derive(Default)]
pub struct Live {
    pub pods: BTreeMap<String, Pod>,
    pub desired: Desired,
    /// Ressource `MimicWorkload` courante, si elle existe.
    pub spec: Option<MimicWorkloadSpec>,
    pub generation: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SegmentInfo {
    pub key: String,
    pub instance: String,
    pub modified_ms: i64,
    pub size: u64,
}

/// Résultat d'une ingestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ingested {
    /// Clé relative du segment (`instance/fichier`).
    pub key: String,
    /// Segment déjà reçu sous cet identifiant : rien n'a été réécrit ni réimporté.
    pub duplicate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestError {
    /// Segment ou identifiant invalide.
    Invalid(String),
    /// Support indisponible : le sidecar renverra le segment.
    Storage(String),
}

/// Échanges d'un même segment, immuables une fois rangés.
struct Chunk {
    /// Instance qui a écrit le segment ; `None` pour un segment horaire, où l'instance se
    /// lit dans l'identifiant de chaque échange.
    instance: Option<String>,
    exchanges: Vec<Exchange>,
}

impl Chunk {
    fn instance_of<'a>(&'a self, e: &'a Exchange) -> &'a str {
        match &self.instance {
            Some(i) => i,
            // Identifiant `<instance>-<ULID>` (spec §5.4).
            None => instance_of_id(&e.id),
        }
    }
}

/// Échanges en mémoire d'un workload, du plus ancien au plus récent.
struct Data {
    /// Tranches, avec l'indice de leur premier échange encore gardé.
    chunks: VecDeque<(Arc<Chunk>, usize)>,
    len: usize,
    ids: BTreeSet<String>,
    bodies: BodySamples,
    /// Exemple de chaque endpoint (spec §5.5.3), hors des plafonds d'échanges.
    exemplars: Exemplars,
    /// Version des exemples déjà écrite sur le support.
    flushed: u64,
    /// Segments dont les échanges ont déjà été proposés aux exemples par ce processus.
    offered: BTreeSet<String>,
    /// Change à chaque ajout ou éviction, et à chaque changement d'exemple.
    version: u64,
}

/// Place des exemples commune à tous les workloads.
struct Room {
    /// Octets tenus ; pris sous le verrou des échanges d'un workload, jamais l'inverse.
    used: Mutex<u64>,
    max: u64,
}

/// Exemples refusés : (borne en nombre, bornes en octets).
type Refused = (u64, u64);

/// Exemples ajoutés à une vue.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Extra {
    /// Tous : le snapshot garde, de deux versions d'un échange, celle qui est rejouable.
    All,
    /// Seulement ceux dont l'échange n'est plus en mémoire (découverte, requêtes).
    Missing,
}

/// Vue figée des échanges d'un workload, du plus ancien au plus récent, suivis d'exemples.
struct View {
    chunks: Vec<(Arc<Chunk>, usize)>,
    extra: Vec<Arc<Exchange>>,
    version: u64,
}

impl View {
    fn iter(&self) -> impl Iterator<Item = Tagged<'_>> {
        self.chunks
            .iter()
            .flat_map(|(c, from)| {
                c.exchanges[*from..]
                    .iter()
                    .map(move |e| Tagged(c.instance_of(e), e))
            })
            .chain(self.extra.iter().map(|e| Tagged(instance_of_id(&e.id), e)))
    }
}

/// Instance d'un échange d'après son identifiant `<instance>-<ULID>` (spec §5.4).
fn instance_of_id(id: &str) -> &str {
    id.rsplit_once('-').map(|(i, _)| i).unwrap_or_default()
}

/// Copie des meilleurs candidats de `list` aux exemples, un par endpoint.
fn winners(list: &[Exchange]) -> Vec<Arc<Exchange>> {
    best_per_endpoint(list)
        .into_values()
        .map(|e| Arc::new(e.clone()))
        .collect()
}

/// Échange vu avec l'instance qui l'a écrit, sans copie.
pub struct Tagged<'a>(pub &'a str, pub &'a Exchange);

impl Borrow<Exchange> for Tagged<'_> {
    fn borrow(&self) -> &Exchange {
        self.1
    }
}

impl Data {
    fn new(samples: usize, max_exemplars: usize, max_exemplar_bytes: u64) -> Self {
        Self {
            chunks: VecDeque::new(),
            len: 0,
            ids: BTreeSet::new(),
            bodies: BodySamples::new(samples),
            exemplars: Exemplars::with_limits(max_exemplars, max_exemplar_bytes),
            flushed: 0,
            offered: BTreeSet::new(),
            version: 0,
        }
    }

    fn view(&self, extra: Extra) -> View {
        View {
            chunks: self.chunks.iter().cloned().collect(),
            extra: self
                .exemplars
                .iter()
                .filter(|x| extra == Extra::All || !self.ids.contains(&x.id))
                .cloned()
                .collect(),
            version: self.version,
        }
    }

    /// Propose des candidats aux exemples ; `key` est le segment d'où ils viennent. Rend les
    /// candidats refusés faute de place.
    fn offer(&mut self, key: Option<&str>, list: Vec<Arc<Exchange>>, room: &Room) -> Refused {
        if let Some(k) = key {
            self.offered.insert(k.to_string());
        }
        self.propose(list, room, false)
    }

    /// Reprend les exemples relus de `exemplars.jsonl`, ceux sans body compris.
    fn restore(&mut self, list: Vec<Arc<Exchange>>, room: &Room) -> Refused {
        self.propose(list, room, true)
    }

    fn propose(&mut self, list: Vec<Arc<Exchange>>, room: &Room, restore: bool) -> Refused {
        let x = &mut self.exemplars;
        let (version, refused, refused_bytes) = (x.version(), x.refused(), x.refused_bytes());
        let mut used = room.used.lock().expect("verrou de la place des exemples");
        for e in list {
            let (before, left) = (x.bytes(), room.max.saturating_sub(*used));
            if restore {
                x.restore_within(e, left);
            } else {
                x.offer_shared_within(e, left);
            }
            *used = (*used + x.bytes()).saturating_sub(before);
        }
        drop(used);
        if x.version() != version {
            self.version += 1;
        }
        (x.refused() - refused, x.refused_bytes() - refused_bytes)
    }

    /// Garde les échanges inconnus de `list`, sans doublon.
    fn fresh(&mut self, list: Vec<Exchange>) -> Vec<Exchange> {
        list.into_iter()
            .filter(|e| self.ids.insert(e.id.clone()))
            .collect()
    }

    /// Ajoute des échanges plus récents que tous les autres, en évinçant d'abord les plus
    /// anciens au-delà de `max` : une éviction libère sa place de body. Rend (ajoutés, évincés).
    fn push_back(&mut self, instance: &str, list: Vec<Exchange>, max: usize) -> (usize, usize) {
        let mut fresh = self.fresh(list);
        let over = (self.len + fresh.len()).saturating_sub(max);
        let evicted = self.evict_front(over.min(self.len));
        for e in fresh.drain(..over - evicted) {
            self.ids.remove(&e.id);
        }
        for e in &mut fresh {
            self.bodies.admit(e);
        }
        let added = fresh.len();
        if added > 0 {
            self.len += added;
            self.chunks.push_back((
                Arc::new(Chunk {
                    instance: Some(instance.to_string()),
                    exchanges: fresh,
                }),
                0,
            ));
            self.version += 1;
        }
        (added, evicted)
    }

    /// Ajoute des échanges plus anciens que tous les autres (relecture du support, du plus
    /// récent au plus ancien), dans la limite de `max`. Rend le nombre ajouté.
    fn push_front(&mut self, instance: Option<String>, list: Vec<Exchange>, max: usize) -> usize {
        let mut fresh = self.fresh(list);
        let room = max.saturating_sub(self.len);
        if fresh.len() > room {
            // Les plus anciens du segment ne tiennent pas.
            for e in fresh.drain(..fresh.len() - room) {
                self.ids.remove(&e.id);
            }
        }
        for e in &mut fresh {
            self.bodies.admit(e);
        }
        let added = fresh.len();
        if added > 0 {
            self.len += added;
            self.chunks.push_front((
                Arc::new(Chunk {
                    instance,
                    exchanges: fresh,
                }),
                0,
            ));
            self.version += 1;
        }
        added
    }

    /// Évince les `n` plus anciens échanges. Rend le nombre évincé.
    fn evict_front(&mut self, mut n: usize) -> usize {
        let mut evicted = 0;
        while n > 0 {
            let Some((chunk, from)) = self.chunks.front_mut() else {
                break;
            };
            let k = n.min(chunk.exchanges.len() - *from);
            for e in &chunk.exchanges[*from..*from + k] {
                self.ids.remove(&e.id);
                self.bodies.release(e);
            }
            *from += k;
            if *from == chunk.exchanges.len() {
                self.chunks.pop_front();
            }
            n -= k;
            evicted += k;
        }
        if evicted > 0 {
            self.len -= evicted;
            self.version += 1;
        }
        evicted
    }
}

/// Segments rangés sur le support, par clé relative et par date.
#[derive(Default)]
struct SegmentIndex {
    by_key: BTreeMap<String, (i64, u64)>,
    by_time: BTreeSet<(i64, String)>,
    bytes: u64,
}

impl SegmentIndex {
    fn contains(&self, key: &str) -> bool {
        self.by_key.contains_key(key)
    }

    /// Range un segment, ou remplace sa date et sa taille.
    fn insert(&mut self, key: String, at_ms: i64, size: u64) {
        self.remove(&key);
        self.by_time.insert((at_ms, key.clone()));
        self.by_key.insert(key, (at_ms, size));
        self.bytes += size;
    }

    fn remove(&mut self, key: &str) {
        if let Some((at, size)) = self.by_key.remove(key) {
            self.by_time.remove(&(at, key.to_string()));
            self.bytes -= size;
        }
    }

    /// Segments datés d'au moins `from_ms`, du plus ancien au plus récent.
    fn since(&self, from_ms: i64) -> Vec<SegmentInfo> {
        self.by_time
            .range((from_ms, String::new())..)
            .map(|(at, key)| SegmentInfo {
                instance: key
                    .split_once('/')
                    .map(|(i, _)| i)
                    .unwrap_or_default()
                    .into(),
                key: key.clone(),
                modified_ms: *at,
                size: self.by_key.get(key).map(|(_, s)| *s).unwrap_or_default(),
            })
            .collect()
    }

    /// Clés datées d'au moins `from_ms`, du plus récent au plus ancien.
    fn newest_first(&self, from_ms: i64) -> Vec<String> {
        self.by_time
            .range((from_ms, String::new())..)
            .rev()
            .map(|(_, k)| k.clone())
            .collect()
    }

    /// Clés datées d'avant `before_ms`, du plus ancien au plus récent.
    fn older_than(&self, before_ms: i64) -> Vec<(i64, String)> {
        self.by_time
            .range(..(before_ms, String::new()))
            .cloned()
            .collect()
    }

    fn oldest(&self) -> Option<String> {
        self.by_time.first().map(|(_, k)| k.clone())
    }
}

/// Dernier document de découverte calculé, et calcul en cours.
#[derive(Default)]
struct DiscoveryCache {
    doc: Option<Arc<Discovery>>,
    /// Version des échanges dont le document est issu.
    version: u64,
    /// Début de son calcul (ms Unix).
    at_ms: u64,
    running: Option<Shared<BoxFuture<'static, Option<Arc<Discovery>>>>>,
}

pub struct Entry {
    pub namespace: String,
    pub name: String,
    /// État souhaité relu du support au premier accès.
    desired_loaded: OnceCell<()>,
    live: Mutex<Live>,
    /// Index construit par une seule liste du support.
    indexed: OnceCell<()>,
    index: Mutex<SegmentIndex>,
    /// Échanges relus du support au premier besoin.
    loaded: OnceCell<()>,
    /// Vrai dès que la relecture a pris sa liste de segments, sous le verrou de l'index.
    load_started: AtomicBool,
    /// Exemples relus du support avant le premier exemple proposé.
    exemplars_loaded: OnceCell<()>,
    /// Une seule écriture des exemples à la fois : la dernière écrite est la plus récente.
    flush: tokio::sync::Mutex<()>,
    data: Mutex<Data>,
    discovery: Mutex<DiscoveryCache>,
}

impl Entry {
    fn live(&self) -> MutexGuard<'_, Live> {
        self.live.lock().expect("verrou de l'état du workload")
    }

    fn index(&self) -> MutexGuard<'_, SegmentIndex> {
        self.index.lock().expect("verrou de l'index des segments")
    }

    fn data(&self) -> MutexGuard<'_, Data> {
        self.data.lock().expect("verrou des échanges")
    }

    fn discovery(&self) -> MutexGuard<'_, DiscoveryCache> {
        self.discovery.lock().expect("verrou de la découverte")
    }

    fn base(&self) -> String {
        base(&self.namespace, &self.name)
    }

    fn segment_path(&self, key: &str) -> Path {
        Path::from(format!("{}/exchanges/{key}", self.base()))
    }

    fn exemplars_path(&self) -> Path {
        Path::from(format!("{}/exemplars.jsonl", self.base()))
    }

    /// Lance le calcul de la découverte en tâche de fond ; à appeler sous le verrou de
    /// `discovery`, qui reçoit le calcul avant que la tâche puisse le terminer.
    fn start_discovery(
        self: &Arc<Self>,
        cache: &mut DiscoveryCache,
    ) -> Shared<BoxFuture<'static, Option<Arc<Discovery>>>> {
        let view = self.data().view(Extra::Missing);
        let entry = self.clone();
        let started = now_ms();
        let work = async move {
            let version = view.version;
            let title = entry.name.clone();
            let doc = tokio::task::spawn_blocking(move || {
                let refs: Vec<&Exchange> = view.iter().map(|t| t.1).collect();
                let version = env!("CARGO_PKG_VERSION");
                Discovery {
                    inbound: infer(&refs, &InferOptions::new(title, version)),
                    outbound: infer_outbound(&refs, version),
                }
            })
            .await
            .ok()
            .map(Arc::new);
            let mut c = entry.discovery();
            c.running = None;
            if let Some(d) = &doc {
                c.doc = Some(d.clone());
                c.version = version;
                c.at_ms = started;
            }
            doc
        }
        .boxed()
        .shared();
        cache.running = Some(work.clone());
        tokio::spawn(work.clone());
        work
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Noms de namespace, workload, instance et fichier : caractères DNS, sans séparateur.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
}

/// Identifiant de segment fourni par un sidecar : un nom valide, hors noms réservés (`_…`).
pub fn valid_segment_id(s: &str) -> bool {
    valid_name(s) && !s.starts_with('_') && s.len() <= 64
}

fn base(ns: &str, wl: &str) -> String {
    format!("{ns}/{wl}")
}

/// Date d'un segment : la fin de son heure moins une milliseconde pour un segment horaire,
/// sa date d'écriture sinon.
fn segment_time(key: &str, written_ms: i64) -> i64 {
    key.strip_prefix(COMPACTED)
        .and_then(|r| r.strip_prefix('/'))
        .and_then(|f| f.strip_suffix(".jsonl"))
        .and_then(hour_start)
        .map_or(written_ms, |start| start + 3_600_000 - 1)
}

fn parse_lines(bytes: &[u8]) -> Vec<Exchange> {
    bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_slice(l).ok())
        .collect()
}

impl Ctl {
    pub fn new(config: ControllerConfig, store: Arc<dyn ObjectStore>, auth: Authenticator) -> Self {
        let exemplar_room = Room {
            used: Mutex::new(0),
            max: config.max_exemplar_bytes_total,
        };
        Self {
            config,
            store,
            auth,
            metrics: Metrics::default(),
            workloads: Mutex::new(BTreeMap::new()),
            scanned: OnceCell::new(),
            total: AtomicUsize::new(0),
            exemplar_room,
            production: Mutex::new(BTreeSet::new()),
        }
    }

    /// Vrai si le namespace est labellisé `env=production`.
    pub fn is_production(&self, ns: &str) -> bool {
        self.production
            .lock()
            .expect("verrou des namespaces")
            .contains(ns)
    }

    /// Labels courants d'un namespace (création, modification) ; un changement du garde-fou
    /// réconcilie aussitôt les ressources du namespace.
    pub async fn set_namespace_labels(&self, ns: &str, labels: &BTreeMap<String, String>) {
        let production = is_production(labels);
        let changed = {
            let mut set = self.production.lock().expect("verrou des namespaces");
            if production {
                set.insert(ns.to_string())
            } else {
                set.remove(ns)
            }
        };
        if !changed {
            return;
        }
        tracing::info!(namespace = ns, production, "garde-fou production");
        let entries: Vec<Arc<Entry>> = self
            .entries()
            .into_iter()
            .filter(|e| e.namespace == ns)
            .collect();
        for e in entries {
            let (spec, generation) = {
                let live = e.live();
                (live.spec.clone(), live.generation)
            };
            if spec.is_some() {
                self.apply_spec(ns, &e.name, spec, generation).await;
            }
        }
    }

    fn entry(&self, ns: &str, wl: &str) -> Arc<Entry> {
        let mut map = self.workloads.lock().expect("verrou des workloads");
        map.entry((ns.to_string(), wl.to_string()))
            .or_insert_with(|| {
                Arc::new(Entry {
                    namespace: ns.into(),
                    name: wl.into(),
                    desired_loaded: OnceCell::new(),
                    live: Mutex::new(Live::default()),
                    indexed: OnceCell::new(),
                    index: Mutex::new(SegmentIndex::default()),
                    loaded: OnceCell::new(),
                    load_started: AtomicBool::new(false),
                    exemplars_loaded: OnceCell::new(),
                    flush: tokio::sync::Mutex::new(()),
                    data: Mutex::new(Data::new(
                        self.config.samples_per_operation,
                        self.config.max_exemplars_per_workload,
                        self.config.max_exemplar_bytes_per_workload,
                    )),
                    discovery: Mutex::new(DiscoveryCache::default()),
                })
            })
            .clone()
    }

    fn entries(&self) -> Vec<Arc<Entry>> {
        self.workloads
            .lock()
            .expect("verrou des workloads")
            .values()
            .cloned()
            .collect()
    }

    /// Plus ancienne date encore dans la rétention.
    fn oldest_ms(&self) -> i64 {
        now_ms().saturating_sub(self.config.retention.as_millis() as u64) as i64
    }

    /// Workload dont l'état souhaité a été relu du support.
    async fn ready(&self, ns: &str, wl: &str) -> Arc<Entry> {
        let e = self.entry(ns, wl);
        e.desired_loaded
            .get_or_init(|| async {
                let path = Path::from(format!("{}/desired.json", base(ns, wl)));
                if let Ok(r) = self.store.get(&path).await
                    && let Ok(bytes) = r.bytes().await
                    && let Ok(d) = serde_json::from_slice::<Desired>(&bytes)
                {
                    e.live().desired = d;
                }
            })
            .await;
        e
    }

    /// Lecture de l'état court d'un workload ; le verrou n'est tenu que pendant `f`.
    pub async fn with_live<R>(&self, ns: &str, wl: &str, f: impl FnOnce(&Live) -> R) -> R {
        let e = self.ready(ns, wl).await;
        let live = e.live();
        f(&live)
    }

    /// Échanges en mémoire d'un workload.
    pub fn exchange_count(&self, ns: &str, wl: &str) -> usize {
        self.entry(ns, wl).data().len
    }

    /// Workloads connus : présents sur le support au démarrage, ou annoncés depuis.
    pub async fn workload_keys(&self) -> Vec<Key> {
        let scan = self
            .scanned
            .get_or_try_init(|| async {
                let namespaces = self
                    .store
                    .list_with_delimiter(None)
                    .await
                    .map_err(|e| e.to_string())?;
                for ns in namespaces.common_prefixes {
                    let wls = self
                        .store
                        .list_with_delimiter(Some(&ns))
                        .await
                        .map_err(|e| e.to_string())?;
                    for wl in wls.common_prefixes {
                        let parts: Vec<&str> = wl.as_ref().split('/').collect();
                        if let [n, w] = parts.as_slice() {
                            self.entry(n, w);
                        }
                    }
                }
                Ok::<(), String>(())
            })
            .await;
        if let Err(e) = scan {
            tracing::warn!(error = %e, "liste des workloads du support");
        }
        self.workloads
            .lock()
            .expect("verrou des workloads")
            .keys()
            .cloned()
            .collect()
    }

    /// Construit l'index des segments du workload par une seule liste du support.
    async fn index_of(&self, e: &Entry) -> Result<(), String> {
        e.indexed
            .get_or_try_init(|| async {
                let prefix = Path::from(format!("{}/exchanges", e.base()));
                let mut found = vec![];
                let mut listing = self.store.list(Some(&prefix));
                while let Some(meta) = listing.next().await {
                    let meta = meta.map_err(|err| format!("liste de {prefix}: {err}"))?;
                    let key = meta
                        .location
                        .as_ref()
                        .strip_prefix(prefix.as_ref())
                        .unwrap_or_default()
                        .trim_start_matches('/')
                        .to_string();
                    let Some((_, file)) = key.split_once('/') else {
                        continue;
                    };
                    if file.contains('/') || !file.ends_with(".jsonl") {
                        continue;
                    }
                    let at = segment_time(&key, meta.last_modified.timestamp_millis());
                    found.push((key, at, meta.size));
                }
                let mut index = e.index();
                for (key, at, size) in found {
                    index.insert(key, at, size);
                }
                Ok::<(), String>(())
            })
            .await
            .map(|_| ())
    }

    /// Relit les échanges du workload au premier besoin : segments de la rétention, du plus
    /// récent au plus ancien, jusqu'au plafond du workload (spec §5.5.2).
    async fn load(&self, e: &Entry) {
        if let Err(err) = self.exemplars_of(e).await {
            tracing::warn!(namespace = %e.namespace, workload = %e.name, error = %err, "relecture des exemples");
        }
        let result = e
            .loaded
            .get_or_try_init(|| async {
                self.index_of(e).await?;
                let keys = {
                    let index = e.index();
                    e.load_started.store(true, Ordering::Relaxed);
                    index.newest_first(self.oldest_ms())
                };
                let mut batch = vec![];
                let mut size = 0;
                for key in keys {
                    if e.data().len >= self.config.max_exchanges_per_workload {
                        break;
                    }
                    // Un segment illisible (supprimé entre-temps) est ignoré.
                    let Ok(r) = self.store.get(&e.segment_path(&key)).await else {
                        continue;
                    };
                    let Ok(bytes) = r.bytes().await else {
                        continue;
                    };
                    size += bytes.len();
                    batch.push((key, bytes));
                    if size >= LOAD_BATCH {
                        self.load_batch(e, std::mem::take(&mut batch)).await;
                        size = 0;
                    }
                }
                self.load_batch(e, batch).await;
                Ok::<(), String>(())
            })
            .await;
        if let Err(err) = result {
            tracing::warn!(namespace = %e.namespace, workload = %e.name, error = %err, "relecture des échanges");
        }
        self.enforce_budget();
    }

    /// Analyse hors des threads du serveur, puis range un lot de segments, du plus récent au
    /// plus ancien.
    async fn load_batch(&self, e: &Entry, batch: Vec<(String, Bytes)>) {
        if batch.is_empty() {
            return;
        }
        let parsed = tokio::task::spawn_blocking(move || {
            batch
                .into_iter()
                .map(|(key, bytes)| {
                    let instance = key
                        .split_once('/')
                        .map(|(i, _)| i)
                        .filter(|i| *i != COMPACTED)
                        .map(String::from);
                    let list = parse_lines(&bytes);
                    let best = winners(&list);
                    (key, instance, list, best)
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        let max = self.config.max_exchanges_per_workload;
        let (mut added, mut refused) = (0, (0, 0));
        {
            let mut d = e.data();
            for (key, instance, list, best) in parsed {
                // Proposés avant que le plafond de bodies ne retire les leurs.
                let (count, bytes) = d.offer(Some(&key), best, &self.exemplar_room);
                refused = (refused.0 + count, refused.1 + bytes);
                added += d.push_front(instance, list, max);
            }
        }
        self.metrics.refuse_exemplars(refused);
        self.total.fetch_add(added, Ordering::Relaxed);
    }

    /// Garde le total des échanges en mémoire sous `max_exchanges_total` : les plus anciens
    /// du workload qui en a le plus partent les premiers.
    fn enforce_budget(&self) {
        let max = self.config.max_exchanges_total;
        while self.total.load(Ordering::Relaxed) > max {
            let over = self.total.load(Ordering::Relaxed) - max;
            let mut sizes: Vec<(usize, Arc<Entry>)> = self
                .entries()
                .into_iter()
                .map(|e| {
                    let n = e.data().len;
                    (n, e)
                })
                .collect();
            sizes.sort_by_key(|(n, _)| std::cmp::Reverse(*n));
            let Some((largest, entry)) = sizes.first() else {
                break;
            };
            let next = sizes.get(1).map_or(0, |(n, _)| *n);
            // Jusqu'à la taille du suivant, pour répartir l'effort entre les plus gros.
            let n = over.min((largest - next).max(1));
            let evicted = entry.data().evict_front(n);
            if evicted == 0 {
                break;
            }
            self.total.fetch_sub(evicted, Ordering::Relaxed);
        }
    }

    /// Range un segment reçu d'un sidecar. Avec `segment`, l'envoi est idempotent : un
    /// segment déjà rangé sous cet identifiant n'est ni réécrit ni réimporté.
    pub async fn ingest(
        &self,
        ns: &str,
        wl: &str,
        instance: &str,
        segment: Option<&str>,
        body: Bytes,
    ) -> Result<Ingested, IngestError> {
        if let Some(s) = segment
            && !valid_segment_id(s)
        {
            return Err(IngestError::Invalid(format!(
                "identifiant de segment invalide: {s}"
            )));
        }
        if instance.starts_with('_') {
            return Err(IngestError::Invalid(format!(
                "instance réservée: {instance}"
            )));
        }
        let lines = body.clone();
        let parsed = tokio::task::spawn_blocking(move || {
            let mut parsed = vec![];
            for (i, line) in lines.split(|b| *b == b'\n').enumerate() {
                if line.is_empty() {
                    continue;
                }
                let e: Exchange =
                    serde_json::from_slice(line).map_err(|e| format!("ligne {}: {e}", i + 1))?;
                parsed.push(e);
            }
            let best = winners(&parsed);
            Ok::<_, String>((parsed, best))
        })
        .await
        .map_err(|e| IngestError::Invalid(e.to_string()))?
        .map_err(IngestError::Invalid)?;
        let (parsed, best) = parsed;

        let e = self.entry(ns, wl);
        self.index_of(&e).await.map_err(IngestError::Storage)?;
        // Les exemples déjà rangés passent avant ceux de ce segment (borne, §5.5.3).
        self.exemplars_of(&e).await.map_err(IngestError::Storage)?;
        let id = segment.map_or_else(|| ulid::Ulid::generate().to_string(), String::from);
        let key = format!("{instance}/{id}.jsonl");
        if e.index().contains(&key) {
            return Ok(Ingested {
                key,
                duplicate: true,
            });
        }
        let path = e.segment_path(&key);
        let create = PutOptions {
            mode: PutMode::Create,
            ..PutOptions::default()
        };
        let size = body.len() as u64;
        let written = match self
            .store
            .put_opts(&path, PutPayload::from(body.clone()), create)
            .await
        {
            Ok(_) => true,
            Err(object_store::Error::AlreadyExists { .. }) => false,
            // Support sans écriture conditionnelle : l'index en mémoire suffit, ce processus
            // étant le seul écrivant.
            Err(object_store::Error::NotImplemented { .. }) => {
                self.store
                    .put(&path, PutPayload::from(body))
                    .await
                    .map_err(|err| IngestError::Storage(format!("écriture de {path}: {err}")))?;
                true
            }
            Err(err) => return Err(IngestError::Storage(format!("écriture de {path}: {err}"))),
        };
        // Relecture pas encore commencée : elle partira de l'index, qui contient désormais ce
        // segment. Ingérer ne force donc pas la relecture de tout l'historique du workload.
        // Commencée : ses segments, plus anciens, sont rangés devant ceux-ci.
        let loading = {
            let mut index = e.index();
            index.insert(key.clone(), now_ms() as i64, size);
            e.load_started.load(Ordering::Relaxed)
        };
        // Exemples proposés tels que reçus, avant le plafond de bodies en mémoire.
        let refused = e.data().offer(Some(&key), best, &self.exemplar_room);
        self.metrics.refuse_exemplars(refused);
        if loading {
            let (added, evicted) =
                e.data()
                    .push_back(instance, parsed, self.config.max_exchanges_per_workload);
            self.total.fetch_add(added, Ordering::Relaxed);
            self.total.fetch_sub(evicted, Ordering::Relaxed);
            self.enforce_budget();
        }
        Ok(Ingested {
            key,
            duplicate: !written,
        })
    }

    /// Segments encore dans la rétention, datés d'au moins `since_ms` s'il est donné.
    pub async fn segments(
        &self,
        ns: &str,
        wl: &str,
        since_ms: Option<i64>,
    ) -> Result<Vec<SegmentInfo>, String> {
        let e = self.entry(ns, wl);
        self.index_of(&e).await?;
        let oldest = self.oldest_ms();
        let from = since_ms.map_or(oldest, |s| s.max(oldest));
        Ok(e.index().since(from))
    }

    pub async fn segment(
        &self,
        ns: &str,
        wl: &str,
        instance: &str,
        file: &str,
    ) -> Option<bytes::Bytes> {
        let path = Path::from(format!("{}/exchanges/{instance}/{file}", base(ns, wl)));
        self.store.get(&path).await.ok()?.bytes().await.ok()
    }

    pub async fn heartbeat(
        &self,
        ns: &str,
        wl: &str,
        instance: &str,
        report: PodReport,
        labels: BTreeMap<String, String>,
    ) -> Desired {
        let e = self.ready(ns, wl).await;
        let now = now_ms();
        let production = self.is_production(ns);
        let mut live = e.live();
        if !live.pods.contains_key(instance) {
            // Un nouveau pod remplace souvent un ancien (déploiement) : on oublie les disparus.
            self.forget_pods(&mut live, now);
        }
        // État propre au pod : sélection (spec.selector), puis garde-fou production.
        let desired = guard_pod(
            desired_for_pod(&live.desired, &labels),
            production,
            &report.mode,
        );
        live.pods.insert(
            instance.into(),
            Pod {
                instance: instance.into(),
                report,
                last_seen_ms: now,
                labels,
            },
        );
        desired
    }

    fn forget_pods(&self, live: &mut Live, now: u64) {
        let forget = self.config.pod_forget.as_millis() as u64;
        live.pods
            .retain(|_, p| now.saturating_sub(p.last_seen_ms) <= forget);
    }

    fn snapshot_path(ns: &str, wl: &str, id: &str) -> Path {
        Path::from(format!("{}/snapshots/{id}.jsonl", base(ns, wl)))
    }

    pub async fn snapshot_bytes(&self, ns: &str, wl: &str, id: &str) -> Option<bytes::Bytes> {
        if !valid_name(id) {
            return None;
        }
        self.store
            .get(&Self::snapshot_path(ns, wl, id))
            .await
            .ok()?
            .bytes()
            .await
            .ok()
    }

    pub async fn snapshots(&self, ns: &str, wl: &str) -> Vec<Value> {
        let prefix = Path::from(format!("{}/snapshots", base(ns, wl)));
        let mut out = vec![];
        let mut listing = self.store.list(Some(&prefix));
        while let Some(Ok(meta)) = listing.next().await {
            let Some(id) = meta
                .location
                .filename()
                .and_then(|f| f.strip_suffix(".jsonl"))
            else {
                continue;
            };
            out.push(serde_json::json!({
                "id": id,
                "size": meta.size,
                "modified_ms": meta.last_modified.timestamp_millis(),
            }));
        }
        out.sort_by_key(|v| std::cmp::Reverse(v["modified_ms"].as_i64()));
        out
    }

    async fn snapshot_exists(&self, ns: &str, wl: &str, id: &str) -> bool {
        valid_name(id)
            && self
                .store
                .head(&Self::snapshot_path(ns, wl, id))
                .await
                .is_ok()
    }

    /// Construit et range un snapshot des échanges sortants du workload.
    async fn build_snapshot(&self, e: &Entry) -> Result<Snapshot, String> {
        self.load(e).await;
        // Échanges en mémoire et exemples : un endpoint ancien reste mockable (§5.5.3).
        let view = e.data().view(Extra::All);
        let opts = SnapshotOptions {
            max_per_key: self.config.snapshot.max_per_key,
        };
        let snap = tokio::task::spawn_blocking(move || build(view.iter().map(|t| t.1), &opts))
            .await
            .map_err(|err| format!("construction du snapshot: {err}"))?;
        let path = Self::snapshot_path(&e.namespace, &e.name, &snap.id);
        if self.store.head(&path).await.is_err() {
            self.store
                .put(&path, PutPayload::from(snap.to_jsonl()))
                .await
                .map_err(|err| format!("écriture du snapshot {}: {err}", snap.id))?;
            self.metrics.snapshots.inc();
        }
        Ok(snap)
    }

    /// Applique la ressource `MimicWorkload` (ou son absence) au workload.
    pub async fn apply_spec(
        &self,
        ns: &str,
        wl: &str,
        spec: Option<MimicWorkloadSpec>,
        generation: Option<i64>,
    ) {
        let e = self.ready(ns, wl).await;
        let current = {
            let mut live = e.live();
            live.spec = spec.clone();
            live.generation = generation;
            live.desired.clone()
        };
        // Les snapshots candidats sont vérifiés avant la réconciliation, qui est pure.
        let mut known = BTreeSet::new();
        for id in [
            spec.as_ref().and_then(|s| s.snapshot.clone()),
            current.snapshot.clone(),
        ]
        .into_iter()
        .flatten()
        {
            if self.snapshot_exists(ns, wl, &id).await {
                known.insert(id);
            }
        }
        let production = self.is_production(ns);
        let mut outcome = reconcile_guarded(
            spec.as_ref(),
            &current,
            &|id| known.contains(id),
            production,
        );
        if outcome.action == Action::BuildSnapshot
            && let Some(spec) = &spec
        {
            match self.build_snapshot(&e).await {
                Ok(snap) => {
                    tracing::info!(namespace = ns, workload = wl, snapshot = %snap.id, exchanges = snap.exchanges.len(), "snapshot construit");
                    outcome.desired = desired_of(spec, Some(snap.id));
                }
                Err(err) => {
                    tracing::warn!(error = %err, "construction du snapshot échouée");
                    outcome.error = Some("SnapshotFailed".into());
                }
            }
        }
        let changed = {
            let mut live = e.live();
            live.error = outcome.error;
            let changed = outcome.desired != live.desired;
            if changed {
                live.desired = outcome.desired.clone();
            }
            changed
        };
        if changed {
            tracing::info!(namespace = ns, workload = wl, mode = ?outcome.desired.mode, snapshot = ?outcome.desired.snapshot, "état souhaité");
            let path = Path::from(format!("{}/desired.json", base(ns, wl)));
            let body = serde_json::to_vec(&outcome.desired).unwrap_or_default();
            if let Err(err) = self.store.put(&path, PutPayload::from(body)).await {
                tracing::warn!(error = %err, "écriture de l'état souhaité échouée");
            }
        }
    }

    /// Chaque pod, son état au regard de l'état souhaité qui lui revient, et s'il est
    /// sélectionné par `spec.selector`.
    pub fn pod_states(&self, live: &Live) -> Vec<(Pod, PodState, bool)> {
        let now = now_ms();
        let timeout = self.config.pod_timeout.as_millis() as u64;
        live.pods
            .values()
            .map(|p| {
                let want = desired_for_pod(&live.desired, &p.labels);
                let s = pod_state(&p.report, p.last_seen_ms, now, timeout, &want);
                (p.clone(), s, selects(&live.desired, &p.labels))
            })
            .collect()
    }

    /// Statut du workload : seuls les pods sélectionnés comptent (spec §5.5.1).
    pub fn summary(&self, live: &Live) -> Summary {
        let states: Vec<PodState> = self
            .pod_states(live)
            .into_iter()
            .filter(|(_, _, selected)| *selected)
            .map(|(_, s, _)| s)
            .collect();
        summarize(&states, live.error.as_deref())
    }

    /// Documents OpenAPI du workload : le dernier calcul, recalculé en tâche de fond au plus
    /// une fois par `discovery_interval` ; seule la première vue attend le calcul.
    pub async fn discovery(&self, ns: &str, wl: &str) -> Arc<Discovery> {
        let e = self.entry(ns, wl);
        self.load(&e).await;
        let version = e.data().version;
        let interval = self.config.discovery_interval.as_millis() as u64;
        let pending = {
            let mut cache = e.discovery();
            if let Some(doc) = cache.doc.clone() {
                let due = now_ms().saturating_sub(cache.at_ms) >= interval;
                if cache.version != version && due && cache.running.is_none() {
                    // Le calcul tourne dans sa propre tâche ; cette vue ne l'attend pas.
                    drop(e.start_discovery(&mut cache));
                }
                return doc;
            }
            match cache.running.clone() {
                Some(r) => r,
                None => e.start_discovery(&mut cache),
            }
        };
        match pending.await {
            Some(doc) => doc,
            None => Arc::new(Discovery {
                inbound: infer::<Exchange>(&[], &InferOptions::new(wl, env!("CARGO_PKG_VERSION"))),
                outbound: BTreeMap::new(),
            }),
        }
    }

    /// Applique `f` aux requêtes enregistrées, les plus récentes d'abord, avec leur instance.
    /// Le tri et `f` tournent hors des threads du serveur, sur les échanges en place.
    pub async fn with_requests<R, F>(
        &self,
        ns: &str,
        wl: &str,
        direction: Direction,
        limit: usize,
        f: F,
    ) -> Option<R>
    where
        R: Send + 'static,
        F: for<'a> FnOnce(Vec<Tagged<'a>>) -> R + Send + 'static,
    {
        let e = self.entry(ns, wl);
        self.load(&e).await;
        let view = e.data().view(Extra::Missing);
        tokio::task::spawn_blocking(move || {
            f(mimic_core::replay::recent_requests(
                view.iter(),
                direction,
                limit,
            ))
        })
        .await
        .ok()
    }

    /// Requêtes enregistrées, les plus récentes d'abord, avec leur instance.
    pub async fn requests(
        &self,
        ns: &str,
        wl: &str,
        direction: Direction,
        limit: usize,
    ) -> Vec<(String, Exchange)> {
        self.with_requests(ns, wl, direction, limit, |list| {
            list.into_iter()
                .map(|t| (t.0.to_string(), t.1.clone()))
                .collect()
        })
        .await
        .unwrap_or_default()
    }

    pub async fn refresh_metrics(&self) {
        let keys = self.workload_keys().await;
        self.metrics.workloads.set(keys.len() as i64);
        self.metrics
            .exchanges
            .set(self.total.load(Ordering::Relaxed) as i64);
        let (exemplars, bytes) = self
            .entries()
            .iter()
            .map(|e| {
                let d = e.data();
                (d.exemplars.len(), d.exemplars.bytes())
            })
            .fold((0, 0), |(n, b), (dn, db)| (n + dn, b + db));
        self.metrics.exemplars.set(exemplars as i64);
        self.metrics.exemplars_bytes.set(bytes as i64);
        let mut counts: BTreeMap<&'static str, i64> =
            [("aligned", 0), ("diverging", 0), ("absent", 0)].into();
        for (ns, wl) in keys {
            let states = self.with_live(&ns, &wl, |l| self.pod_states(l)).await;
            for (_, s, _) in states {
                *counts.entry(s.as_str()).or_default() += 1;
            }
        }
        for (state, n) in counts {
            self.metrics
                .pods
                .get_or_create(&StateLabel {
                    state: state.into(),
                })
                .set(n);
        }
    }

    /// Passe de maintenance (toutes les minutes) : pods oubliés, puis pour chaque workload
    /// exemples écrits s'ils ont changé, rétention, compaction des heures révolues et plafond
    /// de volume du support. Aucun segment n'est supprimé avant que ses échanges aient été
    /// proposés aux exemples et que ceux-ci soient écrits (spec §5.5.3).
    pub async fn maintain(&self) {
        let now = now_ms();
        self.workload_keys().await;
        for e in self.entries() {
            self.forget_pods(&mut e.live(), now);
            if let Err(err) = self.index_of(&e).await {
                tracing::warn!(namespace = %e.namespace, workload = %e.name, error = %err, "index des segments");
                continue;
            }
            if let Err(err) = self.flush_exemplars(&e).await {
                tracing::warn!(namespace = %e.namespace, workload = %e.name, error = %err, "écriture des exemples : aucun segment supprimé");
                continue;
            }
            let expired: Vec<String> = e
                .index()
                .older_than(self.oldest_ms())
                .into_iter()
                .map(|(_, k)| k)
                .collect();
            if !expired.is_empty() && self.prepare_removal(&e, &expired).await {
                for key in expired {
                    self.remove_segment(&e, &key).await;
                }
            }
            self.compact(&e, now as i64).await;
            loop {
                let oldest = {
                    let index = e.index();
                    if index.bytes <= self.config.max_bytes_per_workload {
                        break;
                    }
                    index.oldest()
                };
                let Some(key) = oldest else {
                    break;
                };
                if !self.prepare_removal(&e, std::slice::from_ref(&key)).await
                    || !self.remove_segment(&e, &key).await
                {
                    break;
                }
            }
        }
    }

    /// Écrit les exemples de chaque workload qui ont changé (arrêt du contrôleur).
    pub async fn flush_all(&self) {
        for e in self.entries() {
            if e.exemplars_loaded.initialized()
                && let Err(err) = self.flush_exemplars(&e).await
            {
                tracing::warn!(namespace = %e.namespace, workload = %e.name, error = %err, "écriture des exemples");
            }
        }
    }

    /// Relit une fois `exemplars.jsonl`, avant tout nouvel exemple : ceux d'avant le
    /// redémarrage gardent leur place sous la borne.
    async fn exemplars_of(&self, e: &Entry) -> Result<(), String> {
        e.exemplars_loaded
            .get_or_try_init(|| async {
                let path = e.exemplars_path();
                let bytes = match self.store.get(&path).await {
                    Ok(r) => r
                        .bytes()
                        .await
                        .map_err(|err| format!("lecture de {path}: {err}"))?,
                    Err(object_store::Error::NotFound { .. }) => return Ok(()),
                    Err(err) => return Err(format!("lecture de {path}: {err}")),
                };
                let list = tokio::task::spawn_blocking(move || {
                    Exemplars::parse(&bytes)
                        .into_iter()
                        .map(Arc::new)
                        .collect::<Vec<_>>()
                })
                .await
                .map_err(|err| format!("lecture de {path}: {err}"))?;
                let refused = {
                    let mut d = e.data();
                    let refused = d.restore(list, &self.exemplar_room);
                    // Relus du support : rien à réécrire.
                    d.flushed = d.exemplars.version();
                    refused
                };
                self.metrics.refuse_exemplars(refused);
                Ok::<(), String>(())
            })
            .await
            .map(|_| ())
    }

    /// Écrit `exemplars.jsonl` s'il a changé depuis la dernière écriture.
    async fn flush_exemplars(&self, e: &Entry) -> Result<(), String> {
        self.exemplars_of(e).await?;
        let _one = e.flush.lock().await;
        let (version, list) = {
            let d = e.data();
            if d.exemplars.version() == d.flushed {
                return Ok(());
            }
            let list: Vec<Arc<Exchange>> = d.exemplars.iter().cloned().collect();
            (d.exemplars.version(), list)
        };
        let path = e.exemplars_path();
        let body = tokio::task::spawn_blocking(move || jsonl(&list))
            .await
            .map_err(|err| format!("écriture de {path}: {err}"))?;
        self.store
            .put(&path, PutPayload::from(body))
            .await
            .map_err(|err| format!("écriture de {path}: {err}"))?;
        let mut d = e.data();
        d.flushed = d.flushed.max(version);
        Ok(())
    }

    /// Avant de supprimer `keys` : propose aux exemples les échanges des segments qui ne
    /// l'ont pas encore été (rangés avant le démarrage), puis écrit les exemples. Faux si un
    /// segment n'a pas pu être relu ou si l'écriture a échoué : rien ne doit alors être
    /// supprimé.
    async fn prepare_removal(&self, e: &Entry, keys: &[String]) -> bool {
        if let Err(err) = self.exemplars_of(e).await {
            tracing::warn!(namespace = %e.namespace, workload = %e.name, error = %err, "relecture des exemples");
            return false;
        }
        let unseen: Vec<String> = {
            let d = e.data();
            keys.iter()
                .filter(|k| !d.offered.contains(*k))
                .cloned()
                .collect()
        };
        for key in unseen {
            let bytes = match self.store.get(&e.segment_path(&key)).await {
                Ok(r) => r.bytes().await,
                Err(err) => Err(err),
            };
            let bytes = match bytes {
                Ok(b) => b,
                // Déjà supprimé : plus rien à proposer.
                Err(object_store::Error::NotFound { .. }) => continue,
                Err(err) => {
                    tracing::warn!(segment = %key, error = %err, "relecture avant suppression");
                    return false;
                }
            };
            let best = tokio::task::spawn_blocking(move || winners(&parse_lines(&bytes)))
                .await
                .unwrap_or_default();
            let refused = e.data().offer(Some(&key), best, &self.exemplar_room);
            self.metrics.refuse_exemplars(refused);
        }
        match self.flush_exemplars(e).await {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(namespace = %e.namespace, workload = %e.name, error = %err, "écriture des exemples : segments gardés");
                false
            }
        }
    }

    /// Supprime un segment du support puis de l'index ; faux si le support a refusé.
    async fn remove_segment(&self, e: &Entry, key: &str) -> bool {
        match self.store.delete(&e.segment_path(key)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => {
                e.index().remove(key);
                e.data().offered.remove(key);
                true
            }
            Err(err) => {
                tracing::warn!(segment = key, error = %err, "suppression d'un segment");
                false
            }
        }
    }

    /// Fusionne les segments bruts de chaque heure révolue depuis plus de `COMPACT_GRACE_MS`
    /// en un segment horaire. Celui-ci est écrit avant la suppression des segments bruts : un
    /// arrêt entre les deux ne perd rien, la passe suivante refait la fusion. Les échanges
    /// relus sont proposés aux exemples avant que la fusion ne retire des bodies, et les
    /// exemples sont écrits avant la suppression.
    async fn compact(&self, e: &Entry, now: i64) {
        let mut hours: BTreeMap<i64, (String, Vec<String>)> = BTreeMap::new();
        for (at, key) in e.index().older_than(now - COMPACT_GRACE_MS) {
            if key.starts_with(&format!("{COMPACTED}/")) {
                continue;
            }
            let (name, start, end) = hour_of(at);
            if end > now - COMPACT_GRACE_MS {
                continue;
            }
            hours
                .entry(start)
                .or_insert_with(|| (name, vec![]))
                .1
                .push(key);
        }
        for (start, (name, raw)) in hours {
            let target = format!("{COMPACTED}/{name}.jsonl");
            let mut inputs = raw.clone();
            if e.index().contains(&target) {
                inputs.insert(0, target.clone());
            }
            let mut compactor = Compactor::new(self.config.samples_per_operation);
            let mut complete = true;
            for key in &inputs {
                let bytes = match self.store.get(&e.segment_path(key)).await {
                    Ok(r) => r.bytes().await,
                    Err(err) => Err(err),
                };
                match bytes {
                    Ok(bytes) => {
                        let unseen = !e.data().offered.contains(key);
                        // Analyse et sérialisation hors des threads du serveur.
                        let mut c = compactor;
                        match tokio::task::spawn_blocking(move || {
                            c.push(&bytes);
                            let best = if unseen {
                                winners(&parse_lines(&bytes))
                            } else {
                                vec![]
                            };
                            (c, best)
                        })
                        .await
                        {
                            Ok((c, best)) => {
                                compactor = c;
                                let refused = e.data().offer(Some(key), best, &self.exemplar_room);
                                self.metrics.refuse_exemplars(refused);
                            }
                            Err(_) => {
                                compactor = Compactor::new(0);
                                complete = false;
                                break;
                            }
                        }
                    }
                    Err(object_store::Error::NotFound { .. }) => {}
                    Err(err) => {
                        tracing::warn!(segment = %key, error = %err, "compaction : lecture");
                        complete = false;
                        break;
                    }
                }
            }
            if !complete {
                continue;
            }
            let merged = compactor.finish();
            let size = merged.len() as u64;
            if let Err(err) = self
                .store
                .put(&e.segment_path(&target), PutPayload::from(merged))
                .await
            {
                tracing::warn!(segment = %target, error = %err, "compaction : écriture");
                continue;
            }
            e.index()
                .insert(target.clone(), start + 3_600_000 - 1, size);
            // Ses échanges sont ceux des segments relus, déjà proposés.
            e.data().offered.insert(target);
            if let Err(err) = self.flush_exemplars(e).await {
                tracing::warn!(namespace = %e.namespace, workload = %e.name, error = %err, "compaction : exemples non écrits, segments bruts gardés");
                continue;
            }
            for key in raw {
                self.remove_segment(e, &key).await;
            }
        }
    }
}
