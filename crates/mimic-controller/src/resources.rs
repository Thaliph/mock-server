//! The mimic custom resources the explorer UI shows (spec §7.5, explorer shell): a cache kept
//! by watches, holding only what the UI may render (never `managedFields`, an owner, a
//! `last-applied` annotation or a Secret key), indexed by the deployment they configure.

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock};

use kube::api::DynamicObject;
use serde_json::Value;

/// The kinds the explorer links, in display order.
pub const KINDS: [&str; 3] = ["MimicWorkload", "MimicLoadTest", "MimicConfig"];

/// One resource as the UI may show it.
#[derive(Debug, Clone, PartialEq)]
pub struct Resource {
    pub kind: String,
    pub namespace: String,
    pub name: String,
    /// `spec.deployment`.
    pub deployment: String,
    /// `creationTimestamp`, `generation`, `labels` and `annotations`.
    pub metadata: Value,
    pub spec: Value,
    /// `status.conditions`, string fields only, sorted by `type`.
    pub conditions: Vec<Value>,
    /// `status.phase` of a `MimicLoadTest`.
    pub phase: Option<String>,
}

impl Resource {
    /// `None` for an object without a name, a namespace or a string `spec.deployment`.
    pub fn from_object(_kind: &str, _object: &DynamicObject) -> Option<Resource> {
        todo!()
    }

    /// The JSON of `GET /v1/workloads/<ns>/<dep>/resources`, YAML included.
    pub fn to_json(&self) -> Value {
        todo!()
    }
}

/// A deployment and the resources that name it.
#[derive(Debug, Clone, PartialEq)]
pub struct Overview {
    pub namespace: String,
    pub deployment: String,
    pub mode: &'static str,
    /// `(kind, name)` sorted by kind then name.
    pub resources: Vec<(String, String)>,
}

#[derive(Default)]
pub struct ResourceCache {
    items: RwLock<BTreeMap<(String, String, String), Resource>>,
}

impl ResourceCache {
    pub fn new() -> Arc<ResourceCache> {
        Arc::new(ResourceCache::default())
    }

    pub fn apply(&self, resource: Resource) {
        let key = (
            resource.kind.clone(),
            resource.namespace.clone(),
            resource.name.clone(),
        );
        self.items
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, resource);
    }

    pub fn remove(&self, kind: &str, namespace: &str, name: &str) {
        self.items
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(kind.into(), namespace.into(), name.into()));
    }

    /// Replaces every resource of `kind` after a (re)list.
    pub fn replace(&self, kind: &str, resources: Vec<Resource>) {
        let mut items = self.items.write().unwrap_or_else(PoisonError::into_inner);
        items.retain(|key, _| key.0 != kind);
        for resource in resources {
            let key = (
                resource.kind.clone(),
                resource.namespace.clone(),
                resource.name.clone(),
            );
            items.insert(key, resource);
        }
    }

    /// The resources of a deployment, by kind then name.
    pub fn of(&self, _namespace: &str, _deployment: &str) -> Vec<Resource> {
        todo!()
    }

    /// Every deployment a held resource names, by namespace then deployment.
    pub fn overview(&self) -> Vec<Overview> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn object(value: Value) -> DynamicObject {
        serde_json::from_value(value).expect("an object")
    }

    fn workload() -> DynamicObject {
        object(json!({
            "apiVersion": "mimic.platform.io/v1alpha1",
            "kind": "MimicWorkload",
            "metadata": {
                "name": "front", "namespace": "shop", "uid": "u-1", "generation": 3,
                "creationTimestamp": "2026-11-01T10:00:00Z",
                "labels": {"team": "pay"},
                "annotations": {
                    "kubectl.kubernetes.io/last-applied-configuration": "{\"secret\":\"canary\"}",
                    "note": "<img src=x onerror=alert(1)>"
                },
                "managedFields": [{"manager": "kubectl", "fieldsV1": {"f:spec": {}}}],
                "ownerReferences": [{"name": "o"}],
                "finalizers": ["f"]
            },
            "spec": {"deployment": "front", "mode": "mock"},
            "status": {"conditions": [
                {"type": "Ready", "status": "True", "reason": "Ok", "message": "<b>x</b>",
                 "lastTransitionTime": "2026-11-01T10:00:01Z", "observedGeneration": 3},
                {"type": "Active", "status": "False", "extra": {"nested": 1}}
            ]}
        }))
    }

    #[test]
    fn exp_02_the_sanitiser_drops_managed_fields_owners_and_last_applied() {
        let resource = Resource::from_object("MimicWorkload", &workload()).expect("held");
        assert_eq!(resource.deployment, "front");
        let json = resource.to_json();
        let text = json.to_string();
        for dropped in [
            "managedFields",
            "fieldsV1",
            "ownerReferences",
            "finalizers",
            "last-applied",
            "canary",
            "u-1",
        ] {
            assert!(!text.contains(dropped), "{dropped} is rendered");
        }
        assert_eq!(json["metadata"]["labels"], json!({"team": "pay"}));
        assert_eq!(
            json["metadata"]["annotations"],
            json!({"note": "<img src=x onerror=alert(1)>"})
        );
        assert_eq!(json["metadata"]["generation"], 3);
        assert_eq!(
            json["metadata"]["creationTimestamp"],
            "2026-11-01T10:00:00Z"
        );
        assert_eq!(json["spec"], json!({"deployment": "front", "mode": "mock"}));
        assert!(json["yaml"].as_str().unwrap().contains("mode: \"mock\""));
    }

    #[test]
    fn exp_02_conditions_keep_their_string_fields_sorted_by_type() {
        let resource = Resource::from_object("MimicWorkload", &workload()).unwrap();
        assert_eq!(
            resource.conditions,
            vec![
                json!({"type": "Active", "status": "False"}),
                json!({"type": "Ready", "status": "True", "reason": "Ok", "message": "<b>x</b>",
                       "lastTransitionTime": "2026-11-01T10:00:01Z"}),
            ]
        );
        assert!(resource.to_json().get("phase").is_none());
    }

    #[test]
    fn exp_02_a_load_test_shows_the_secret_name_and_field_names_only() {
        let test = object(json!({
            "apiVersion": "mimic.platform.io/v1alpha1",
            "kind": "MimicLoadTest",
            "metadata": {"name": "t", "namespace": "shop"},
            "spec": {
                "deployment": "pay", "duration": "5m",
                "credentials": {"secretName": "payment-load", "fields": [
                    {"field": "header/authorization", "key": "token"}]}
            },
            "status": {"phase": "Running"}
        }));
        let json = Resource::from_object("MimicLoadTest", &test)
            .unwrap()
            .to_json();
        assert_eq!(json["phase"], "Running");
        assert_eq!(
            json["spec"]["credentials"],
            json!({"secretName": "payment-load", "fields": [{"field": "header/authorization"}]})
        );
        assert!(!json.to_string().contains("token"));
        assert_eq!(json["spec"]["duration"], "5m");
    }

    #[test]
    fn exp_02_an_object_without_a_deployment_is_not_held() {
        let no_dep = object(json!({
            "apiVersion": "v1", "kind": "MimicWorkload",
            "metadata": {"name": "x", "namespace": "n"}, "spec": {}
        }));
        assert!(Resource::from_object("MimicWorkload", &no_dep).is_none());
        let numeric = object(json!({
            "apiVersion": "v1", "kind": "MimicWorkload",
            "metadata": {"name": "x", "namespace": "n"}, "spec": {"deployment": 3}
        }));
        assert!(Resource::from_object("MimicWorkload", &numeric).is_none());
    }

    fn held(kind: &str, ns: &str, name: &str, dep: &str, mode: &str) -> Resource {
        Resource::from_object(
            kind,
            &object(json!({
                "apiVersion": "v1", "kind": kind,
                "metadata": {"name": name, "namespace": ns},
                "spec": {"deployment": dep, "mode": mode}
            })),
        )
        .unwrap()
    }

    #[test]
    fn exp_03_the_cache_indexes_by_deployment_and_sorts() {
        let cache = ResourceCache::new();
        cache.replace(
            "MimicWorkload",
            vec![
                held("MimicWorkload", "shop", "b", "front", "discovery"),
                held("MimicWorkload", "shop", "a", "front", "mock"),
                held("MimicWorkload", "bank", "w", "pay", "discovery"),
            ],
        );
        cache.apply(held("MimicLoadTest", "shop", "t", "front", ""));
        let names: Vec<_> = cache
            .of("shop", "front")
            .iter()
            .map(|r| format!("{}/{}", r.kind, r.name))
            .collect();
        assert_eq!(
            names,
            ["MimicLoadTest/t", "MimicWorkload/a", "MimicWorkload/b"]
        );
        assert!(cache.of("shop", "none").is_empty());
        let overview: Vec<_> = cache
            .overview()
            .into_iter()
            .map(|o| (o.namespace, o.deployment, o.mode))
            .collect();
        assert_eq!(
            overview,
            [
                ("bank".to_string(), "pay".to_string(), "discovery"),
                ("shop".to_string(), "front".to_string(), "mock")
            ]
        );
        cache.remove("MimicWorkload", "shop", "a");
        assert_eq!(cache.overview()[1].mode, "discovery");
        cache.replace("MimicWorkload", vec![]);
        assert_eq!(cache.overview().len(), 1, "the load test still names front");
    }
}
