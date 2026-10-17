//! État du contrôleur : enregistrements par workload, état souhaité, pods, snapshots.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use mimic_core::infer::{InferOptions, infer, infer_outbound};
use mimic_core::model::{Direction, Exchange};
use mimic_core::snapshot::{Snapshot, SnapshotOptions, build};
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use serde::Serialize;
use serde_json::Value;

use crate::auth::Authenticator;
use crate::config::ControllerConfig;
use crate::metrics::{Metrics, StateLabel};
use crate::reconcile::{
    Action, Desired, MimicWorkloadSpec, PodReport, PodState, Summary, pod_state, reconcile,
    summarize,
};

pub type Key = (String, String);

pub struct Ctl {
    pub config: ControllerConfig,
    pub store: Arc<dyn ObjectStore>,
    pub auth: Authenticator,
    pub metrics: Metrics,
    workloads: Mutex<BTreeMap<Key, Arc<tokio::sync::Mutex<Workload>>>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Pod {
    pub instance: String,
    pub report: PodReport,
    pub last_seen_ms: u64,
}

/// Documents OpenAPI d'un workload.
pub struct Discovery {
    pub inbound: Value,
    pub outbound: BTreeMap<String, Value>,
}

pub struct Workload {
    pub namespace: String,
    pub name: String,
    loaded: bool,
    /// Échanges importés, avec l'instance qui les a écrits, du plus ancien au plus récent.
    exchanges: VecDeque<(String, Exchange)>,
    ids: BTreeSet<String>,
    read: BTreeSet<String>,
    version: u64,
    discovery: Option<(u64, Arc<Discovery>)>,
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

fn base(ns: &str, wl: &str) -> String {
    format!("{ns}/{wl}")
}

impl Ctl {
    pub fn new(config: ControllerConfig, store: Arc<dyn ObjectStore>, auth: Authenticator) -> Self {
        Self {
            config,
            store,
            auth,
            metrics: Metrics::default(),
            workloads: Mutex::new(BTreeMap::new()),
        }
    }

    fn entry(&self, ns: &str, wl: &str) -> Arc<tokio::sync::Mutex<Workload>> {
        let mut map = self.workloads.lock().expect("verrou des workloads");
        map.entry((ns.to_string(), wl.to_string()))
            .or_insert_with(|| {
                Arc::new(tokio::sync::Mutex::new(Workload {
                    namespace: ns.into(),
                    name: wl.into(),
                    loaded: false,
                    exchanges: VecDeque::new(),
                    ids: BTreeSet::new(),
                    read: BTreeSet::new(),
                    version: 0,
                    discovery: None,
                    pods: BTreeMap::new(),
                    desired: Desired::default(),
                    spec: None,
                    generation: None,
                    error: None,
                }))
            })
            .clone()
    }

    /// Workload verrouillé, avec son état souhaité relu du support au premier accès.
    pub async fn workload(&self, ns: &str, wl: &str) -> tokio::sync::OwnedMutexGuard<Workload> {
        let mut w = self.entry(ns, wl).lock_owned().await;
        if !w.loaded {
            let path = Path::from(format!("{}/desired.json", base(ns, wl)));
            if let Ok(r) = self.store.get(&path).await
                && let Ok(bytes) = r.bytes().await
                && let Ok(d) = serde_json::from_slice::<Desired>(&bytes)
            {
                w.desired = d;
            }
            w.loaded = true;
        }
        w
    }

    /// Workloads connus : présents sur le support ou annoncés par un battement.
    pub async fn workload_keys(&self) -> Vec<Key> {
        let mut keys: BTreeSet<Key> = self
            .workloads
            .lock()
            .expect("verrou des workloads")
            .keys()
            .cloned()
            .collect();
        if let Ok(namespaces) = self.store.list_with_delimiter(None).await {
            for ns in namespaces.common_prefixes {
                if let Ok(wls) = self.store.list_with_delimiter(Some(&ns)).await {
                    for wl in wls.common_prefixes {
                        let parts: Vec<&str> = wl.as_ref().split('/').collect();
                        if let [n, w] = parts.as_slice() {
                            keys.insert((n.to_string(), w.to_string()));
                        }
                    }
                }
            }
        }
        keys.into_iter().collect()
    }

    pub async fn ingest(
        &self,
        ns: &str,
        wl: &str,
        instance: &str,
        body: &[u8],
    ) -> Result<String, String> {
        let mut parsed = vec![];
        for (i, line) in body.split(|b| *b == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let e: Exchange =
                serde_json::from_slice(line).map_err(|e| format!("ligne {}: {e}", i + 1))?;
            parsed.push(e);
        }
        let rel = format!("{instance}/{}.jsonl", ulid::Ulid::generate());
        let path = Path::from(format!("{}/exchanges/{rel}", base(ns, wl)));
        self.store
            .put(&path, PutPayload::from(body.to_vec()))
            .await
            .map_err(|e| format!("écriture de {path}: {e}"))?;
        let mut w = self.workload(ns, wl).await;
        w.read.insert(path.to_string());
        for e in parsed {
            w.add(instance, e, self.config.max_exchanges_per_workload);
        }
        Ok(rel)
    }

    /// Importe les segments que ce contrôleur n'a pas encore lus (autre réplica, redémarrage).
    pub async fn refresh(&self, w: &mut Workload) {
        let prefix = Path::from(format!("{}/exchanges", base(&w.namespace, &w.name)));
        let oldest = now_ms().saturating_sub(self.config.retention.as_millis() as u64) as i64;
        let mut todo = vec![];
        let mut listing = self.store.list(Some(&prefix));
        while let Some(Ok(meta)) = listing.next().await {
            let key = meta.location.to_string();
            if w.read.contains(&key) || meta.last_modified.timestamp_millis() < oldest {
                continue;
            }
            todo.push(meta.location);
        }
        // Ordre des clés : instance puis ULID, donc chronologique par instance.
        todo.sort_by(|a, b| a.as_ref().cmp(b.as_ref()));
        for location in todo {
            let instance = location
                .as_ref()
                .strip_prefix(prefix.as_ref())
                .and_then(|r| r.trim_start_matches('/').split('/').next())
                .unwrap_or_default()
                .to_string();
            let Ok(r) = self.store.get(&location).await else {
                continue;
            };
            let Ok(bytes) = r.bytes().await else {
                continue;
            };
            for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
                if let Ok(e) = serde_json::from_slice::<Exchange>(line) {
                    w.add(&instance, e, self.config.max_exchanges_per_workload);
                }
            }
            w.read.insert(location.to_string());
        }
    }

    pub async fn segments(&self, ns: &str, wl: &str) -> Result<Vec<SegmentInfo>, String> {
        let prefix = Path::from(format!("{}/exchanges", base(ns, wl)));
        let mut out = vec![];
        let mut listing = self.store.list(Some(&prefix));
        while let Some(meta) = listing.next().await {
            let meta = meta.map_err(|e| e.to_string())?;
            let key = meta
                .location
                .as_ref()
                .strip_prefix(prefix.as_ref())
                .unwrap_or_default()
                .trim_start_matches('/')
                .to_string();
            let Some((instance, _)) = key.split_once('/') else {
                continue;
            };
            out.push(SegmentInfo {
                instance: instance.into(),
                key: key.clone(),
                modified_ms: meta.last_modified.timestamp_millis(),
                size: meta.size,
            });
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
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
    ) -> Desired {
        let mut w = self.workload(ns, wl).await;
        w.pods.insert(
            instance.into(),
            Pod {
                instance: instance.into(),
                report,
                last_seen_ms: now_ms(),
            },
        );
        w.desired.clone()
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
    pub async fn build_snapshot(&self, w: &mut Workload) -> Result<Snapshot, String> {
        self.refresh(w).await;
        let snap = build(
            w.exchanges.iter().map(|(_, e)| e.clone()),
            &SnapshotOptions {
                max_per_key: self.config.snapshot.max_per_key,
            },
        );
        let path = Self::snapshot_path(&w.namespace, &w.name, &snap.id);
        if self.store.head(&path).await.is_err() {
            self.store
                .put(&path, PutPayload::from(snap.to_jsonl()))
                .await
                .map_err(|e| format!("écriture du snapshot {}: {e}", snap.id))?;
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
        let mut w = self.workload(ns, wl).await;
        w.spec = spec.clone();
        w.generation = generation;
        // Les snapshots candidats sont vérifiés avant la réconciliation, qui est pure.
        let mut known = BTreeSet::new();
        for id in [
            spec.as_ref().and_then(|s| s.snapshot.clone()),
            w.desired.snapshot.clone(),
        ]
        .into_iter()
        .flatten()
        {
            if self.snapshot_exists(ns, wl, &id).await {
                known.insert(id);
            }
        }
        let mut outcome = reconcile(spec.as_ref(), &w.desired, &|id| known.contains(id));
        if outcome.action == Action::BuildSnapshot
            && let Some(spec) = &spec
        {
            match self.build_snapshot(&mut w).await {
                Ok(snap) => {
                    tracing::info!(namespace = ns, workload = wl, snapshot = %snap.id, exchanges = snap.exchanges.len(), "snapshot construit");
                    outcome.desired = Desired {
                        mode: Some(spec.mode.clone()),
                        snapshot: Some(snap.id),
                    };
                }
                Err(e) => {
                    tracing::warn!(error = %e, "construction du snapshot échouée");
                    outcome.error = Some("SnapshotFailed".into());
                }
            }
        }
        w.error = outcome.error;
        if outcome.desired != w.desired {
            tracing::info!(namespace = ns, workload = wl, mode = ?outcome.desired.mode, snapshot = ?outcome.desired.snapshot, "état souhaité");
            w.desired = outcome.desired;
            let path = Path::from(format!("{}/desired.json", base(ns, wl)));
            let body = serde_json::to_vec(&w.desired).unwrap_or_default();
            if let Err(e) = self.store.put(&path, PutPayload::from(body)).await {
                tracing::warn!(error = %e, "écriture de l'état souhaité échouée");
            }
        }
    }

    pub fn pod_states(&self, w: &Workload) -> Vec<(Pod, PodState)> {
        let now = now_ms();
        let timeout = self.config.pod_timeout.as_millis() as u64;
        w.pods
            .values()
            .map(|p| {
                let s = pod_state(&p.report, p.last_seen_ms, now, timeout, &w.desired);
                (p.clone(), s)
            })
            .collect()
    }

    pub fn summary(&self, w: &Workload) -> Summary {
        let states: Vec<PodState> = self.pod_states(w).into_iter().map(|(_, s)| s).collect();
        summarize(&states, w.error.as_deref())
    }

    pub async fn discovery(&self, w: &mut Workload) -> Arc<Discovery> {
        self.refresh(w).await;
        if let Some((v, d)) = &w.discovery
            && *v == w.version
        {
            return d.clone();
        }
        let exchanges: Vec<Exchange> = w.exchanges.iter().map(|(_, e)| e.clone()).collect();
        let version = env!("CARGO_PKG_VERSION");
        let d = Arc::new(Discovery {
            inbound: infer(&exchanges, &InferOptions::new(w.name.clone(), version)),
            outbound: infer_outbound(&exchanges, version),
        });
        w.discovery = Some((w.version, d.clone()));
        d
    }

    /// Requêtes enregistrées, les plus récentes d'abord, avec leur instance.
    pub async fn requests(
        &self,
        w: &mut Workload,
        direction: Direction,
        limit: usize,
    ) -> Vec<(String, Exchange)> {
        self.refresh(w).await;
        let by_id: BTreeMap<&str, &str> = w
            .exchanges
            .iter()
            .map(|(i, e)| (e.id.as_str(), i.as_str()))
            .collect();
        let list = mimic_core::replay::recent_requests(
            w.exchanges.iter().map(|(_, e)| e.clone()),
            direction,
            limit,
        );
        list.into_iter()
            .map(|e| {
                let instance = by_id
                    .get(e.id.as_str())
                    .copied()
                    .unwrap_or_default()
                    .to_string();
                (instance, e)
            })
            .collect()
    }

    pub async fn refresh_metrics(&self) {
        let keys = self.workload_keys().await;
        self.metrics.workloads.set(keys.len() as i64);
        let mut counts: BTreeMap<&'static str, i64> =
            [("aligned", 0), ("diverging", 0), ("absent", 0)].into();
        for (ns, wl) in keys {
            let w = self.workload(&ns, &wl).await;
            for (_, s) in self.pod_states(&w) {
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
}

impl Workload {
    fn add(&mut self, instance: &str, e: Exchange, max: usize) {
        if !self.ids.insert(e.id.clone()) {
            return;
        }
        self.exchanges.push_back((instance.to_string(), e));
        while self.exchanges.len() > max {
            if let Some((_, old)) = self.exchanges.pop_front() {
                self.ids.remove(&old.id);
            }
        }
        self.version += 1;
    }

    pub fn exchange_count(&self) -> usize {
        self.exchanges.len()
    }
}
