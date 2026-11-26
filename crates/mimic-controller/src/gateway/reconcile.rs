//! The gateway reconciler (spec §7.10): the tables the watches feed, and the passes that apply
//! the objects of each accepted Gateway, write the status of each mimic GatewayClass and of each
//! Gateway of a mimic class in a listed namespace, and delete the managed objects no accepted
//! Gateway needs. Both replicas run it (no leader): equal applies change nothing in the cluster.
//! The Gateways mimic does not serve are stubs (#576): they start no work.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Service;
use mimic_core::gateway::accept::Acceptance;
use mimic_core::gateway::input::{
    DeploymentState, Gateway, GatewayClass, ObjectKind, Observed, ServiceState,
};
use mimic_core::gateway::status::Plan;
use mimic_core::gateway::{Settings, accept, managed_for, objects, status};
use serde_json::Value;
use tokio::sync::watch;
use tokio::time::Instant;

use super::cluster::{ApiError, Cluster, StatusTarget};
use super::convert::{self, GatewayEntry, Seen};
use super::metrics::{GatewayMetrics, GatewayState, Outcome};
use crate::mesh::Event;
use crate::mesh::convert::Key;
use crate::mesh::table::Table;

/// The first retry after a failure; it doubles up to [`BACKOFF_MAX`] (GW-14).
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// The objects are applied again at least this often (§7.10), whatever the digests say.
const RESYNC: Duration = Duration::from_secs(300);

/// What the watches deliver.
#[derive(Default)]
struct Tables {
    classes: Table<GatewayClass>,
    gateways: Table<GatewayEntry>,
    deployments: Table<Seen<DeploymentState>>,
    services: Table<Seen<ServiceState>>,
    /// The `gateways` CRD is served: false until the first look.
    crd: bool,
    /// UIDs of the Gateways this replica saw deleted: their objects are the garbage
    /// collector's. Kept while a watched object names one as its owner.
    tombstones: BTreeSet<String>,
    /// The copy of the last pass, while no table changed: a retry or the resync copies nothing.
    picture: Option<Arc<Picture>>,
}

/// A copy of the tables a pass works on, taken once they all listed.
struct Picture {
    classes: BTreeMap<Key, GatewayClass>,
    gateways: BTreeMap<Key, GatewayEntry>,
    deployments: BTreeMap<Key, Seen<DeploymentState>>,
    services: BTreeMap<Key, Seen<ServiceState>>,
    tombstones: BTreeSet<String>,
}

impl Picture {
    fn class_of(&self, class: &str) -> Option<&GatewayClass> {
        self.classes.get(&(String::new(), class.to_string()))
    }

    /// The watched Deployment `key` when it is managed for the Gateway `gateway`.
    fn deployment(&self, key: &Key, gateway: &str) -> Option<&Seen<DeploymentState>> {
        self.deployments.get(key).filter(|s| mine(s, gateway))
    }

    /// The watched Service `key` when it is managed for the Gateway `gateway`.
    fn service(&self, key: &Key, gateway: &str) -> Option<&Seen<ServiceState>> {
        self.services.get(key).filter(|s| mine(s, gateway))
    }
}

/// What a pass keeps for the next one (in memory, one per replica).
struct State {
    /// Body and digest of the last successful apply of each object.
    applied: BTreeMap<(ObjectKind, Key), (Value, Option<u64>)>,
    /// Failures in a row, and when the key may be tried again.
    failing: BTreeMap<Key, (u32, Instant)>,
    /// The Gateways whose conflict was logged (one WARN per streak).
    conflicts: BTreeSet<Key>,
    /// The first object of a generated name that is not managed, of each Gateway.
    unmanaged: BTreeMap<Key, (ObjectKind, String)>,
    /// The state of each Gateway of a mimic class at its last pass, for the gauge.
    states: BTreeMap<Key, GatewayState>,
    resync_at: Instant,
}

pub struct Reconciler {
    settings: Settings,
    cluster: Arc<dyn Cluster>,
    metrics: GatewayMetrics,
    tables: Mutex<Tables>,
    /// Held for a whole pass: one at a time.
    state: tokio::sync::Mutex<State>,
    /// Counts the changes of the tables and of the presence; `run` waits on it.
    changes: watch::Sender<u64>,
    /// Counts the mimic classes that appeared: the `gateways` watch lists again, so that their
    /// stubs are converted in full (§7.10).
    relists: watch::Sender<u64>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A watched object managed by mimic for the Gateway `gateway`.
fn mine<S>(seen: &Seen<S>, gateway: &str) -> bool {
    seen.managed && seen.gateway == gateway
}

fn name_of(key: &Key) -> String {
    if key.0.is_empty() {
        key.1.clone()
    } else {
        format!("{}/{}", key.0, key.1)
    }
}

/// A request of one Gateway (or class) failed.
struct Failure {
    kind: &'static str,
    error: ApiError,
}

fn failure(kind: ObjectKind) -> impl FnOnce(ApiError) -> Failure {
    move |error| Failure {
        kind: kind.as_str(),
        error,
    }
}

impl State {
    /// `key` may be tried now.
    fn due(&self, key: &Key, at: Instant) -> bool {
        self.failing.get(key).is_none_or(|(_, due)| *due <= at)
    }

    /// Counts a failure of `key` in its streak, with one WARN at the first.
    fn failed(&mut self, key: &Key, failure: &Failure, at: Instant) {
        let n = self.failing.get(key).map_or(1, |(n, _)| n + 1);
        let delay = BACKOFF_FIRST
            .saturating_mul(1 << (n - 1).min(5))
            .min(BACKOFF_MAX);
        self.failing.insert(key.clone(), (n, at + delay));
        if n == 1 {
            tracing::warn!(
                gateway = name_of(key),
                kind = failure.kind,
                error = %failure.error,
                "gateway reconcile failed"
            );
        }
    }
}

impl Reconciler {
    pub fn new(
        settings: Settings,
        cluster: Arc<dyn Cluster>,
        metrics: GatewayMetrics,
    ) -> Arc<Reconciler> {
        Arc::new(Reconciler {
            settings,
            cluster,
            metrics,
            tables: Mutex::default(),
            state: tokio::sync::Mutex::new(State {
                applied: BTreeMap::new(),
                failing: BTreeMap::new(),
                conflicts: BTreeSet::new(),
                unmanaged: BTreeMap::new(),
                states: BTreeMap::new(),
                resync_at: Instant::now() + RESYNC,
            }),
            changes: watch::channel(0).0,
            relists: watch::channel(0).0,
        })
    }

    /// A table changed: the next pass takes a new copy.
    fn changed(&self, mut tables: std::sync::MutexGuard<'_, Tables>) {
        tables.picture = None;
        drop(tables);
        self.changes.send_modify(|n| *n += 1);
    }

    /// The Gateway `namespace`/… of class `class` is served: converted in full and reconciled.
    pub fn serves(&self, namespace: &str, class: &str) -> bool {
        self.settings.namespaces.contains(namespace)
            && lock(&self.tables)
                .classes
                .items
                .contains_key(&(String::new(), class.to_string()))
    }

    /// Changes when a mimic class appears: the `gateways` watch then lists again.
    pub fn relists(&self) -> watch::Receiver<u64> {
        self.relists.subscribe()
    }

    pub fn feed_classes(&self, event: Event<GatewayClass>) {
        let mut tables = lock(&self.tables);
        let before: BTreeSet<Key> = tables.classes.items.keys().cloned().collect();
        if tables.classes.feed(event) {
            let added = tables.classes.items.keys().any(|k| !before.contains(k));
            self.changed(tables);
            if added {
                self.relists.send_modify(|n| *n += 1);
            }
        }
    }

    pub fn feed_gateways(&self, event: Event<GatewayEntry>) {
        let mut tables = lock(&self.tables);
        if let Event::Delete(key) = &event
            && let Some(GatewayEntry::Full(gone)) = tables.gateways.items.get(key)
        {
            let uid = gone.uid.clone();
            tables.tombstones.insert(uid);
        }
        if tables.gateways.feed(event) {
            self.changed(tables);
        }
    }

    pub fn feed_deployments(&self, event: Event<Seen<DeploymentState>>) {
        let mut tables = lock(&self.tables);
        if tables.deployments.feed(event) {
            self.changed(tables);
        }
    }

    pub fn feed_services(&self, event: Event<Seen<ServiceState>>) {
        let mut tables = lock(&self.tables);
        if tables.services.feed(event) {
            self.changed(tables);
        }
    }

    /// The `gateways` CRD is served or not; when it is not, the gateway table is emptied and a
    /// pass waits for a new full list (GW-15).
    pub fn crd(&self, present: bool) {
        let mut tables = lock(&self.tables);
        let changed = tables.crd != present;
        tables.crd = present;
        if !present {
            tables.gateways = Table::default();
            tables.picture = None;
        }
        if changed {
            self.changed(tables);
        }
    }

    /// A copy of the tables, or `None` while the picture is partial: the CRD absent or a table
    /// not listed yet (GW-13). The copy is kept until a table changes. The tombstones no watched
    /// object names any more are dropped.
    fn picture(&self) -> Option<Arc<Picture>> {
        let mut tables = lock(&self.tables);
        let t = &mut *tables;
        if !t.crd
            || !t.classes.synced
            || !t.gateways.synced
            || !t.deployments.synced
            || !t.services.synced
        {
            return None;
        }
        if let Some(picture) = &t.picture {
            return Some(picture.clone());
        }
        let owners: BTreeSet<&String> = t
            .deployments
            .items
            .values()
            .filter_map(|s| s.owner.as_ref())
            .chain(t.services.items.values().filter_map(|s| s.owner.as_ref()))
            .collect();
        let kept: BTreeSet<String> = t
            .tombstones
            .iter()
            .filter(|uid| owners.contains(uid))
            .cloned()
            .collect();
        t.tombstones = kept;
        let picture = Arc::new(Picture {
            classes: t.classes.items.clone(),
            gateways: t.gateways.items.clone(),
            deployments: t.deployments.items.clone(),
            services: t.services.items.clone(),
            tombstones: t.tombstones.clone(),
        });
        t.picture = Some(picture.clone());
        Some(picture)
    }

    /// One pass (spec §7.10): the classes, each Gateway of a mimic class, then the managed
    /// objects that no Gateway owns. Returns when to run again: the earliest retry, or the
    /// resync.
    pub async fn pass(&self, now: &str) -> Option<Instant> {
        let mut state = self.state.lock().await;
        let picture = self.picture()?;
        let at = Instant::now();
        if at >= state.resync_at {
            state.applied.clear();
            state.resync_at = at + RESYNC;
        }
        for (key, class) in &picture.classes {
            if !state.due(key, at) {
                continue;
            }
            if let Plan::Write(body) = status::class(class, now) {
                let target = StatusTarget::Class { name: &class.name };
                if let Err(error) = self.cluster.apply_status(target, &body).await {
                    let kind = "GatewayClass";
                    state.failed(key, &Failure { kind, error }, at);
                    continue;
                }
            }
            state.failing.remove(key);
        }
        let mut states = BTreeMap::new();
        for (key, entry) in &picture.gateways {
            // A stub (another class, a namespace not listed, or a class that became a mimic one
            // before the relist) gets nothing.
            let Some(gw) = entry.full() else {
                continue;
            };
            let Some(class) = picture.class_of(&gw.class) else {
                continue;
            };
            if !self.settings.namespaces.contains(&gw.namespace) {
                continue;
            }
            if !state.due(key, at) {
                if let Some(kept) = state.states.get(key) {
                    states.insert(key.clone(), *kept);
                }
                continue;
            }
            let acceptance = accept::gateway(gw, class, &self.settings);
            let (result, kept) = self
                .gateway(&mut state, &picture, gw, &acceptance, now, at)
                .await;
            self.metrics.count(result);
            states.insert(key.clone(), kept);
        }
        self.sweep(&mut state, &picture, at).await;
        // The keys of the sweep: the Gateways named by a watched object.
        let named: BTreeSet<Key> = picture
            .deployments
            .iter()
            .map(|(k, s)| (k.0.clone(), s.gateway.clone()))
            .chain(
                picture
                    .services
                    .iter()
                    .map(|(k, s)| (k.0.clone(), s.gateway.clone())),
            )
            .collect();
        state.failing.retain(|key, _| {
            picture.gateways.contains_key(key)
                || picture.classes.contains_key(key)
                || named.contains(key)
        });
        state
            .unmanaged
            .retain(|key, _| picture.gateways.contains_key(key));
        state
            .conflicts
            .retain(|key| picture.gateways.contains_key(key));
        state.states = states;
        for kind in GatewayState::ALL {
            let n = state.states.values().filter(|s| **s == kind).count();
            self.metrics.set(kind, n);
        }
        // A retry this pass did not take (its key left the sweep) waits for the next change:
        // a due instant in the past would make `run` spin.
        let retry = state
            .failing
            .values()
            .map(|(_, due)| *due)
            .filter(|due| *due > at)
            .min();
        Some(retry.map_or(state.resync_at, |due| due.min(state.resync_at)))
    }

    /// One Gateway of a mimic class: its objects (or their deletion), then its status. Returns
    /// the result to count and the state to keep.
    async fn gateway(
        &self,
        state: &mut State,
        picture: &Picture,
        gw: &Gateway,
        acceptance: &Acceptance,
        now: &str,
        at: Instant,
    ) -> (Outcome, GatewayState) {
        let key = (gw.namespace.clone(), gw.name.clone());
        let name = objects::name(&gw.name);
        let object_key = (gw.namespace.clone(), name.clone());
        let mut outcome = if !acceptance.is_accepted() {
            state.unmanaged.remove(&key);
            state.conflicts.remove(&key);
            self.delete_objects(state, picture, &object_key, &gw.name)
                .await
        } else {
            self.objects(state, picture, gw, acceptance, &key, &object_key)
                .await
        };
        let observed = Observed {
            deployment: picture
                .deployment(&object_key, &gw.name)
                .map(|s| s.state.clone()),
            service: picture
                .service(&object_key, &gw.name)
                .map(|s| s.state.clone()),
            unmanaged: state.unmanaged.get(&key).cloned(),
        };
        let programmed = status::programmed(acceptance, &observed);
        if outcome.is_ok()
            && let Plan::Write(body) = status::gateway(gw, acceptance, &observed, now)
        {
            let target = StatusTarget::Gateway {
                namespace: &gw.namespace,
                name: &gw.name,
            };
            match self.cluster.apply_status(target, &body).await {
                Ok(()) => {
                    if programmed.ok && !status::was_programmed(&gw.observed) {
                        tracing::info!(
                            gateway = name_of(&key),
                            generation = gw.generation,
                            "gateway programmed"
                        );
                    }
                }
                Err(error) => {
                    outcome = Err(Failure {
                        kind: "Gateway",
                        error,
                    })
                }
            }
        }
        let kept = if !acceptance.is_accepted() {
            GatewayState::NotAccepted
        } else if programmed.ok {
            GatewayState::Programmed
        } else {
            GatewayState::Pending
        };
        let result = match outcome {
            Err(failure) => {
                state.failed(&key, &failure, at);
                Outcome::Error
            }
            Ok(()) => {
                state.failing.remove(&key);
                if state.unmanaged.contains_key(&key) {
                    Outcome::Conflict
                } else if !acceptance.is_accepted() {
                    Outcome::Invalid
                } else {
                    Outcome::Ok
                }
            }
        };
        (result, kept)
    }

    /// The object step of an accepted Gateway (GW-10, GW-12): the objects that need an apply,
    /// their ownership, then the applies.
    async fn objects(
        &self,
        state: &mut State,
        picture: &Picture,
        gw: &Gateway,
        acceptance: &Acceptance,
        key: &Key,
        object_key: &Key,
    ) -> Result<(), Failure> {
        let Some(bodies) = objects::objects(gw, acceptance, &self.settings) else {
            return Ok(());
        };
        let (namespace, name) = object_key;
        // Whether the watched object is managed for `gw`, and its digest.
        let watched = |kind: ObjectKind| -> Option<(bool, u64)> {
            match kind {
                ObjectKind::Deployment => picture
                    .deployments
                    .get(object_key)
                    .map(|s| (mine(s, &gw.name), s.digest)),
                ObjectKind::Service => picture
                    .services
                    .get(object_key)
                    .map(|s| (mine(s, &gw.name), s.digest)),
                ObjectKind::PodDisruptionBudget => None,
            }
        };
        let all = [
            (ObjectKind::Deployment, bodies.deployment),
            (ObjectKind::Service, bodies.service),
            (
                ObjectKind::PodDisruptionBudget,
                bodies.pod_disruption_budget,
            ),
        ];
        let needed: Vec<(ObjectKind, Value)> = all
            .into_iter()
            .filter(
                |(kind, body)| match state.applied.get(&(*kind, object_key.clone())) {
                    None => true,
                    Some((applied, digest)) => {
                        applied != body
                            || (*kind != ObjectKind::PodDisruptionBudget
                                && watched(*kind).map(|(_, d)| d) != *digest)
                    }
                },
            )
            .collect();
        for (kind, _) in &needed {
            let managed = match watched(*kind) {
                Some((managed, _)) => Some(managed),
                None => self
                    .cluster
                    .get(*kind, namespace, name)
                    .await
                    .map_err(failure(*kind))?
                    .map(|o| managed_for(&o["metadata"]["labels"], &gw.name)),
            };
            if managed == Some(false) {
                state.unmanaged.insert(key.clone(), (*kind, name.clone()));
                if state.conflicts.insert(key.clone()) {
                    tracing::warn!(
                        gateway = name_of(key),
                        kind = kind.as_str(),
                        name,
                        "gateway object not managed"
                    );
                }
                return Ok(());
            }
        }
        state.unmanaged.remove(key);
        state.conflicts.remove(key);
        for (kind, body) in needed {
            let answer = self
                .cluster
                .apply(kind, namespace, name, &body)
                .await
                .map_err(failure(kind))?;
            let digest = match kind {
                ObjectKind::Deployment => serde_json::from_value::<Deployment>(answer)
                    .ok()
                    .and_then(|d| convert::deployment(&d))
                    .map(|(_, s)| s.digest),
                ObjectKind::Service => serde_json::from_value::<Service>(answer)
                    .ok()
                    .and_then(|s| convert::service(&s))
                    .map(|(_, s)| s.digest),
                ObjectKind::PodDisruptionBudget => None,
            };
            state
                .applied
                .insert((kind, object_key.clone()), (body, digest));
        }
        Ok(())
    }

    /// Deletes the managed Deployment and Service `object_key` of Gateway `gateway`, then its
    /// managed PodDisruptionBudget (GW-13). Each delete carries the object's UID.
    async fn delete_objects(
        &self,
        state: &mut State,
        picture: &Picture,
        object_key: &Key,
        gateway: &str,
    ) -> Result<(), Failure> {
        let (namespace, name) = object_key;
        let watched = [
            (
                ObjectKind::Deployment,
                picture
                    .deployment(object_key, gateway)
                    .map(|s| s.uid.clone()),
            ),
            (
                ObjectKind::Service,
                picture.service(object_key, gateway).map(|s| s.uid.clone()),
            ),
        ];
        let mut deleted = false;
        for (kind, uid) in watched {
            if let Some(uid) = uid {
                self.cluster
                    .delete(kind, namespace, name, &uid)
                    .await
                    .map_err(failure(kind))?;
                deleted = true;
            }
        }
        if deleted {
            let kind = ObjectKind::PodDisruptionBudget;
            let budget = self
                .cluster
                .get(kind, namespace, name)
                .await
                .map_err(failure(kind))?;
            if let Some(budget) = budget
                && managed_for(&budget["metadata"]["labels"], gateway)
                && let Some(uid) = budget["metadata"]["uid"].as_str()
            {
                self.cluster
                    .delete(kind, namespace, name, uid)
                    .await
                    .map_err(failure(kind))?;
            }
            for kind in [
                ObjectKind::Deployment,
                ObjectKind::Service,
                ObjectKind::PodDisruptionBudget,
            ] {
                state.applied.remove(&(kind, object_key.clone()));
            }
        }
        Ok(())
    }

    /// The managed objects, in the listed namespaces, of no Gateway of their namespace (unless
    /// the garbage collector has them) or of a Gateway that left the mimic classes (GW-13).
    async fn sweep(&self, state: &mut State, picture: &Picture, at: Instant) {
        let labelled: BTreeSet<(Key, String)> = picture
            .deployments
            .iter()
            .map(|(k, s)| (k, &s.gateway, s.managed, &s.owner))
            .chain(
                picture
                    .services
                    .iter()
                    .map(|(k, s)| (k, &s.gateway, s.managed, &s.owner)),
            )
            .filter(|(k, _, managed, _)| *managed && self.settings.namespaces.contains(&k.0))
            .filter(|(k, gateway, _, owner)| {
                match picture.gateways.get(&(k.0.clone(), (*gateway).clone())) {
                    None => !owner
                        .as_ref()
                        .is_some_and(|o| picture.tombstones.contains(o)),
                    Some(gw) => {
                        picture.class_of(gw.class()).is_none() && k.1 == objects::name(gateway)
                    }
                }
            })
            .map(|(k, gateway, _, _)| (k.clone(), gateway.clone()))
            .collect();
        for (object_key, gateway) in labelled {
            let key = (object_key.0.clone(), gateway.clone());
            if !state.due(&key, at) {
                continue;
            }
            match self
                .delete_objects(state, picture, &object_key, &gateway)
                .await
            {
                Ok(()) => {
                    state.failing.remove(&key);
                }
                Err(failure) => state.failed(&key, &failure, at),
            }
        }
    }

    /// A pass after each change of a table or of the presence, and when a retry or the resync
    /// is due.
    pub async fn run(self: Arc<Self>) {
        let mut changes = self.changes.subscribe();
        loop {
            changes.borrow_and_update();
            let due = self.pass(&crate::mesh::status::clock()).await;
            tokio::select! {
                changed = changes.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                () = async {
                    match due {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {}
            }
        }
    }
}
