//! Cache of the `MimicWorkload`s (spec §7.1), indexed by namespace and `spec.deployment`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use futures::StreamExt;
use kube::api::{Api, ApiResource, DynamicObject, GroupVersionKind};
use kube::runtime::WatchStreamExt;
use kube::runtime::watcher::{self, Event};
use mimic_core::mock;
pub use mimic_core::mock::{Effective, MockSettings};
use mimic_core::proxy::tls::MAX_EXCLUDES;

/// `(namespace, name, spec.deployment, spec.tls.exclude, spec.discovery.redact)` of a
/// `MimicWorkload`.
pub type Entry = (String, String, String, Vec<String>, Vec<String>);

/// A `MimicWorkload` and the `status.discovery` it holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub namespace: String,
    pub name: String,
    pub deployment: String,
    pub status: Option<serde_json::Value>,
    /// The `status.mode` the cluster holds for it.
    pub mode_status: Option<serde_json::Value>,
}

/// `spec.deployment`, `spec.tls.exclude` and `spec.discovery.redact` of a `MimicWorkload`.
type Settings = (String, Vec<String>, Vec<String>);

#[derive(Default)]
struct State {
    /// `(namespace, MimicWorkload name)` to its settings.
    items: BTreeMap<(String, String), Settings>,
    /// `(namespace, MimicWorkload name)` to the `status.discovery` the cluster holds for it.
    statuses: BTreeMap<(String, String), serde_json::Value>,
    /// `(namespace, MimicWorkload name)` to the `status.mode` the cluster holds for it.
    mode_statuses: BTreeMap<(String, String), serde_json::Value>,
    /// `(namespace, MimicWorkload name)` to its mock settings (absent: the defaults).
    mocks: BTreeMap<(String, String), MockSettings>,
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
    pub fn apply(
        &self,
        namespace: &str,
        name: &str,
        deployment: &str,
        exclude: Vec<String>,
        redact: Vec<String>,
    ) {
        self.write().items.insert(
            (namespace.into(), name.into()),
            (deployment.into(), exclude, redact),
        );
    }

    pub fn remove(&self, namespace: &str, name: &str) {
        let mut state = self.write();
        let key = (namespace.to_string(), name.to_string());
        state.items.remove(&key);
        state.statuses.remove(&key);
        state.mode_statuses.remove(&key);
        state.mocks.remove(&key);
    }

    /// Replaces the whole content after a (re)list and marks the cache synced.
    pub fn replace(&self, items: impl IntoIterator<Item = Entry>) {
        let mut state = self.write();
        state.items = items
            .into_iter()
            .map(|(ns, n, d, e, r)| ((ns, n), (d, e, r)))
            .collect();
        state.statuses.clear();
        state.mode_statuses.clear();
        state.mocks.clear();
        state.synced = true;
    }

    /// Records the mock settings of a `MimicWorkload`.
    pub fn set_mock(&self, namespace: &str, name: &str, mock: MockSettings) {
        let mut state = self.write();
        let key = (namespace.to_string(), name.to_string());
        if !state.items.contains_key(&key) {
            return;
        }
        if mock == MockSettings::default() {
            state.mocks.remove(&key);
        } else {
            state.mocks.insert(key, mock);
        }
    }

    /// The effective mode of `deployment` in `namespace` (spec §7): `mock` when any of its
    /// `MimicWorkload`s says so, with the latency factor of the first of them in name order and
    /// the union of their passthrough lists.
    pub fn effective(&self, namespace: &str, deployment: &str) -> Effective {
        let state = self.read();
        // `items` is sorted by `(namespace, name)`, so the settings come in name order.
        mock::effective(
            state
                .items
                .iter()
                .filter(|(key, (d, _, _))| key.0 == namespace && d == deployment)
                .filter_map(|(key, _)| state.mocks.get(key)),
        )
    }

    /// Records the `status.discovery` the cluster holds for a `MimicWorkload`.
    pub fn set_status(&self, namespace: &str, name: &str, status: Option<serde_json::Value>) {
        let mut state = self.write();
        let key = (namespace.to_string(), name.to_string());
        if !state.items.contains_key(&key) {
            return;
        }
        match status {
            Some(status) => state.statuses.insert(key, status),
            None => state.statuses.remove(&key),
        };
    }

    /// Records the `status.mode` the cluster holds for a `MimicWorkload`.
    pub fn set_mode_status(&self, namespace: &str, name: &str, status: Option<serde_json::Value>) {
        let mut state = self.write();
        let key = (namespace.to_string(), name.to_string());
        if !state.items.contains_key(&key) {
            return;
        }
        match status {
            Some(status) => state.mode_statuses.insert(key, status),
            None => state.mode_statuses.remove(&key),
        };
    }

    /// Every `MimicWorkload` with the status it holds, sorted by namespace then name.
    pub fn targets(&self) -> Vec<Target> {
        let state = self.read();
        state
            .items
            .iter()
            .map(|(key, (deployment, _, _))| Target {
                namespace: key.0.clone(),
                name: key.1.clone(),
                deployment: deployment.clone(),
                status: state.statuses.get(key).cloned(),
                mode_status: state.mode_statuses.get(key).cloned(),
            })
            .collect()
    }

    pub fn is_synced(&self) -> bool {
        self.read().synced
    }

    /// Whether a `MimicWorkload` of `namespace` has `spec.deployment` equal to `deployment`.
    pub fn has(&self, namespace: &str, deployment: &str) -> bool {
        self.read()
            .items
            .iter()
            .any(|((ns, _), (d, _, _))| ns == namespace && d == deployment)
    }

    /// `spec.tls.exclude` of every `MimicWorkload` of `namespace` naming `deployment`: the
    /// sorted, deduplicated union (spec §5), cut to the sidecar's limit of
    /// [`MAX_EXCLUDES`] entries: a longer list would make it exit.
    pub fn exclude(&self, namespace: &str, deployment: &str) -> Vec<String> {
        let mut union = self.union(namespace, deployment, |(_, exclude, _)| exclude);
        if union.len() > MAX_EXCLUDES {
            tracing::warn!(
                namespace,
                deployment,
                entries = union.len(),
                kept = MAX_EXCLUDES,
                "tls exclude truncated"
            );
            union.truncate(MAX_EXCLUDES);
        }
        union
    }

    /// `spec.discovery.redact` of every `MimicWorkload` of `namespace` naming `deployment`: the
    /// sorted, deduplicated union, lower-cased (spec §7): the CRD accepts any case, the sidecar
    /// compares names in lower case.
    pub fn redact(&self, namespace: &str, deployment: &str) -> Vec<String> {
        self.union(namespace, deployment, |(_, _, redact)| redact)
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn union(
        &self,
        namespace: &str,
        deployment: &str,
        field: fn(&Settings) -> &Vec<String>,
    ) -> Vec<String> {
        let state = self.read();
        let union: BTreeSet<&String> = state
            .items
            .iter()
            .filter(|((ns, _), (d, _, _))| ns == namespace && d == deployment)
            .flat_map(|(_, settings)| field(settings))
            .collect();
        union.into_iter().cloned().collect()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    pub fn synced_with(items: &[(&str, &str, &str)]) -> WorkloadCache {
        let items: Vec<_> = items
            .iter()
            .map(|(a, b, c)| (*a, *b, *c, &[][..]))
            .collect();
        WorkloadCache::synced_with_exclude(&items)
    }

    #[cfg(test)]
    pub fn synced_with_exclude(items: &[(&str, &str, &str, &[&str])]) -> WorkloadCache {
        let items: Vec<_> = items
            .iter()
            .map(|(a, b, c, e)| (*a, *b, *c, *e, &[][..]))
            .collect();
        WorkloadCache::synced_with_settings(&items)
    }

    #[cfg(test)]
    pub fn synced_with_settings(items: &[TestWorkload]) -> WorkloadCache {
        let owned = |list: &[&str]| list.iter().map(|x| x.to_string()).collect();
        let cache = WorkloadCache::new();
        cache.replace(items.iter().map(|(a, b, c, e, r)| {
            (
                a.to_string(),
                b.to_string(),
                c.to_string(),
                owned(e),
                owned(r),
            )
        }));
        cache
    }
}

/// `(namespace, name, deployment, exclude, redact)` of a test workload.
#[cfg(test)]
pub type TestWorkload<'a> = (&'a str, &'a str, &'a str, &'a [&'a str], &'a [&'a str]);

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
    let mut statuses = Vec::new();
    let mut mode_statuses = Vec::new();
    let mut mocks = Vec::new();
    while let Some(event) = events.next().await {
        match event {
            Ok(Event::Init) => {
                listing.clear();
                statuses.clear();
                mode_statuses.clear();
                mocks.clear();
            }
            Ok(Event::InitApply(object)) => {
                if let Some(found) = entry(&object) {
                    statuses.push((found.0.clone(), found.1.clone(), held_status(&object)));
                    mode_statuses.push((found.0.clone(), found.1.clone(), held_mode(&object)));
                    mocks.push((found.0.clone(), found.1.clone(), mock_of(&object)));
                    listing.push(found);
                }
            }
            Ok(Event::InitDone) => {
                cache.replace(std::mem::take(&mut listing));
                for (ns, name, status) in std::mem::take(&mut statuses) {
                    cache.set_status(&ns, &name, status);
                }
                for (ns, name, status) in std::mem::take(&mut mode_statuses) {
                    cache.set_mode_status(&ns, &name, status);
                }
                for (ns, name, mock) in std::mem::take(&mut mocks) {
                    cache.set_mock(&ns, &name, mock);
                }
                tracing::info!("MimicWorkload cache synced");
            }
            Ok(Event::Apply(object)) => match entry(&object) {
                Some((ns, name, deployment, exclude, redact)) => {
                    cache.apply(&ns, &name, &deployment, exclude, redact);
                    cache.set_status(&ns, &name, held_status(&object));
                    cache.set_mode_status(&ns, &name, held_mode(&object));
                    cache.set_mock(&ns, &name, mock_of(&object));
                }
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

/// `status.discovery` of a `MimicWorkload`, as the cluster holds it.
fn held_status(object: &DynamicObject) -> Option<serde_json::Value> {
    object.data.get("status")?.get("discovery").cloned()
}

/// `status.mode` of a `MimicWorkload`, as the cluster holds it.
fn held_mode(object: &DynamicObject) -> Option<serde_json::Value> {
    object.data.get("status")?.get("mode").cloned()
}

/// `spec.mode` and `spec.mock` of a `MimicWorkload`; what the CRD would refuse reads as the
/// default.
pub fn mock_of(object: &DynamicObject) -> MockSettings {
    let Some(spec) = object.data.get("spec") else {
        return MockSettings::default();
    };
    let mock = spec.get("mock");
    MockSettings {
        mock: spec.get("mode").and_then(|m| m.as_str()) == Some("mock"),
        factor: mock
            .and_then(|m| m.get("latencyFactor"))
            .and_then(|f| f.as_f64())
            .and_then(hundredths)
            .unwrap_or(100),
        passthrough: mock
            .and_then(|m| m.get("passthrough"))
            .and_then(|list| list.as_array())
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.as_str().map(str::to_string))
            .collect(),
    }
}

/// `factor` (0 to 100) in hundredths, rounded half up on its shortest decimal form, so that
/// `1.005` gives 101 whatever its binary value.
fn hundredths(factor: f64) -> Option<u32> {
    if !(0.0..=100.0).contains(&factor) {
        return None;
    }
    let text = factor.to_string();
    let (whole, frac) = text.split_once('.').unwrap_or((&text, ""));
    let digit = |at: usize| frac.as_bytes().get(at).map_or(0, |d| u32::from(d - b'0'));
    let rounded = u32::from(digit(2) >= 5);
    Some(whole.parse::<u32>().ok()? * 100 + digit(0) * 10 + digit(1) + rounded)
}

fn entry(object: &DynamicObject) -> Option<Entry> {
    let spec = object.data.get("spec")?;
    let deployment = spec.get("deployment")?.as_str()?;
    let strings = |section: &str, field: &str| -> Vec<String> {
        spec.get(section)
            .and_then(|section| section.get(field))
            .and_then(|list| list.as_array())
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.as_str().map(str::to_string))
            .collect()
    };
    Some((
        object.metadata.namespace.clone()?,
        object.metadata.name.clone()?,
        deployment.to_string(),
        strings("tls", "exclude"),
        strings("discovery", "redact"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inj_08_cache_is_keyed_by_namespace_and_spec_deployment() {
        let cache = WorkloadCache::new();
        assert!(!cache.is_synced());
        cache.replace([("shop".into(), "w1".into(), "api".into(), vec![], vec![])]);
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
        cache.apply("shop", "w3", "web", vec![], vec![]);
        assert!(cache.has("shop", "web"));
        cache.apply("shop", "w3", "api", vec![], vec![]);
        assert!(!cache.has("shop", "web") && cache.has("shop", "api"));
    }

    #[test]
    fn inj_08_cache_replace_drops_vanished_workloads() {
        let cache = WorkloadCache::synced_with(&[("shop", "w1", "api")]);
        cache.replace([("shop".into(), "w2".into(), "web".into(), vec![], vec![])]);
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
    fn tls_28_the_exclusion_union_is_cut_to_256_entries_in_sorted_order() {
        let names: Vec<Vec<String>> = (0..3)
            .map(|w| {
                (0..100)
                    .map(|i| format!("h{:03}.test", w * 100 + i))
                    .collect()
            })
            .collect();
        let cache = WorkloadCache::new();
        for (i, list) in names.iter().enumerate() {
            cache.apply("shop", &format!("w{i}"), "api", list.clone(), vec![]);
        }
        let union = cache.exclude("shop", "api");
        assert_eq!(union.len(), 256);
        assert_eq!(union[0], "h000.test");
        assert_eq!(union[255], "h255.test");
    }

    #[test]
    fn dsc_20_redactions_are_the_sorted_deduplicated_union_per_deployment() {
        let cache = WorkloadCache::synced_with_settings(&[
            ("shop", "w1", "api", &[], &["x-tenant-key", "pin"]),
            ("shop", "w2", "api", &["a.test"], &["pin", "badge"]),
            ("shop", "w3", "web", &[], &["web-key"]),
            ("other", "w4", "api", &[], &["other-key"]),
        ]);
        assert_eq!(
            cache.redact("shop", "api"),
            ["badge", "pin", "x-tenant-key"]
        );
        assert_eq!(cache.exclude("shop", "api"), ["a.test"]);
        assert_eq!(cache.redact("shop", "web"), ["web-key"]);
        assert!(cache.redact("shop", "none").is_empty());
        cache.remove("shop", "w2");
        assert_eq!(cache.redact("shop", "api"), ["pin", "x-tenant-key"]);
    }

    #[test]
    fn dsc_20_redactions_are_lower_cased() {
        let cache = WorkloadCache::synced_with_settings(&[
            ("shop", "w1", "api", &[], &["X-Tenant-Key", "Pin"]),
            ("shop", "w2", "api", &[], &["pin", "BADGE"]),
        ]);
        assert_eq!(
            cache.redact("shop", "api"),
            ["badge", "pin", "x-tenant-key"]
        );
    }

    #[test]
    fn dsc_20_entry_reads_spec_discovery_redact() {
        let object: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "mimic.platform.io/v1alpha1", "kind": "MimicWorkload",
            "metadata": {"name": "w", "namespace": "shop"},
            "spec": {"deployment": "api", "discovery": {"redact": ["pin", 7, "x-tenant-key"]}},
        }))
        .unwrap();
        let (_, _, _, exclude, redact) = entry(&object).unwrap();
        assert!(exclude.is_empty());
        assert_eq!(redact, ["pin", "x-tenant-key"]);
    }

    #[test]
    fn tls_12_entry_reads_spec_tls_exclude() {
        let object: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "mimic.platform.io/v1alpha1", "kind": "MimicWorkload",
            "metadata": {"name": "w", "namespace": "shop"},
            "spec": {"deployment": "api", "tls": {"exclude": ["a.test", 7, "b.test"]}},
        }))
        .unwrap();
        let (ns, name, deployment, exclude, _) = entry(&object).unwrap();
        assert_eq!(
            (ns.as_str(), name.as_str(), deployment.as_str()),
            ("shop", "w", "api")
        );
        assert_eq!(exclude, ["a.test", "b.test"]);
    }

    fn mock(on: bool, factor: u32, passthrough: &[&str]) -> MockSettings {
        MockSettings {
            mock: on,
            factor,
            passthrough: passthrough.iter().map(|p| p.to_string()).collect(),
        }
    }

    #[test]
    fn mck_01_no_workload_or_only_discovery_gives_discovery_factor_1_no_passthrough() {
        let cache = WorkloadCache::synced_with(&[("shop", "w1", "api")]);
        let none = cache.effective("shop", "web");
        assert_eq!(
            (none.mode().as_str(), none.factor_text()),
            ("discovery", "1".into())
        );
        assert!(none.passthrough.is_empty());
        cache.set_mock("shop", "w1", mock(false, 100, &[]));
        let found = cache.effective("shop", "api");
        assert_eq!(found.mode().as_str(), "discovery");
        assert_eq!(found.factor, 100);
    }

    #[test]
    fn mck_01_one_mock_workload_makes_the_deployment_mock_with_the_first_factor_in_name_order() {
        let cache = WorkloadCache::synced_with(&[
            ("shop", "a", "api"),
            ("shop", "b", "api"),
            ("shop", "c", "api"),
            ("other", "a", "api"),
        ]);
        cache.set_mock("shop", "c", mock(true, 300, &[]));
        cache.set_mock("shop", "b", mock(true, 50, &[]));
        cache.set_mock("shop", "a", mock(false, 700, &[]));
        cache.set_mock("other", "a", mock(true, 900, &[]));
        let found = cache.effective("shop", "api");
        assert_eq!(found.mode().as_str(), "mock");
        assert_eq!(found.factor_text(), "0.5");
        // Another namespace never leaks in.
        assert_eq!(cache.effective("other", "api").factor, 900);
        cache.remove("shop", "b");
        assert_eq!(cache.effective("shop", "api").factor, 300);
        cache.remove("shop", "c");
        assert_eq!(cache.effective("shop", "api").mode().as_str(), "discovery");
    }

    #[test]
    fn mck_01_passthrough_is_the_sorted_deduplicated_union_cut_to_256() {
        let cache = WorkloadCache::synced_with(&[("shop", "a", "api"), ("shop", "b", "api")]);
        cache.set_mock("shop", "a", mock(false, 100, &["b.test", "a.test"]));
        cache.set_mock("shop", "b", mock(true, 100, &["b.test", "*.c.test"]));
        assert_eq!(
            cache.effective("shop", "api").passthrough,
            ["*.c.test", "a.test", "b.test"]
        );
        let many: Vec<String> = (0..200).map(|i| format!("h{i:03}.test")).collect();
        let more: Vec<String> = (100..300).map(|i| format!("h{i:03}.test")).collect();
        fn refs(v: &[String]) -> Vec<&str> {
            v.iter().map(String::as_str).collect()
        }
        cache.set_mock("shop", "a", mock(false, 100, &refs(&many)));
        cache.set_mock("shop", "b", mock(false, 100, &refs(&more)));
        let union = cache.effective("shop", "api").passthrough;
        assert_eq!(union.len(), 256);
        assert_eq!(
            (union[0].as_str(), union[255].as_str()),
            ("h000.test", "h255.test")
        );
    }

    #[test]
    fn mck_01_factor_text_is_the_shortest_decimal() {
        let text = |factor| {
            Effective {
                mock: true,
                factor,
                passthrough: Vec::new(),
            }
            .factor_text()
        };
        assert_eq!(
            [
                text(100),
                text(50),
                text(0),
                text(10_000),
                text(5),
                text(130),
                text(101)
            ],
            ["1", "0.5", "0", "100", "0.05", "1.3", "1.01"]
        );
    }

    #[test]
    fn mck_01_mock_of_reads_the_spec_and_rounds_the_factor_half_up() {
        let object = |spec: serde_json::Value| -> DynamicObject {
            serde_json::from_value(serde_json::json!({
                "apiVersion": "mimic.platform.io/v1alpha1", "kind": "MimicWorkload",
                "metadata": {"name": "w", "namespace": "shop"}, "spec": spec,
            }))
            .unwrap()
        };
        let read = |spec| mock_of(&object(spec));
        assert_eq!(
            read(serde_json::json!({"deployment": "api"})),
            MockSettings::default()
        );
        let full = read(serde_json::json!({"deployment": "api", "mode": "mock",
            "mock": {"latencyFactor": 0.5, "passthrough": ["a.test", 7, "b.test"]}}));
        assert_eq!(full, mock(true, 50, &["a.test", "b.test"]));
        let factor = |f: f64| {
            read(serde_json::json!({"deployment": "api", "mock": {"latencyFactor": f}})).factor
        };
        assert_eq!(
            [
                factor(0.005),
                factor(0.004),
                factor(1.005),
                factor(0.0),
                factor(100.0),
                factor(2.0)
            ],
            [1, 0, 101, 0, 10_000, 200]
        );
        // A value the CRD would refuse reads as the default.
        assert_eq!(factor(101.0), 100);
        assert_eq!(factor(-1.0), 100);
    }
}
