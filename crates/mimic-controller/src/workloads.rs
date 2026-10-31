//! Cache of the `MimicWorkload`s (spec §7.1), indexed by namespace and `spec.deployment`.

use std::collections::BTreeMap;
use std::sync::RwLock;

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
        let _ = (namespace, name, deployment);
        todo!()
    }

    pub fn remove(&self, namespace: &str, name: &str) {
        let _ = (namespace, name);
        todo!()
    }

    /// Replaces the whole content after a (re)list and marks the cache synced.
    pub fn replace(&self, items: impl IntoIterator<Item = (String, String, String)>) {
        let _ = items.into_iter().count();
        todo!()
    }

    pub fn is_synced(&self) -> bool {
        todo!()
    }

    /// Whether a `MimicWorkload` of `namespace` has `spec.deployment` equal to `deployment`.
    pub fn has(&self, namespace: &str, deployment: &str) -> bool {
        let _ = (namespace, deployment);
        todo!()
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
}
