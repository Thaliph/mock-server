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
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::future::{BoxFuture, FutureExt, Shared};
use mimic_core::compact::{BodySamples, Compactor, hour_of, hour_start};
use mimic_core::exemplar::{Exemplars, best_per_endpoint, is_exemplar, jsonl};
use mimic_core::infer::{InferOptions, infer, infer_outbound};
use mimic_core::model::{Direction, Exchange};
use mimic_core::snapshot::{
    SNAPSHOT_META_FORMAT, Snapshot, SnapshotEntry, SnapshotMeta, SnapshotOptions, build, to_purge,
};
use mimic_core::unmatched::{MAX_MERGED, UnmatchedReport};
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::OnceCell;

use crate::auth::Authenticator;
use crate::config::ControllerConfig;
use crate::metrics::Metrics;
use crate::mock_form::{EncodedMock, MAX_CACHE_BYTES, MockForms};
use crate::reconcile::{
    Action, CONTROL_NAMESPACE, Desired, EMERGENCY_REASON, MimicWorkloadSpec, PodReport, PodState,
    Summary, desired_for_pod, desired_of, emergency_desired, guard_pod, is_production, is_stopped,
    pod_state, reconcile_guarded, selects, summarize,
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
    /// Namespaces en arrêt d'urgence ([`CONTROL_NAMESPACE`] : tous).
    stopped: Mutex<BTreeSet<String>>,
    /// Formes de mock des snapshots déjà demandées (spec §5.6).
    mock_forms: MockForms,
    /// Réplique active (spec §5.5.4) : seule à servir les sidecars et à toucher au support.
    active: AtomicBool,
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
    /// Échanges du snapshot servi (`desired.snapshot`), résolu hors du verrou court lors de
    /// la réconciliation ; `None` sans snapshot ou s'il n'a pas pu être lu (CTL-45,
    /// `status.snapshotExchanges`).
    pub snapshot_exchanges: Option<u64>,
    /// Ressource `MimicWorkload` courante, si elle existe.
    pub spec: Option<MimicWorkloadSpec>,
    pub generation: Option<i64>,
    pub error: Option<String>,
    /// Arrêt d'urgence en cours pour ce namespace, ou pour tous (spec §5.5.1).
    pub stopped: bool,
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
    ///
    /// Les exemples complets d'abord : leur taille tenait déjà sous la borne. Un exemple sans
    /// body, toujours gardé et qui la dépasse souvent, ne doit pas leur prendre la place à cause
    /// de l'ordre des endpoints dans le fichier.
    fn restore(&mut self, mut list: Vec<Arc<Exchange>>, room: &Room) -> Refused {
        list.sort_by_key(|e| !is_exemplar(e));
        self.propose(list, room, true)
    }

    fn propose(&mut self, list: Vec<Arc<Exchange>>, room: &Room, restore: bool) -> Refused {
        let x = &mut self.exemplars;
        let (version, refused, refused_bytes) = (x.version(), x.refused(), x.refused_bytes());
        let mut used = room.used.lock().unwrap_or_else(PoisonError::into_inner);
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
    /// État souhaité relu du support au premier accès ; une erreur du support n'initialise
    /// rien et la lecture est retentée à l'accès suivant.
    desired_loaded: OnceCell<()>,
    /// Ressource reçue pendant que l'état souhaité était illisible : réappliquée à la
    /// prochaine passe de maintenance (spec §9.2b).
    reapply: AtomicBool,
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
    /// Sel de redaction du workload, tiré une fois et gardé sur le support (spec §7).
    salt: OnceCell<String>,
}

impl Entry {
    fn live(&self) -> MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn index(&self) -> MutexGuard<'_, SegmentIndex> {
        self.index.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn data(&self) -> MutexGuard<'_, Data> {
        self.data.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn discovery(&self) -> MutexGuard<'_, DiscoveryCache> {
        self.discovery
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn base(&self) -> String {
        base(&self.namespace, &self.name)
    }

    fn salt_path(&self) -> Path {
        Path::from(format!("{}/redact-salt", self.base()))
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
            stopped: Mutex::new(BTreeSet::new()),
            mock_forms: MockForms::new(MAX_CACHE_BYTES),
            active: AtomicBool::new(false),
        }
    }

    /// Vrai si cette réplique est active : elle sert les sidecars et l'interface.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// Sert ou cesse de servir. Cesser remet aussi la jauge `mimic_controller_leader` à 0 ;
    /// servir ne la passe pas à 1, ce que fait `set_leader` une fois la marque posée (CTL-52).
    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::SeqCst);
        if !active {
            self.metrics.leader.set(0);
        }
    }

    /// Jauge `mimic_controller_leader` à 1 : la réplique sert et le Service la route (CTL-52).
    pub fn set_leader(&self) {
        self.metrics.leader.set(1);
    }

    /// Prise de relais (spec §5.5.4) : recharge ce qui sert les sidecars avant de les servir,
    /// c'est-à-dire la liste des workloads, l'état souhaité de chacun, l'index de ses segments
    /// et ses exemples. Les échanges restent relus à la première vue d'un workload.
    pub async fn warm_up(&self) {
        let keys = self.workload_keys().await;
        for (ns, wl) in &keys {
            // Erreur journalisée par `ready` ; l'état souhaité sera relu au premier appel.
            let Ok(e) = self.ready(ns, wl).await else {
                continue;
            };
            if let Err(err) = self.index_of(&e).await {
                tracing::warn!(namespace = %ns, workload = %wl, error = %err, "index des segments");
            }
            if let Err(err) = self.exemplars_of(&e).await {
                tracing::warn!(namespace = %ns, workload = %wl, error = %err, "relecture des exemples");
            }
        }
        self.refresh_metrics().await;
        tracing::info!(workloads = keys.len(), "état persisté rechargé");
    }

    /// Vrai si le namespace est labellisé `env=production`.
    pub fn is_production(&self, ns: &str) -> bool {
        self.production
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(ns)
    }

    /// Vrai si le namespace est en arrêt d'urgence, lui ou tous (spec §5.5.1).
    pub fn is_stopped(&self, ns: &str) -> bool {
        let set = self.stopped.lock().unwrap_or_else(PoisonError::into_inner);
        set.contains(ns) || set.contains(CONTROL_NAMESPACE)
    }

    /// Labels courants d'un namespace (création, modification) : arrêt d'urgence, appliqué
    /// au battement suivant, puis garde-fou production, qui réconcilie aussitôt les
    /// ressources du namespace.
    pub async fn set_namespace_labels(&self, ns: &str, labels: &BTreeMap<String, String>) {
        let stop = is_stopped(labels);
        let stop_changed = {
            let mut set = self.stopped.lock().unwrap_or_else(PoisonError::into_inner);
            if stop {
                set.insert(ns.to_string())
            } else {
                set.remove(ns)
            }
        };
        if stop_changed {
            tracing::warn!(namespace = ns, stop, "arrêt d'urgence");
            for e in self.entries() {
                if ns == CONTROL_NAMESPACE || e.namespace == ns {
                    let stopped = self.is_stopped(&e.namespace);
                    e.live().stopped = stopped;
                }
            }
        }
        let production = is_production(labels);
        let changed = {
            let mut set = self
                .production
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
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
        let mut map = self
            .workloads
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        map.entry((ns.to_string(), wl.to_string()))
            .or_insert_with(|| {
                Arc::new(Entry {
                    namespace: ns.into(),
                    name: wl.into(),
                    desired_loaded: OnceCell::new(),
                    reapply: AtomicBool::new(false),
                    live: Mutex::new(Live {
                        stopped: self.is_stopped(ns),
                        ..Live::default()
                    }),
                    indexed: OnceCell::new(),
                    index: Mutex::new(SegmentIndex::default()),
                    loaded: OnceCell::new(),
                    load_started: AtomicBool::new(false),
                    exemplars_loaded: OnceCell::new(),
                    flush: tokio::sync::Mutex::new(()),
                    salt: OnceCell::new(),
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
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// Plus ancienne date encore dans la rétention.
    fn oldest_ms(&self) -> i64 {
        now_ms().saturating_sub(self.config.retention.as_millis() as u64) as i64
    }

    /// Workload dont l'état souhaité a été relu du support. Absent ou illisible : état
    /// vide. Erreur du support : rien n'est retenu, l'appel suivant relit (spec §9.2b).
    async fn ready(&self, ns: &str, wl: &str) -> Result<Arc<Entry>, String> {
        let e = self.entry(ns, wl);
        e.desired_loaded
            .get_or_try_init(|| async {
                let path = Path::from(format!("{}/desired.json", base(ns, wl)));
                let bytes = match self.store.get(&path).await {
                    Ok(r) => r.bytes().await,
                    Err(err) => Err(err),
                };
                match bytes {
                    Ok(bytes) => match serde_json::from_slice::<Desired>(&bytes) {
                        Ok(d) => e.live().desired = d,
                        // Le relire ne le réparerait pas : état vide, signalé.
                        Err(err) => {
                            tracing::error!(namespace = ns, workload = wl, error = %err, "desired.json illisible : état souhaité vide");
                        }
                    },
                    Err(object_store::Error::NotFound { .. }) => {}
                    Err(err) => {
                        tracing::warn!(namespace = ns, workload = wl, error = %err, "lecture de l'état souhaité, retentée au prochain appel");
                        return Err(format!("lecture de {path}: {err}"));
                    }
                }
                Ok(())
            })
            .await?;
        Ok(e)
    }

    /// Lecture de l'état court d'un workload ; le verrou n'est tenu que pendant `f`.
    /// Erreur si l'état souhaité n'a pas pu être relu du support.
    pub async fn with_live<R>(
        &self,
        ns: &str,
        wl: &str,
        f: impl FnOnce(&Live) -> R,
    ) -> Result<R, String> {
        let e = self.ready(ns, wl).await?;
        let live = e.live();
        Ok(f(&live))
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
            .unwrap_or_else(PoisonError::into_inner)
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
    /// Sel de redaction du workload (spec §7) : relu du support, sinon tiré au hasard et
    /// écrit par une écriture conditionnelle, si bien que deux pods ou deux répliques
    /// simultanés obtiennent le même. Une erreur du support n'initialise rien.
    pub async fn workload_salt(&self, ns: &str, wl: &str) -> Result<String, String> {
        let e = self.entry(ns, wl);
        let path = e.salt_path();
        e.salt
            .get_or_try_init(|| async {
                let read = |bytes: Bytes| {
                    let salt = String::from_utf8_lossy(&bytes).trim().to_string();
                    if salt.is_empty() {
                        Err(format!("{path} vide"))
                    } else {
                        Ok(salt)
                    }
                };
                match self.store.get(&path).await {
                    Ok(r) => return read(r.bytes().await.map_err(|e| e.to_string())?),
                    Err(object_store::Error::NotFound { .. }) => {}
                    Err(err) => return Err(format!("lecture de {path}: {err}")),
                }
                let mut raw = [0u8; 32];
                getrandom::fill(&mut raw).map_err(|e| format!("tirage du sel: {e}"))?;
                let salt: String = raw.iter().map(|b| format!("{b:02x}")).collect();
                let create = PutOptions {
                    mode: PutMode::Create,
                    ..PutOptions::default()
                };
                match self
                    .store
                    .put_opts(&path, PutPayload::from(salt.clone().into_bytes()), create)
                    .await
                {
                    Ok(_) => Ok(salt),
                    // Écrit entre-temps par un autre : c'est le sien qui vaut.
                    Err(object_store::Error::AlreadyExists { .. }) => {
                        let r = self
                            .store
                            .get(&path)
                            .await
                            .map_err(|err| format!("lecture de {path}: {err}"))?;
                        read(r.bytes().await.map_err(|e| e.to_string())?)
                    }
                    Err(object_store::Error::NotImplemented { .. }) => {
                        self.store
                            .put(&path, PutPayload::from(salt.clone().into_bytes()))
                            .await
                            .map_err(|err| format!("écriture de {path}: {err}"))?;
                        Ok(salt)
                    }
                    Err(err) => Err(format!("écriture de {path}: {err}")),
                }
            })
            .await
            .cloned()
    }

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
            Ok::<_, String>(parsed)
        })
        .await
        .map_err(|e| IngestError::Invalid(e.to_string()))?
        .map_err(IngestError::Invalid)?;

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
        // Redaction de nouveau, avec le sel du workload (spec §7) : un sidecar d'une version
        // antérieure ou mal configuré ne fait rien écrire de sensible. Après le contrôle de
        // doublon, qui n'a pas besoin du support.
        let salt = self
            .workload_salt(ns, wl)
            .await
            .map_err(IngestError::Storage)?;
        let redactor = mimic_core::redact::Redactor::new(
            mimic_core::redact::RedactConfig::default(),
            salt.into_bytes(),
        );
        let (parsed, best, body) = tokio::task::spawn_blocking(move || {
            let mut parsed = parsed;
            let mut rewritten = false;
            for e in &mut parsed {
                let before = e.redactions.len();
                // Bodies non analysables : laissés tels que le sidecar les a choisis.
                redactor.redact_with(e, true);
                rewritten |= e.redactions.len() > before;
            }
            // Réécrit seulement si une valeur a été remplacée : sinon octet pour octet.
            let body = if rewritten {
                let mut out = Vec::with_capacity(body.len());
                for e in &parsed {
                    serde_json::to_writer(&mut out, e).map_err(|e| e.to_string())?;
                    out.push(b'\n');
                }
                Bytes::from(out)
            } else {
                body
            };
            let best = winners(&parsed);
            Ok::<_, String>((parsed, best, body))
        })
        .await
        .map_err(|e| IngestError::Invalid(e.to_string()))?
        .map_err(IngestError::Invalid)?;
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
    ) -> Result<Desired, String> {
        let e = self.ready(ns, wl).await?;
        let now = now_ms();
        let production = self.is_production(ns);
        let mut live = e.live();
        if !live.pods.contains_key(instance) {
            // Un nouveau pod remplace souvent un ancien (déploiement) : on oublie les disparus.
            self.forget_pods(&mut live, now);
        }
        // État propre au pod : arrêt d'urgence, sinon sélection (spec.selector), puis
        // garde-fou production.
        let desired = if live.stopped {
            emergency_desired()
        } else {
            guard_pod(
                desired_for_pod(&live.desired, &labels),
                production,
                &report.mode,
            )
        };
        live.pods.insert(
            instance.into(),
            Pod {
                instance: instance.into(),
                report,
                last_seen_ms: now,
                labels,
            },
        );
        Ok(desired)
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

    /// Forme de mock du snapshot (spec §5.6), calculée à la première demande hors des
    /// threads du serveur, puis gardée ; `None` si le snapshot est inconnu ou illisible.
    pub async fn mock_snapshot(&self, ns: &str, wl: &str, id: &str) -> Option<Arc<EncodedMock>> {
        if !valid_name(id) {
            return None;
        }
        self.mock_forms
            .get(&format!("{}/{id}", base(ns, wl)), || async {
                let full = self.snapshot_bytes(ns, wl, id).await?;
                let (owned, full_bytes) = (id.to_string(), full.len());
                match tokio::task::spawn_blocking(move || EncodedMock::from_full(&full, &owned))
                    .await
                {
                    Ok(Ok(form)) => {
                        tracing::info!(
                            namespace = ns,
                            workload = wl,
                            snapshot = id,
                            full = full_bytes,
                            identity = form.identity_len,
                            zstd = form.zstd.len(),
                            gzip = form.gzip.len(),
                            "forme de mock calculée"
                        );
                        Some(form)
                    }
                    Ok(Err(err)) => {
                        tracing::warn!(error = %err, snapshot = id, "forme de mock impossible");
                        None
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, snapshot = id, "forme de mock impossible");
                        None
                    }
                }
            })
            .await
    }

    /// Snapshots du workload : identifiant, taille, date, échanges et `pinned` quand
    /// connus (métadonnées, CTL-43), du plus récent au plus ancien (`mimicctl snapshot ls`,
    /// CLI-12).
    pub async fn snapshots(&self, ns: &str, wl: &str) -> Vec<Value> {
        let prefix = Path::from(format!("{}/snapshots", base(ns, wl)));
        let mut out = vec![];
        let mut listing = self.store.list(Some(&prefix));
        while let Some(Ok(entry)) = listing.next().await {
            let Some(id) = entry
                .location
                .filename()
                .and_then(|f| f.strip_suffix(".jsonl"))
            else {
                continue;
            };
            let meta = self.snapshot_meta(ns, wl, id).await;
            out.push(serde_json::json!({
                "id": id,
                "size": entry.size,
                "modified_ms": entry.last_modified.timestamp_millis(),
                "created_ms": meta.map(|m| m.created_ms),
                "exchanges": meta.map(|m| m.exchanges),
                "pinned": meta.map(|m| m.pinned).unwrap_or(false),
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
        self.write_snapshot(&e.namespace, &e.name, &snap, false)
            .await?;
        Ok(snap)
    }

    fn snapshot_meta_path(ns: &str, wl: &str, id: &str) -> Path {
        Path::from(format!("{}/snapshots/{id}.meta.json", base(ns, wl)))
    }

    /// Métadonnées du snapshot (CTL-43, cycle de vie) ; `None` s'il n'y en a pas (snapshot
    /// gardé par une version antérieure, ou disparu).
    pub async fn snapshot_meta(&self, ns: &str, wl: &str, id: &str) -> Option<SnapshotMeta> {
        if !valid_name(id) {
            return None;
        }
        let bytes = self
            .store
            .get(&Self::snapshot_meta_path(ns, wl, id))
            .await
            .ok()?
            .bytes()
            .await
            .ok()?;
        SnapshotMeta::from_json(&bytes).ok()
    }

    /// Écrit le contenu et les métadonnées d'un snapshot, si son contenu n'existe pas déjà
    /// (immuable, content-addressed) : jamais de métadonnées réécrites sur un snapshot déjà
    /// connu, pour ne jamais effacer un `pinned` posé par `mimicctl snapshot pin` (CTL-43).
    async fn write_snapshot(
        &self,
        ns: &str,
        wl: &str,
        snap: &Snapshot,
        pinned: bool,
    ) -> Result<(), String> {
        let path = Self::snapshot_path(ns, wl, &snap.id);
        if self.store.head(&path).await.is_err() {
            self.store
                .put(&path, PutPayload::from(snap.to_jsonl()))
                .await
                .map_err(|err| format!("écriture du snapshot {}: {err}", snap.id))?;
            let meta = SnapshotMeta {
                format: SNAPSHOT_META_FORMAT,
                created_ms: now_ms() as i64,
                exchanges: snap.exchanges.len(),
                pinned,
            };
            self.store
                .put(
                    &Self::snapshot_meta_path(ns, wl, &snap.id),
                    PutPayload::from(meta.to_json()),
                )
                .await
                .map_err(|err| format!("métadonnées du snapshot {}: {err}", snap.id))?;
            self.metrics.snapshots.inc();
        }
        Ok(())
    }

    /// Importe un snapshot (`mimicctl snapshot import`, CTL-46) : contenu canonicalisé,
    /// épinglé par défaut pour survivre à la purge d'un contrôleur sans trafic qui le
    /// référence.
    pub async fn import_snapshot(
        &self,
        ns: &str,
        wl: &str,
        bytes: &[u8],
    ) -> Result<Snapshot, String> {
        let snap = Snapshot::parse(bytes)?;
        self.write_snapshot(ns, wl, &snap, true).await?;
        Ok(snap)
    }

    /// Change le drapeau `pinned` d'un snapshot déjà construit ou importé (CTL-46). Recrée
    /// des métadonnées par défaut si le snapshot n'en avait pas encore (version antérieure).
    pub async fn pin_snapshot(
        &self,
        ns: &str,
        wl: &str,
        id: &str,
        pinned: bool,
    ) -> Result<(), String> {
        if self.snapshot_bytes(ns, wl, id).await.is_none() {
            return Err("snapshot inconnu".into());
        }
        let existing = self.snapshot_meta(ns, wl, id).await;
        let meta = SnapshotMeta {
            format: SNAPSHOT_META_FORMAT,
            created_ms: match existing {
                Some(m) => m.created_ms,
                None => now_ms() as i64,
            },
            exchanges: match existing {
                Some(m) => m.exchanges,
                None => self.exchanges_count(ns, wl, id).await,
            },
            pinned,
        };
        self.store
            .put(
                &Self::snapshot_meta_path(ns, wl, id),
                PutPayload::from(meta.to_json()),
            )
            .await
            .map_err(|err| format!("métadonnées du snapshot {id}: {err}"))?;
        Ok(())
    }

    /// Échanges d'un snapshot sans métadonnées (version antérieure), en relisant son
    /// contenu : secours rare, seulement au premier accès d'épinglage ou de purge.
    async fn exchanges_count(&self, ns: &str, wl: &str, id: &str) -> usize {
        self.snapshot_bytes(ns, wl, id)
            .await
            .map(|b| b.split(|c| *c == b'\n').filter(|l| !l.is_empty()).count())
            .unwrap_or(0)
    }

    /// Référencés : jamais purgés, quel que soit leur âge (CTL-44).
    fn referenced_snapshots(live: &Live) -> BTreeSet<String> {
        [
            live.spec.as_ref().and_then(|s| s.snapshot.clone()),
            live.desired.snapshot.clone(),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// Purge les snapshots du workload au-delà de `snapshot.keep_last`, hors référencés et
    /// épinglés (CTL-44) ; contenu, métadonnées et forme de mock en cache supprimés
    /// ensemble. Retourne les identifiants supprimés.
    async fn purge_snapshots(&self, e: &Entry) -> BTreeSet<String> {
        let referenced = Self::referenced_snapshots(&e.live());
        let listing = self.snapshots(&e.namespace, &e.name).await;
        let mut candidates = Vec::with_capacity(listing.len());
        for v in &listing {
            let Some(id) = v["id"].as_str() else { continue };
            let (created_ms, pinned) = match self.snapshot_meta(&e.namespace, &e.name, id).await {
                Some(m) => (m.created_ms, m.pinned),
                // Snapshot d'avant cette fonctionnalité : métadonnées comblées une fois,
                // non épinglé, datées de la dernière écriture connue du support.
                None => {
                    let created_ms = v["modified_ms"].as_i64().unwrap_or(0);
                    let exchanges = self.exchanges_count(&e.namespace, &e.name, id).await;
                    let meta = SnapshotMeta {
                        format: SNAPSHOT_META_FORMAT,
                        created_ms,
                        exchanges,
                        pinned: false,
                    };
                    let _ = self
                        .store
                        .put(
                            &Self::snapshot_meta_path(&e.namespace, &e.name, id),
                            PutPayload::from(meta.to_json()),
                        )
                        .await;
                    (created_ms, false)
                }
            };
            candidates.push(SnapshotEntry {
                id: id.to_string(),
                created_ms,
                pinned,
            });
        }
        let purge = to_purge(&candidates, &referenced, self.config.snapshot.keep_last);
        for id in &purge {
            let _ = self
                .store
                .delete(&Self::snapshot_path(&e.namespace, &e.name, id))
                .await;
            let _ = self
                .store
                .delete(&Self::snapshot_meta_path(&e.namespace, &e.name, id))
                .await;
            self.mock_forms
                .evict(&format!("{}/{id}", base(&e.namespace, &e.name)));
            self.metrics.snapshots_deleted.inc();
            tracing::info!(namespace = %e.namespace, workload = %e.name, snapshot = %id, "snapshot purgé");
        }
        purge
    }

    /// Purge immédiate (`mimicctl snapshot gc`), hors de la passe de maintenance minute.
    pub async fn gc_snapshots(&self, ns: &str, wl: &str) -> BTreeSet<String> {
        self.purge_snapshots(&self.entry(ns, wl)).await
    }

    /// Applique la ressource `MimicWorkload` (ou son absence) au workload.
    pub async fn apply_spec(
        &self,
        ns: &str,
        wl: &str,
        spec: Option<MimicWorkloadSpec>,
        generation: Option<i64>,
    ) {
        let e = match self.ready(ns, wl).await {
            Ok(e) => {
                e.reapply.store(false, Ordering::SeqCst);
                e
            }
            Err(err) => {
                // Gardée pour la passe de maintenance, qui la réappliquera.
                tracing::warn!(namespace = ns, workload = wl, error = %err, "ressource réappliquée à la prochaine maintenance");
                let e = self.entry(ns, wl);
                let mut live = e.live();
                live.spec = spec;
                live.generation = generation;
                e.reapply.store(true, Ordering::SeqCst);
                return;
            }
        };
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
        // Échanges du snapshot construit ici, s'il y en a un : connus sans relecture.
        let mut built_exchanges = None;
        if outcome.action == Action::BuildSnapshot
            && let Some(spec) = &spec
        {
            match self.build_snapshot(&e).await {
                Ok(snap) => {
                    tracing::info!(namespace = ns, workload = wl, snapshot = %snap.id, exchanges = snap.exchanges.len(), "snapshot construit");
                    built_exchanges = Some(snap.exchanges.len() as u64);
                    outcome.desired = desired_of(spec, Some(snap.id));
                }
                Err(err) => {
                    tracing::warn!(error = %err, "construction du snapshot échouée");
                    outcome.error = Some("SnapshotFailed".into());
                }
            }
        }
        // Résolu avant le verrou court (CTL-45) : une relecture des métadonnées, voire du
        // contenu, est une I/O qui ne doit jamais s'y trouver.
        let snapshot_exchanges = match &outcome.desired.snapshot {
            Some(_) if built_exchanges.is_some() => built_exchanges,
            Some(id) => match self.snapshot_meta(ns, wl, id).await {
                Some(meta) => Some(meta.exchanges as u64),
                None => self
                    .snapshot_bytes(ns, wl, id)
                    .await
                    .map(|b| b.split(|c| *c == b'\n').filter(|l| !l.is_empty()).count() as u64),
            },
            None => None,
        };
        let changed = {
            let mut live = e.live();
            live.error = outcome.error;
            let changed = outcome.desired != live.desired;
            if changed {
                live.desired = outcome.desired.clone();
            }
            live.snapshot_exchanges = snapshot_exchanges;
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
                let want = if live.stopped {
                    emergency_desired()
                } else {
                    desired_for_pod(&live.desired, &p.labels)
                };
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
        let error = if live.stopped {
            Some(EMERGENCY_REASON)
        } else {
            live.error.as_deref()
        };
        let mut summary = summarize(&states, error);
        summary.unmatched = self.unmatched(live).total;
        summary
    }

    /// Relevé des requêtes sans réponse du workload (spec §5.3, §9.2b) : celui des pods
    /// sélectionnés encore connus, fusionné et borné.
    pub fn unmatched(&self, live: &Live) -> UnmatchedReport {
        UnmatchedReport::merge(
            live.pods
                .values()
                .filter(|p| selects(&live.desired, &p.labels))
                .filter_map(|p| p.report.unmatched.as_ref()),
            MAX_MERGED,
        )
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
        let mut counts: BTreeMap<String, [i64; 3]> = BTreeMap::new();
        for (ns, wl) in keys {
            let slot = counts.entry(format!("{ns}/{wl}")).or_default();
            let Ok(states) = self.with_live(&ns, &wl, |l| self.pod_states(l)).await else {
                continue;
            };
            for (_, s, _) in states {
                // Dans l'ordre de `Metrics::set_pods`.
                let i = match s {
                    PodState::Aligned => 0,
                    PodState::Diverging => 1,
                    PodState::Absent => 2,
                };
                slot[i] += 1;
            }
        }
        self.metrics.set_pods(&counts);
    }

    /// Passe de maintenance (toutes les minutes) : pods oubliés, puis pour chaque workload
    /// exemples écrits s'ils ont changé, rétention, compaction des heures révolues, plafond
    /// de volume du support et purge des snapshots au-delà de `snapshot.keep_last` (CTL-44).
    /// Aucun segment n'est supprimé avant que ses échanges aient été proposés aux exemples et
    /// que ceux-ci soient écrits (spec §5.5.3).
    pub async fn maintain(&self) {
        let now = now_ms();
        self.workload_keys().await;
        for e in self.entries() {
            if e.reapply.load(Ordering::SeqCst) {
                let (spec, generation) = {
                    let live = e.live();
                    (live.spec.clone(), live.generation)
                };
                self.apply_spec(&e.namespace, &e.name, spec, generation)
                    .await;
            }
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
            self.purge_snapshots(&e).await;
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
