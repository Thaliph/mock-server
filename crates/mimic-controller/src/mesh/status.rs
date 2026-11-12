//! `status.conditions` of the `MimicConfig`s (spec §7, §7.2): computed by `mesh::policy` from
//! each published snapshot and applied, by server-side apply with the field manager
//! `mimic-mesh`, when it differs from what this replica applied last. Both replicas do it:
//! equal applies change nothing in the cluster.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use kube::api::{Api, ApiResource, DynamicObject, GroupVersionKind, Patch, PatchParams};
use mimic_core::mesh::input::Snapshot;
use mimic_core::mesh::policy::{Condition, ConfigStatus, statuses};
use serde_json::{Value, json};
use tokio::sync::OnceCell;

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

fn condition(kind: &str, c: &Condition, generation: i64) -> Value {
    let mut message = c.message.clone();
    if message.len() > MAX_MESSAGE {
        let mut end = MAX_MESSAGE;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
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
