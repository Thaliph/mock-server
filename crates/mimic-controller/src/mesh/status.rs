//! `status.conditions` of the `MimicConfig`s (spec §7, §7.2): computed by `mesh::policy` from
//! each published snapshot and applied, by server-side apply with the field manager
//! `mimic-mesh`, when it differs from what this replica applied last. Both replicas do it:
//! equal applies change nothing in the cluster.
//!
//! The `status.parents[]` of the `HTTPRoute`s (spec §7.9 "Routes") is written differently: the
//! list is atomic and shared with the other controllers of the route, so it is a JSON merge patch
//! carrying the `resourceVersion` that was read, which keeps their entries and replaces mimic's.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use kube::api::{Api, ApiResource, DynamicObject, GroupVersionKind, Patch, PatchParams};
use mimic_core::mesh::input::{ParentRef, Route, Snapshot};
use mimic_core::mesh::policy::{Condition, ConfigStatus, statuses};
use mimic_core::mesh::route::{Compiled, compile};
use serde_json::{Value, json};
use tokio::sync::OnceCell;
use tokio::time::Instant;

use super::convert::Key;
use super::hub::Hub;

pub const FIELD_MANAGER: &str = "mimic-mesh";

/// A failed apply is tried again after this long without a new publication.
const RETRY: Duration = Duration::from_secs(30);
/// `status.conditions[].message` is at most this long in the CRD.
const MAX_MESSAGE: usize = 1024;

/// Forced: `status.conditions` belongs to this manager alone, so a conflict is no second owner
/// to respect.
pub fn apply_params() -> PatchParams {
    PatchParams::apply(FIELD_MANAGER).force()
}

pub type ApplyFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// Applies the status of a `MimicConfig`.
pub trait StatusSink: Send + Sync {
    fn apply<'a>(&'a self, namespace: &'a str, name: &'a str, body: &'a Value) -> ApplyFuture<'a>;
}

/// Server-side apply on the status subresource.
pub struct KubeSink {
    client: Arc<OnceCell<kube::Client>>,
}

impl KubeSink {
    /// `client` is set once the cluster client is available; applies fail until then.
    pub fn new(client: Arc<OnceCell<kube::Client>>) -> KubeSink {
        KubeSink { client }
    }
}

impl StatusSink for KubeSink {
    fn apply<'a>(&'a self, namespace: &'a str, name: &'a str, body: &'a Value) -> ApplyFuture<'a> {
        Box::pin(async move {
            let client = self.client.get().ok_or("no Kubernetes client yet")?;
            let resource = ApiResource::from_gvk(&GroupVersionKind::gvk(
                "mimic.platform.io",
                "v1alpha1",
                "MimicConfig",
            ));
            let api = Api::<DynamicObject>::namespaced_with(client.clone(), namespace, &resource);
            api.patch_status(name, &apply_params(), &Patch::Apply(body))
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }
}

/// `message` cut to the 1024 bytes of the CRD, on a character boundary.
fn cut(message: &str) -> String {
    let mut end = message.len().min(MAX_MESSAGE);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_string()
}

fn condition(kind: &str, c: &Condition, generation: i64) -> Value {
    let message = cut(&c.message);
    let mut value = json!({
        "type": kind,
        "status": if c.ok { "True" } else { "False" },
        "reason": c.reason,
        "observedGeneration": generation,
    });
    if !message.is_empty() {
        value["message"] = Value::String(message);
    }
    value
}

/// The object applied for one `MimicConfig`.
fn body(name: &str, generation: i64, status: &ConfigStatus) -> Value {
    json!({
        "apiVersion": "mimic.platform.io/v1alpha1",
        "kind": "MimicConfig",
        "metadata": {"name": name},
        "status": {"conditions": [
            condition("Accepted", &status.accepted, generation),
            condition("ResolvedRefs", &status.resolved_refs, generation),
        ]},
    })
}

/// One pass over a snapshot: applies the statuses that differ from `last` (what was applied
/// before). False when an apply failed: it is tried again at the next pass.
pub async fn pass(
    snapshot: &Snapshot,
    last: &mut BTreeMap<Key, Value>,
    sink: &dyn StatusSink,
) -> bool {
    let statuses = statuses(snapshot);
    let generations: BTreeMap<Key, i64> = snapshot
        .configs
        .iter()
        .map(|c| ((c.namespace.clone(), c.name.clone()), c.generation))
        .collect();
    last.retain(|key, _| statuses.contains_key(key));
    let mut all = true;
    for (key, status) in &statuses {
        let body = body(&key.1, generations.get(key).copied().unwrap_or(0), status);
        if last.get(key) == Some(&body) {
            continue;
        }
        match sink.apply(&key.0, &key.1, &body).await {
            Ok(()) => {
                last.insert(key.clone(), body);
            }
            Err(error) => {
                all = false;
                tracing::warn!(
                    namespace = key.0,
                    name = key.1,
                    error,
                    "MimicConfig status not applied"
                );
            }
        }
    }
    all
}

/// Applies the statuses of every published snapshot, for ever.
pub async fn run(hub: Arc<Hub>, sink: Arc<dyn StatusSink>) {
    let mut published = hub.subscribe();
    let mut last = BTreeMap::new();
    loop {
        let current = published.borrow_and_update().clone();
        let settled = !current.synced || pass(&current.snapshot, &mut last, &*sink).await;
        if settled {
            if published.changed().await.is_err() {
                return;
            }
        } else {
            tokio::select! {
                changed = published.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                () = tokio::time::sleep(RETRY) => {}
            }
        }
    }
}

/// The controller of the entries of `status.parents[]` that are mimic's.
pub const MIMIC_CONTROLLER: &str = "mimic.platform.io/mesh";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchError {
    /// `409`: the route changed since it was read.
    Conflict,
    Other(String),
}

pub type PatchFuture<'a> = Pin<Box<dyn Future<Output = Result<(), PatchError>> + Send + 'a>>;

/// Patches the status of an `HTTPRoute`.
pub trait RouteSink: Send + Sync {
    fn patch<'a>(&'a self, namespace: &'a str, name: &'a str, body: &'a Value) -> PatchFuture<'a>;
}

/// Merge patch on the status subresource of the route.
pub struct KubeRouteSink {
    client: Arc<OnceCell<kube::Client>>,
}

impl KubeRouteSink {
    /// `client` is set once the cluster client is available; patches fail until then.
    pub fn new(client: Arc<OnceCell<kube::Client>>) -> KubeRouteSink {
        KubeRouteSink { client }
    }
}

impl RouteSink for KubeRouteSink {
    fn patch<'a>(&'a self, namespace: &'a str, name: &'a str, body: &'a Value) -> PatchFuture<'a> {
        Box::pin(async move {
            let client = self
                .client
                .get()
                .ok_or_else(|| PatchError::Other("no Kubernetes client yet".into()))?;
            let resource = ApiResource::from_gvk(&GroupVersionKind::gvk(
                "gateway.networking.k8s.io",
                "v1",
                "HTTPRoute",
            ));
            let api = Api::<DynamicObject>::namespaced_with(client.clone(), namespace, &resource);
            match api
                .patch_status(name, &route_patch_params(), &Patch::Merge(body))
                .await
            {
                Ok(_) => Ok(()),
                Err(kube::Error::Api(status)) if status.code == 409 => Err(PatchError::Conflict),
                Err(error) => Err(PatchError::Other(error.to_string())),
            }
        })
    }
}

/// A route's patch that failed is tried again after this long.
const ROUTE_RETRY: Duration = Duration::from_secs(30);
/// `status.parents` holds at most this many entries in the CRD.
const MAX_PARENTS: usize = 32;

/// What to do with the status of one route.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// The cluster holds mimic's entries already (up to `lastTransitionTime` of unchanged
    /// conditions).
    Keep,
    /// The whole `status.parents` to write.
    Write(Vec<Value>),
    /// The list would exceed the 32 entries of the CRD.
    TooMany,
}

/// What the writer remembers between two passes.
#[derive(Default)]
pub struct RouteState {
    /// The `resourceVersion` a patch was sent from, answered `ok` or `conflict`: the route is not
    /// decided again before the watch delivers another one, so a patch is never sent twice from
    /// the same read.
    sent: BTreeMap<Key, String>,
    /// The routes whose patch failed, and when each may be tried again; present for as long as
    /// the streak lasts (one WARN per streak).
    failing: BTreeMap<Key, Instant>,
}

/// Only `status.parents` is replaced, by a merge patch (the other controllers' entries are in the
/// list written back): the field manager is `mimic-mesh`.
pub fn route_patch_params() -> PatchParams {
    PatchParams {
        field_manager: Some(FIELD_MANAGER.to_string()),
        ..PatchParams::default()
    }
}

/// The merge patch: the `resourceVersion` read as precondition, and the new `status.parents`.
pub fn route_body(resource_version: &str, parents: &[Value]) -> Value {
    json!({
        "metadata": {"resourceVersion": resource_version},
        "status": {"parents": parents},
    })
}

/// A condition of mimic's entry; `previous` is the cluster's condition of the same type, whose
/// `lastTransitionTime` stays while the `status` does not change.
pub fn route_condition(
    kind: &str,
    c: &Condition,
    generation: i64,
    previous: Option<&Value>,
    now: &str,
) -> Value {
    let status = if c.ok { "True" } else { "False" };
    let since = previous
        .filter(|p| p["status"] == status)
        .and_then(|p| p["lastTransitionTime"].as_str())
        .unwrap_or(now);
    json!({
        "type": kind,
        "status": status,
        "reason": c.reason,
        "message": cut(&c.message),
        "observedGeneration": generation,
        "lastTransitionTime": since,
    })
}

/// `parent` as the route writes it, which is how its entry is told from another.
fn parent_ref(p: &ParentRef) -> Value {
    let mut value = json!({"group": p.group, "kind": p.kind, "name": p.name});
    if let Some(namespace) = &p.namespace {
        value["namespace"] = json!(namespace);
    }
    if let Some(section) = &p.section_name {
        value["sectionName"] = json!(section);
    }
    if let Some(port) = p.port {
        value["port"] = json!(port);
    }
    value
}

fn is_mimic(entry: &Value) -> bool {
    entry["controllerName"] == MIMIC_CONTROLLER
}

/// Decides what to write for `route`, given the `status.parents` the cluster holds. The entries of
/// other controllers are kept as they are and in their order, mimic's follow in `parentRefs`
/// order; `now` (RFC 3339, seconds) is the time of the conditions that change.
pub fn plan_route(route: &Route, compiled: &Compiled, observed: &[Value], now: &str) -> Plan {
    let ours: Vec<Value> = compiled
        .parents
        .iter()
        .map(|status| {
            let parent_ref = parent_ref(&route.parents[status.index]);
            let before = observed
                .iter()
                .find(|e| is_mimic(e) && e["parentRef"] == parent_ref);
            let previous = |kind: &str| {
                before
                    .and_then(|e| e["conditions"].as_array())
                    .and_then(|list| list.iter().find(|c| c["type"] == kind))
            };
            json!({
                "parentRef": parent_ref,
                "controllerName": MIMIC_CONTROLLER,
                "conditions": [
                    route_condition("Accepted", &status.accepted, route.generation,
                        previous("Accepted"), now),
                    route_condition("ResolvedRefs", &status.resolved_refs, route.generation,
                        previous("ResolvedRefs"), now),
                ],
            })
        })
        .collect();
    let held: Vec<&Value> = observed.iter().filter(|e| is_mimic(e)).collect();
    if held.len() == ours.len() && held.iter().zip(&ours).all(|(a, b)| *a == b) {
        return Plan::Keep;
    }
    let mut list: Vec<Value> = observed.iter().filter(|e| !is_mimic(e)).cloned().collect();
    list.extend(ours);
    if list.len() > MAX_PARENTS {
        Plan::TooMany
    } else {
        Plan::Write(list)
    }
}

/// One pass over the published snapshot: one patch at a time, for each route whose entries differ
/// from the cluster's. Returns when the next failed route may be tried again.
pub async fn route_pass(
    hub: &Hub,
    state: &mut RouteState,
    sink: &dyn RouteSink,
    now: &str,
) -> Option<Instant> {
    let published = hub.published();
    let snapshot = &published.snapshot;
    let keys: BTreeSet<Key> = snapshot
        .routes
        .iter()
        .map(|r| (r.namespace.clone(), r.name.clone()))
        .collect();
    state.sent.retain(|key, _| keys.contains(key));
    state.failing.retain(|key, _| keys.contains(key));
    for route in &snapshot.routes {
        let key = (route.namespace.clone(), route.name.clone());
        let Some(observed) = hub.observed_route(&key) else {
            continue;
        };
        // The snapshot or the watch is behind the other: the next publication decides.
        if observed.generation != route.generation
            || state.sent.get(&key) == Some(&observed.resource_version)
            || state
                .failing
                .get(&key)
                .is_some_and(|due| *due > Instant::now())
        {
            continue;
        }
        let compiled = compile(route, &snapshot.services, snapshot.grants.as_deref());
        let failed = match plan_route(route, &compiled, &observed.parents, now) {
            Plan::Keep => {
                state.failing.remove(&key);
                continue;
            }
            Plan::TooMany => Some("status.parents would exceed 32 entries".to_string()),
            Plan::Write(parents) => {
                let body = route_body(&observed.resource_version, &parents);
                match sink.patch(&key.0, &key.1, &body).await {
                    Ok(()) => {
                        hub.count_route_status("ok");
                        state.sent.insert(key.clone(), observed.resource_version);
                        state.failing.remove(&key);
                        None
                    }
                    Err(PatchError::Conflict) => {
                        hub.count_route_status("conflict");
                        state.sent.insert(key.clone(), observed.resource_version);
                        state.failing.remove(&key);
                        None
                    }
                    Err(PatchError::Other(error)) => Some(error),
                }
            }
        };
        if let Some(error) = failed {
            hub.count_route_status("error");
            let due = Instant::now() + ROUTE_RETRY;
            if state.failing.insert(key.clone(), due).is_none() {
                tracing::warn!(
                    route = format!("{}/{}", key.0, key.1),
                    error,
                    "httproute status failed"
                );
            }
        }
    }
    state.failing.values().min().copied()
}

/// RFC 3339, in seconds, of now.
fn clock() -> String {
    let now = time::OffsetDateTime::now_utc();
    now.replace_nanosecond(0)
        .unwrap_or(now)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Writes the statuses of the routes for every publication and every route read, for ever.
pub async fn run_routes(hub: Arc<Hub>, sink: Arc<dyn RouteSink>) {
    let mut published = hub.subscribe();
    let mut observed = hub.route_changes();
    let mut state = RouteState::default();
    loop {
        let current = published.borrow_and_update().clone();
        observed.borrow_and_update();
        let due = if current.synced {
            route_pass(&hub, &mut state, &*sink, &clock()).await
        } else {
            None
        };
        tokio::select! {
            changed = published.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            changed = observed.changed() => {
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
