//! The hub of the mesh view (spec §7.2, §7.9): the objects the watches deliver, the snapshot
//! published after a debounce, the views built from it, cached and long-polled.
//!
//! The view itself is `mimic_core::mesh::view::build`, a pure function; nothing here decides
//! what a sidecar may see. What the hub adds is time: coalescing of changes, a cache bounded in
//! bytes, and held requests with a budget of their own.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use mimic_core::Mode;
use mimic_core::discovery::egress::matches_etag;
use mimic_core::mesh::input::{
    Config, EndpointSlice, ReferenceGrant, Service, Snapshot, TargetKind,
};
use mimic_core::mesh::policy::Winners;
use mimic_core::mesh::view::build;
use prometheus_client::metrics::gauge::Gauge;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

use super::config::MeshConfig;
use super::convert::Key;
use super::metrics::{MeshMetrics, label};

/// The longest wait between two publications while changes keep arriving.
const MAX_DEBOUNCE: Duration = Duration::from_secs(1);

/// What a watch delivers, as `kube::runtime::watcher::Event` does.
#[derive(Debug, Clone, PartialEq)]
pub enum Event<T> {
    /// A (re)list starts.
    Init,
    InitApply(Key, T),
    /// The (re)list is complete: it replaces what the watch held.
    InitDone,
    Apply(Key, T),
    Delete(Key),
}

struct Table<V> {
    items: BTreeMap<Key, V>,
    /// The objects of a relist that is not complete: the old content is served meanwhile.
    staging: Option<BTreeMap<Key, V>>,
    /// A list was complete once.
    synced: bool,
}

impl<V> Default for Table<V> {
    fn default() -> Table<V> {
        Table {
            items: BTreeMap::new(),
            staging: None,
            synced: false,
        }
    }
}

impl<V: PartialEq> Table<V> {
    /// Applies an event; false when it changed nothing a view depends on (a watch event that
    /// only moved a `resourceVersion`, such as the echo of this controller's own status apply).
    fn feed(&mut self, event: Event<V>) -> bool {
        match event {
            Event::Init => {
                self.staging = Some(BTreeMap::new());
                false
            }
            Event::InitApply(key, value) => match &mut self.staging {
                Some(staging) => {
                    staging.insert(key, value);
                    false
                }
                None => {
                    self.items.insert(key, value);
                    true
                }
            },
            Event::InitDone => {
                let first = !self.synced;
                self.synced = true;
                match self.staging.take() {
                    Some(staging) if staging != self.items => {
                        self.items = staging;
                        true
                    }
                    _ => first,
                }
            }
            Event::Apply(key, value) => {
                let same = self.items.get(&key) == Some(&value);
                if !same {
                    self.items.insert(key, value);
                }
                !same
            }
            Event::Delete(key) => self.items.remove(&key).is_some(),
        }
    }
}

#[derive(Default)]
struct Tables {
    services: Table<Service>,
    slices: Table<EndpointSlice>,
    configs: Table<Config>,
    deployments: Table<()>,
    grants: Table<ReferenceGrant>,
    /// The `ReferenceGrant` CRD is not installed: nobody consents.
    grants_absent: bool,
}

impl Tables {
    fn synced(&self) -> bool {
        self.services.synced
            && self.slices.synced
            && self.configs.synced
            && self.deployments.synced
            && (self.grants.synced || self.grants_absent)
    }
}

/// What views are built from: a snapshot of the objects, and the winning `MimicConfig` of each
/// Deployment (for the cache key).
pub struct Published {
    /// 0 until the first publication.
    pub epoch: u64,
    /// Every list was complete when it was taken.
    pub synced: bool,
    pub snapshot: Snapshot,
    own: BTreeMap<Key, (String, i64)>,
}

/// A built view, shared between the deployments with the same inputs.
pub struct Entry {
    pub etag: String,
    pub json: String,
    gzip: OnceLock<Vec<u8>>,
}

impl Entry {
    /// The view, gzip-compressed (at most as large again as the view, held with it).
    pub fn gzip(&self) -> &[u8] {
        self.gzip.get_or_init(|| {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            // Into memory: neither step can fail.
            let _ = encoder.write_all(self.json.as_bytes());
            encoder.finish().unwrap_or_default()
        })
    }
}

/// What a request for a view is answered with.
pub enum Answer {
    View(Arc<Entry>),
    /// The `If-None-Match` held the current `ETag` until the timeout.
    NotModified(String),
    /// The watch budget is spent.
    Busy,
    /// The watches have not listed everything yet.
    NotReady,
}

type CacheKey = (String, Mode, Option<(String, i64)>);

struct Slot {
    epoch: u64,
    entry: Arc<Entry>,
    used: u64,
}

#[derive(Default)]
struct Cache {
    slots: BTreeMap<CacheKey, Slot>,
    bytes: u64,
    tick: u64,
}

/// A held request: it holds a share of the watch budget and counts in `mimic_mesh_watches`.
struct Watch {
    _permit: OwnedSemaphorePermit,
    gauge: Gauge,
}

impl Watch {
    fn new(permit: OwnedSemaphorePermit, gauge: Gauge) -> Watch {
        gauge.inc();
        Watch {
            _permit: permit,
            gauge,
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.gauge.dec();
    }
}

pub struct Hub {
    config: MeshConfig,
    metrics: MeshMetrics,
    tables: Mutex<Tables>,
    /// Counts the changes; the debouncer waits on it.
    dirty: watch::Sender<u64>,
    published: watch::Sender<Arc<Published>>,
    cache: Mutex<Cache>,
    watches: Arc<Semaphore>,
    closed: watch::Sender<bool>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

macro_rules! feeder {
    ($name:ident, $table:ident, $value:ty) => {
        pub fn $name(&self, event: Event<$value>) {
            if lock(&self.tables).$table.feed(event) {
                self.dirty();
            }
        }
    };
}

impl Hub {
    pub fn new(config: MeshConfig, metrics: MeshMetrics) -> Arc<Hub> {
        let watches = Arc::new(Semaphore::new(config.max_watches));
        Arc::new(Hub {
            config,
            metrics,
            tables: Mutex::default(),
            dirty: watch::channel(0).0,
            published: watch::channel(Arc::new(Published {
                epoch: 0,
                synced: false,
                snapshot: Snapshot::default(),
                own: BTreeMap::new(),
            }))
            .0,
            cache: Mutex::default(),
            watches,
            closed: watch::channel(false).0,
        })
    }

    feeder!(feed_services, services, Service);
    feeder!(feed_slices, slices, EndpointSlice);
    feeder!(feed_configs, configs, Config);
    feeder!(feed_deployments, deployments, ());

    pub fn feed_grants(&self, event: Event<ReferenceGrant>) {
        let changed = {
            let mut tables = lock(&self.tables);
            let was_absent = std::mem::take(&mut tables.grants_absent);
            tables.grants.feed(event) || was_absent
        };
        if changed {
            self.dirty();
        }
    }

    /// The `ReferenceGrant` CRD is not installed (or no longer): nobody consents.
    pub fn grants_absent(&self) {
        let changed = {
            let mut tables = lock(&self.tables);
            let changed = !tables.grants_absent;
            tables.grants = Table::default();
            tables.grants_absent = true;
            changed
        };
        if changed {
            self.dirty();
        }
    }

    /// Something a view depends on changed (an object, a mode).
    pub fn dirty(&self) {
        self.dirty.send_modify(|n| *n += 1);
    }

    #[cfg(test)]
    pub fn metrics(&self) -> &MeshMetrics {
        &self.metrics
    }

    #[cfg(test)]
    pub fn published(&self) -> Arc<Published> {
        self.published.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<Published>> {
        self.published.subscribe()
    }

    /// Ends the held requests (the process is stopping).
    pub fn close(&self) {
        self.closed.send_replace(true);
    }

    /// Takes the snapshot of what the watches hold now and publishes it: the held requests
    /// look at their view again.
    pub fn publish_now(&self) {
        let (snapshot, synced) = {
            let t = lock(&self.tables);
            let grants = (!t.grants_absent).then(|| t.grants.items.values().cloned().collect());
            let objects = &self.metrics.objects;
            let count = |kind: &str, n: usize| {
                objects
                    .get_or_create(&label("kind", kind))
                    .set(i64::try_from(n).unwrap_or(i64::MAX));
            };
            count("service", t.services.items.len());
            count("endpointslice", t.slices.items.len());
            count("mimicconfig", t.configs.items.len());
            count("referencegrant", t.grants.items.len());
            count("deployment", t.deployments.items.len());
            let snapshot = Snapshot {
                cluster: self.config.cluster.clone(),
                services: t.services.items.values().cloned().collect(),
                slices: t.slices.items.values().cloned().collect(),
                configs: t.configs.items.values().cloned().collect(),
                grants,
                deployments: t.deployments.items.keys().cloned().collect(),
            };
            (snapshot, t.synced())
        };
        let winners = Winners::new(&snapshot.configs);
        let targets: BTreeSet<Key> = snapshot
            .configs
            .iter()
            .flat_map(|c| {
                c.targets
                    .iter()
                    .filter(|t| t.kind == TargetKind::Deployment)
                    .map(|t| (c.namespace.clone(), t.name.clone()))
            })
            .collect();
        let own = targets
            .into_iter()
            .filter_map(|(ns, name)| {
                let w = winners.get(TargetKind::Deployment, &ns, &name)?;
                Some(((ns, name), (w.name.clone(), w.generation)))
            })
            .collect();
        self.published.send_modify(|current| {
            *current = Arc::new(Published {
                epoch: current.epoch + 1,
                synced,
                snapshot,
                own,
            });
        });
    }

    /// Publishes the changes, coalesced: the debounce after a quiet period, doubled up to one
    /// second while changes keep arriving (spec §7.2).
    pub async fn run(self: Arc<Hub>) {
        let mut dirty = self.dirty.subscribe();
        let ceiling = MAX_DEBOUNCE.max(self.config.debounce);
        loop {
            if dirty.changed().await.is_err() {
                return;
            }
            let mut wait = self.config.debounce;
            loop {
                dirty.mark_unchanged();
                tokio::time::sleep(wait).await;
                let busy = dirty.has_changed().unwrap_or(false);
                dirty.mark_unchanged();
                self.publish_now();
                if !busy {
                    break;
                }
                wait = (wait * 2).min(ceiling);
                // A change within the next wait: the stream goes on, and so does the doubling.
                if !matches!(
                    tokio::time::timeout(wait, dirty.changed()).await,
                    Ok(Ok(()))
                ) {
                    break;
                }
            }
        }
    }

    /// The view of `deployment` of `namespace`, built or cached.
    fn view(
        &self,
        published: &Published,
        namespace: &str,
        deployment: &str,
        mode: Mode,
    ) -> Arc<Entry> {
        let own = published
            .own
            .get(&(namespace.to_string(), deployment.to_string()))
            .cloned();
        let key = (namespace.to_string(), mode, own);
        let mut cache = lock(&self.cache);
        cache.tick += 1;
        let tick = cache.tick;
        if let Some(slot) = cache.slots.get_mut(&key)
            && slot.epoch == published.epoch
        {
            slot.used = tick;
            return slot.entry.clone();
        }
        let started = Instant::now();
        let built = build(
            &published.snapshot,
            namespace,
            deployment,
            mode,
            &self.config.limits,
        );
        self.metrics
            .build_duration
            .observe(started.elapsed().as_secs_f64());
        let result = if built.truncated > 0 {
            "truncated"
        } else {
            "ok"
        };
        self.metrics
            .builds
            .get_or_create(&label("result", result))
            .inc();
        if built.truncated > 0 {
            self.metrics.truncated.inc_by(built.truncated as u64);
            tracing::warn!(
                namespace,
                deployment,
                left_out = built.truncated,
                "mesh view truncated"
            );
        }
        let entry = Arc::new(Entry {
            etag: built.etag,
            json: built.json,
            gzip: OnceLock::new(),
        });
        self.admit(&mut cache, key, published.epoch, tick, &entry);
        entry
    }

    /// Caches a view, least recently used out first; a view over the whole budget is not cached.
    fn admit(&self, cache: &mut Cache, key: CacheKey, epoch: u64, tick: u64, entry: &Arc<Entry>) {
        if let Some(old) = cache.slots.remove(&key) {
            cache.bytes -= old.entry.json.len() as u64;
        }
        let size = entry.json.len() as u64;
        if size <= self.config.cache_bytes {
            while cache.bytes + size > self.config.cache_bytes {
                let Some(oldest) = cache
                    .slots
                    .iter()
                    .min_by_key(|(_, slot)| slot.used)
                    .map(|(key, _)| key.clone())
                else {
                    break;
                };
                if let Some(slot) = cache.slots.remove(&oldest) {
                    cache.bytes -= slot.entry.json.len() as u64;
                }
            }
            cache.bytes += size;
            cache.slots.insert(
                key,
                Slot {
                    epoch,
                    entry: entry.clone(),
                    used: tick,
                },
            );
        }
        self.metrics
            .cache_bytes
            .set(i64::try_from(cache.bytes).unwrap_or(i64::MAX));
    }

    /// Answers a request for the view of `deployment` of `namespace`. `held` are the values of
    /// `If-None-Match`: when one is the current `ETag` the request waits for a change, up to
    /// `MIMIC_MESH_WATCH_TIMEOUT`. `mode` is read at every look, so a mode that changes wakes it.
    pub async fn answer(
        &self,
        namespace: &str,
        deployment: &str,
        mode: &(dyn Fn() -> Mode + Send + Sync),
        held: &[&str],
    ) -> Answer {
        let mut changes = self.published.subscribe();
        // The snapshot is not kept while the request waits: a view is all it needs.
        let first = {
            let published = changes.borrow_and_update().clone();
            if published.epoch == 0 || !published.synced {
                return Answer::NotReady;
            }
            self.view(&published, namespace, deployment, mode())
        };
        let mut entry = first;
        let current = |entry: &Entry| held.iter().any(|h| matches_etag(h, &entry.etag));
        if !current(&entry) {
            return Answer::View(entry);
        }
        let Ok(permit) = self.watches.clone().try_acquire_owned() else {
            return Answer::Busy;
        };
        let _watch = Watch::new(permit, self.metrics.watches.clone());
        let deadline = tokio::time::Instant::now() + self.config.watch_timeout;
        let mut closed = self.closed.subscribe();
        loop {
            tokio::select! {
                changed = changes.changed() => {
                    if changed.is_err() {
                        return Answer::NotModified(entry.etag.clone());
                    }
                    let published = changes.borrow_and_update().clone();
                    entry = self.view(&published, namespace, deployment, mode());
                    if !current(&entry) {
                        return Answer::View(entry);
                    }
                }
                () = tokio::time::sleep_until(deadline) => {
                    return Answer::NotModified(entry.etag.clone());
                }
                _ = closed.wait_for(|closed| *closed) => {
                    return Answer::NotModified(entry.etag.clone());
                }
            }
        }
    }
}
