//! Cache of the `MimicWorkload`s (spec §7.1), indexed by namespace and `spec.deployment`.

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use futures::StreamExt;
use kube::api::{Api, ApiResource, DynamicObject, GroupVersionKind};
use kube::runtime::WatchStreamExt;
use kube::runtime::watcher::{self, Event};

#[derive(Default)]
struct State {
    /// `(namespace, MimicWorkload name)` to `spec.deployment`.
    items: BTreeMap<(String, String), String>,
    synced: bool,
}

#[derive(Default)]
pub struct WorkloadCache {
    state: RwLock<State>,
}

impl WorkloadCache {
    pub fn new() -> WorkloadCache {
        WorkloadCache::default()
    }

    /// Adds or updates a `MimicWorkload`.
    pub fn apply(&self, namespace: &str, name: &str, deployment: &str) {
        self.write()
            .items
            .insert((namespace.into(), name.into()), deployment.into());
    }

    pub fn remove(&self, namespace: &str, name: &str) {
        self.write().items.remove(&(namespace.into(), name.into()));
    }

    /// Replaces the whole content after a (re)list and marks the cache synced.
    pub fn replace(&self, items: impl IntoIterator<Item = (String, String, String)>) {
        let mut state = self.write();
        state.items = items.into_iter().map(|(ns, n, d)| ((ns, n), d)).collect();
        state.synced = true;
    }

    pub fn is_synced(&self) -> bool {
        self.read().synced
    }

    /// Whether a `MimicWorkload` of `namespace` has `spec.deployment` equal to `deployment`.
    pub fn has(&self, namespace: &str, deployment: &str) -> bool {
        self.read()
            .items
            .iter()
            .any(|((ns, _), d)| ns == namespace && d == deployment)
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    pub fn synced_with(items: &[(&str, &str, &str)]) -> WorkloadCache {
        let cache = WorkloadCache::new();
        cache.replace(
            items
                .iter()
                .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string())),
        );
        cache
    }
}

/// The client of the cluster, waited for as long as it takes (outside a cluster it never comes).
pub async fn client() -> kube::Client {
    loop {
        match kube::Client::try_default().await {
            Ok(client) => return client,
            Err(e) => {
                tracing::warn!(error = %e, "no Kubernetes client yet");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

/// Keeps `cache` in line with the `MimicWorkload`s of the cluster, for ever. As long as it is
/// unsynced, `/inject/workload` fails closed and the controller is not ready.
pub async fn watch(client: kube::Client, cache: Arc<WorkloadCache>) {
    let resource = ApiResource::from_gvk(&GroupVersionKind::gvk(
        "mimic.platform.io",
        "v1alpha1",
        "MimicWorkload",
    ));
    let api = Api::<DynamicObject>::all_with(client, &resource);
    let mut events =
        std::pin::pin!(watcher::watcher(api, watcher::Config::default()).default_backoff());
    let mut listing = Vec::new();
    while let Some(event) = events.next().await {
        match event {
            Ok(Event::Init) => listing.clear(),
            Ok(Event::InitApply(object)) => listing.extend(entry(&object)),
            Ok(Event::InitDone) => {
                cache.replace(std::mem::take(&mut listing));
                tracing::info!("MimicWorkload cache synced");
            }
            Ok(Event::Apply(object)) => match entry(&object) {
                Some((ns, name, deployment)) => cache.apply(&ns, &name, &deployment),
                None => remove(&cache, &object),
            },
            Ok(Event::Delete(object)) => remove(&cache, &object),
            Err(e) => tracing::warn!(error = %e, "MimicWorkload watch error"),
        }
    }
}

fn remove(cache: &WorkloadCache, object: &DynamicObject) {
    if let (Some(ns), Some(name)) = (&object.metadata.namespace, &object.metadata.name) {
        cache.remove(ns, name);
    }
}

/// `(namespace, name, spec.deployment)` of a `MimicWorkload`.
fn entry(object: &DynamicObject) -> Option<(String, String, String)> {
    let deployment = object.data.get("spec")?.get("deployment")?.as_str()?;
    Some((
        object.metadata.namespace.clone()?,
        object.metadata.name.clone()?,
        deployment.to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inj_08_cache_is_keyed_by_namespace_and_spec_deployment() {
        let cache = WorkloadCache::new();
        assert!(!cache.is_synced());
        cache.replace([("shop".into(), "w1".into(), "api".into())]);
        assert!(cache.is_synced());
        assert!(cache.has("shop", "api"));
        assert!(!cache.has("shop", "web"));
        assert!(!cache.has("other", "api"));
    }

    #[test]
    fn inj_08_cache_keeps_a_deployment_selected_while_one_workload_names_it() {
        let cache = WorkloadCache::synced_with(&[("shop", "w1", "api"), ("shop", "w2", "api")]);
        cache.remove("shop", "w1");
        assert!(cache.has("shop", "api"));
        cache.remove("shop", "w2");
        assert!(!cache.has("shop", "api"));
        cache.apply("shop", "w3", "web");
        assert!(cache.has("shop", "web"));
        cache.apply("shop", "w3", "api");
        assert!(!cache.has("shop", "web") && cache.has("shop", "api"));
    }

    #[test]
    fn inj_08_cache_replace_drops_vanished_workloads() {
        let cache = WorkloadCache::synced_with(&[("shop", "w1", "api")]);
        cache.replace([("shop".into(), "w2".into(), "web".into())]);
        assert!(!cache.has("shop", "api") && cache.has("shop", "web"));
    }

    #[test]
    fn tls_12_exclusions_are_the_sorted_deduplicated_union_per_deployment() {
        let cache = WorkloadCache::synced_with_exclude(&[
            ("shop", "w1", "api", &["b.test", "a.test"]),
            ("shop", "w2", "api", &["b.test", "*.c.test"]),
            ("shop", "w3", "web", &["web.test"]),
            ("other", "w4", "api", &["other.test"]),
        ]);
        assert_eq!(
            cache.exclude("shop", "api"),
            ["*.c.test", "a.test", "b.test"]
        );
        assert_eq!(cache.exclude("shop", "web"), ["web.test"]);
        assert!(cache.exclude("shop", "none").is_empty());
        cache.remove("shop", "w2");
        assert_eq!(cache.exclude("shop", "api"), ["a.test", "b.test"]);
    }

    #[test]
    fn tls_12_entry_reads_spec_tls_exclude() {
        let object: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "mimic.platform.io/v1alpha1", "kind": "MimicWorkload",
            "metadata": {"name": "w", "namespace": "shop"},
            "spec": {"deployment": "api", "tls": {"exclude": ["a.test", 7, "b.test"]}},
        }))
        .unwrap();
        let (ns, name, deployment, exclude) = entry(&object).unwrap();
        assert_eq!((ns.as_str(), name.as_str(), deployment.as_str()), ("shop", "w", "api"));
        assert_eq!(exclude, ["a.test", "b.test"]);
    }
}
