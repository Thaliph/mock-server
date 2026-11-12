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
    /// The gzip copy was counted in the cache (see [`Hub::compress`]).
    charged: std::sync::atomic::AtomicBool,
}

impl Entry {
    /// What it weighs in the cache: the view, and its gzip copy once that is counted.
    fn cost(&self) -> u64 {
        let zipped = if self.charged.load(std::sync::atomic::Ordering::SeqCst) {
            self.gzip.get().map_or(0, Vec::len)
        } else {
            0
        };
        (self.json.len() + zipped) as u64
    }

    /// The view, gzip-compressed; go through [`Hub::compress`] first so that the cache counts it.
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

/// A view being built: leaves the map of gates when it ends, nobody else waiting.
struct Gate<'a> {
    hub: &'a Hub,
    key: CacheKey,
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl Drop for Gate<'_> {
    fn drop(&mut self) {
        let mut gates = lock(&self.hub.gates);
        // The map and this guard hold it: nobody waits.
        if Arc::strong_count(&self.lock) == 2 {
            gates.remove(&self.key);
        }
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
    /// One lock per view being built: the others of that key wait for it, the other keys do not.
    gates: Mutex<BTreeMap<CacheKey, Arc<tokio::sync::Mutex<()>>>>,
    watches: Arc<Semaphore>,
    closed: watch::Sender<bool>,
}

/// The newest publication; the guard of the channel does not live across an await.
fn latest(changes: &mut watch::Receiver<Arc<Published>>) -> Arc<Published> {
    changes.borrow_and_update().clone()
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
            gates: Mutex::default(),
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
    async fn view(
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
        if let Some(entry) = self.cached(&key, published.epoch) {
            return entry;
        }
        let gate_lock = self.gate_of(&key);
        let gate = Gate {
            hub: self,
            key: key.clone(),
            lock: gate_lock,
        };
        // Waits without blocking a worker; the gate leaves the map when this ends, also on a panic.
        let _building = gate.lock.lock().await;
        // Another request may have built it while this one waited.
        if let Some(entry) = self.cached(&key, published.epoch) {
            return entry;
        }
        let entry = self.build_entry(published, namespace, deployment, mode);
        {
            let mut cache = lock(&self.cache);
            cache.tick += 1;
            let tick = cache.tick;
            self.admit(&mut cache, key, published.epoch, tick, &entry);
        }
        entry
    }

    fn gate_of(&self, key: &CacheKey) -> Arc<tokio::sync::Mutex<()>> {
        lock(&self.gates).entry(key.clone()).or_default().clone()
    }

    fn cached(&self, key: &CacheKey, epoch: u64) -> Option<Arc<Entry>> {
        let mut cache = lock(&self.cache);
        cache.tick += 1;
        let tick = cache.tick;
        let slot = cache
            .slots
            .get_mut(key)
            .filter(|slot| slot.epoch == epoch)?;
        slot.used = tick;
        Some(slot.entry.clone())
    }

    /// Builds a view, outside the lock of the cache.
    fn build_entry(
        &self,
        published: &Published,
        namespace: &str,
        deployment: &str,
        mode: Mode,
    ) -> Arc<Entry> {
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
                skipped = built.skipped.join(","),
                "mesh view truncated"
            );
        }
        Arc::new(Entry {
            etag: built.etag,
            json: built.json,
            gzip: OnceLock::new(),
            charged: Default::default(),
        })
    }

    /// Caches a view, least recently used out first; a view over the whole budget is not cached.
    fn admit(&self, cache: &mut Cache, key: CacheKey, epoch: u64, tick: u64, entry: &Arc<Entry>) {
        if let Some(old) = cache.slots.remove(&key) {
            cache.bytes = cache.bytes.saturating_sub(old.entry.cost());
        }
        let size = entry.cost();
        if size <= self.config.cache_bytes {
            self.make_room(cache, size, None);
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

    /// Evicts the least recently used slots (not `keep`) until `size` more bytes fit.
    fn make_room(&self, cache: &mut Cache, size: u64, keep: Option<&CacheKey>) {
        while cache.bytes + size > self.config.cache_bytes {
            let Some(oldest) = cache
                .slots
                .iter()
                .filter(|(key, _)| Some(*key) != keep)
                .min_by_key(|(_, slot)| slot.used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(slot) = cache.slots.remove(&oldest) {
                cache.bytes = cache.bytes.saturating_sub(slot.entry.cost());
            }
        }
    }

    /// Compresses a view for a sidecar that accepts gzip, and counts the copy in the cache budget
    /// while the view is cached (a copy of a view that is not cached lives with its answer).
    pub fn compress(&self, entry: &Arc<Entry>) {
        use std::sync::atomic::Ordering::SeqCst;
        let size = entry.gzip().len() as u64;
        let mut cache = lock(&self.cache);
        let Some(key) = cache
            .slots
            .iter()
            .find(|(_, slot)| Arc::ptr_eq(&slot.entry, entry))
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        // Under the lock, with the slot found: the flag and the bytes change together, so that no
        // admit or eviction subtracts a copy that was not added.
        if entry.charged.swap(true, SeqCst) {
            return;
        }
        self.make_room(&mut cache, size, Some(&key));
        cache.bytes += size;
        if cache.bytes > self.config.cache_bytes
            && let Some(slot) = cache.slots.remove(&key)
        {
            // Alone it is over the budget: the view is served, not kept.
            cache.bytes = cache.bytes.saturating_sub(slot.entry.cost());
        }
        self.metrics
            .cache_bytes
            .set(i64::try_from(cache.bytes).unwrap_or(i64::MAX));
    }

    /// Tests: the cache holds exactly what its entries weigh.
    #[cfg(test)]
    pub fn cache_consistent(&self) -> bool {
        let cache = lock(&self.cache);
        cache.bytes == cache.slots.values().map(|s| s.entry.cost()).sum::<u64>()
    }

    /// Answers a request for the view of `deployment` of `namespace`. `held` are the values of
    /// `If-None-Match`: when one is the current `ETag` the request waits for a change, up to
    /// `MIMIC_MESH_WATCH_TIMEOUT`; `on_hold` is called once it is held. `mode` is read at every look, so a mode that changes wakes it.
    pub async fn answer(
        &self,
        namespace: &str,
        deployment: &str,
        mode: &(dyn Fn() -> Mode + Send + Sync),
        held: &[&str],
        on_hold: &(dyn Fn() + Send + Sync),
    ) -> Answer {
        let mut changes = self.published.subscribe();
        // The snapshot is not kept while the request waits: a view is all it needs.
        let first = {
            let published = latest(&mut changes);
            if published.epoch == 0 || !published.synced {
                return Answer::NotReady;
            }
            self.view(&published, namespace, deployment, mode()).await
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
        // Only now, with a share of the watch budget, may the connection leave the listener's cap.
        on_hold();
        let deadline = tokio::time::Instant::now() + self.config.watch_timeout;
        let mut closed = self.closed.subscribe();
        loop {
            // What woke it, as plain values: no guard of a channel lives across the awaits below.
            let woken = tokio::select! {
                changed = changes.changed() => changed.is_ok(),
                () = tokio::time::sleep_until(deadline) => false,
                () = async { let _ = closed.wait_for(|closed| *closed).await; } => false,
            };
            if !woken {
                return Answer::NotModified(entry.etag.clone());
            }
            let published = latest(&mut changes);
            entry = self.view(&published, namespace, deployment, mode()).await;
            if !current(&entry) {
                return Answer::View(entry);
            }
        }
    }
}
