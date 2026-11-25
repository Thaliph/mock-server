//! A fake API server for the gateway reconciler (spec §12.30): one store of objects that
//! records each call, answers an apply by replacing `labels`, `ownerReferences` and `spec`,
//! moves `resourceVersion` and `generation` only on a change, and fails the calls scripted.
//! `Rig` runs one `Reconciler` per replica on it, fed from the store through the real
//! converters. The input fixtures of the gateway tests are here too.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Service;
use kube::api::DynamicObject;
use mimic_core::gateway::input::{Gateway, GatewayClass, Infrastructure, Listener, ObjectKind};
use mimic_core::gateway::{GATEWAY_LABEL, ServiceType, Settings};
use prometheus_client::registry::Registry;
use serde_json::{Value, json};
use tokio::time::Instant;

use super::cluster::{ApiError, Cluster, ClusterFuture, StatusTarget};
use super::convert;
use super::metrics::GatewayMetrics;
use super::reconcile::Reconciler;
use crate::mesh::Event;
use crate::mesh::convert::Key;

/// The clock of every status the tests compute.
pub(super) const NOW: &str = "2026-11-25T10:00:00Z";

pub(super) fn settings(namespaces: &[&str]) -> Settings {
    Settings {
        namespaces: namespaces.iter().map(|n| n.to_string()).collect(),
        max_replicas: 10,
        service_type: ServiceType::ClusterIP,
        image: "ghcr.io/thaliph/mimic-sidecar@sha256:00".into(),
        log: "info".into(),
        resources: json!({"requests": {"cpu": "100m", "memory": "64Mi"},
                          "limits": {"memory": "128Mi"}}),
    }
}

pub(super) fn class(name: &str) -> GatewayClass {
    GatewayClass {
        name: name.into(),
        generation: 1,
        has_parameters_ref: false,
        observed: vec![],
    }
}

pub(super) fn listener(name: &str, port: u16, protocol: &str) -> Listener {
    Listener {
        name: name.into(),
        hostname: None,
        port,
        protocol: protocol.into(),
        allowed_from: None,
        kinds: None,
        unknown_fields: vec![],
    }
}

pub(super) fn gateway(namespace: &str, name: &str, listeners: Vec<Listener>) -> Gateway {
    Gateway {
        namespace: namespace.into(),
        name: name.into(),
        uid: format!("uid-{name}"),
        generation: 1,
        class: "mimic".into(),
        listeners,
        has_addresses: false,
        has_tls: false,
        allowed_listeners_from: None,
        infrastructure: Infrastructure::default(),
        unknown_fields: vec![],
        oversize: false,
        observed: Default::default(),
    }
}

pub(super) fn key(namespace: &str, name: &str) -> Key {
    (namespace.to_string(), name.to_string())
}

/// `(kind, namespace, name)`; `namespace` is empty for a GatewayClass.
pub(super) type Id = (String, String, String);

/// One request: `apply Deployment shop/web-mimic`, `get …`, `delete …`, `status Gateway
/// shop/web`, `status GatewayClass mimic`.
#[derive(Debug, Clone)]
pub(super) struct Request {
    pub what: String,
    pub body: Value,
    pub ok: bool,
    pub at: Instant,
}

#[derive(Default)]
pub(super) struct FakeApi {
    pub objects: Mutex<BTreeMap<Id, Value>>,
    pub calls: Mutex<Vec<Request>>,
    /// The next call whose `what` starts with the prefix fails with the error.
    pub errors: Mutex<Vec<(String, ApiError)>>,
    /// Set by [`FakeApi::fail_all`].
    always: Mutex<Option<ApiError>>,
    uids: Mutex<u64>,
    /// Service addresses given so far.
    addresses: Mutex<u64>,
}

fn id(kind: &str, namespace: &str, name: &str) -> Id {
    (kind.to_string(), namespace.to_string(), name.to_string())
}

fn version(object: &Value) -> u64 {
    object["metadata"]["resourceVersion"]
        .as_str()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Moves the `resourceVersion` (and the `generation` when `spec` changed) of `object`.
fn bump(object: &mut Value, spec_changed: bool) {
    let next = version(object) + 1;
    object["metadata"]["resourceVersion"] = json!(next.to_string());
    if spec_changed {
        let generation = object["metadata"]["generation"].as_i64().unwrap_or(0) + 1;
        object["metadata"]["generation"] = json!(generation);
    }
}

impl FakeApi {
    pub fn fail(&self, prefix: &str, reason: &str, code: u16) {
        self.errors.lock().unwrap().push((
            prefix.to_string(),
            ApiError {
                reason: reason.into(),
                code,
            },
        ));
    }

    /// Every call fails with `reason` and `code` from now on.
    pub fn fail_all(&self, reason: &str, code: u16) {
        *self.always.lock().unwrap() = Some(ApiError {
            reason: reason.into(),
            code,
        });
    }

    pub fn calls(&self) -> Vec<Request> {
        self.calls.lock().unwrap().clone()
    }

    /// The `what` of each call, in order.
    pub fn whats(&self) -> Vec<String> {
        self.calls().into_iter().map(|c| c.what).collect()
    }

    pub fn clear(&self) {
        self.calls.lock().unwrap().clear();
    }

    pub fn get(&self, kind: &str, namespace: &str, name: &str) -> Option<Value> {
        self.objects
            .lock()
            .unwrap()
            .get(&id(kind, namespace, name))
            .cloned()
    }

    /// The `resourceVersion` of every object.
    pub fn versions(&self) -> BTreeMap<Id, u64> {
        self.objects
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), version(v)))
            .collect()
    }

    fn uid(&self) -> String {
        let mut n = self.uids.lock().unwrap();
        *n += 1;
        format!("uid-{n}")
    }

    /// Puts an object as a user would create it; the server sets the UID, the versions and a
    /// Service's cluster IPs.
    pub fn put(&self, kind: &str, mut object: Value) {
        let namespace = object["metadata"]["namespace"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let name = object["metadata"]["name"].as_str().unwrap().to_string();
        object["apiVersion"] = json!(match kind {
            "Deployment" => "apps/v1",
            "Service" => "v1",
            "PodDisruptionBudget" => "policy/v1",
            _ => "gateway.networking.k8s.io/v1",
        });
        object["kind"] = json!(kind);
        if object["metadata"]["uid"].is_null() {
            object["metadata"]["uid"] = json!(self.uid());
        }
        object["metadata"]["resourceVersion"] = json!("1");
        if object["metadata"]["generation"].is_null() {
            object["metadata"]["generation"] = json!(1);
        }
        if kind == "Service" && object["spec"]["clusterIPs"].is_null() {
            let n = {
                let mut given = self.addresses.lock().unwrap();
                *given += 1;
                *given + 9
            };
            object["spec"]["clusterIP"] = json!(format!("10.96.0.{n}"));
            object["spec"]["clusterIPs"] = json!([format!("10.96.0.{n}")]);
        }
        self.objects
            .lock()
            .unwrap()
            .insert(id(kind, &namespace, &name), object);
    }

    /// Changes an object by hand: `change` edits it, the versions move as the server's would.
    pub fn edit(&self, kind: &str, namespace: &str, name: &str, change: impl FnOnce(&mut Value)) {
        let mut objects = self.objects.lock().unwrap();
        let object = objects
            .get_mut(&id(kind, namespace, name))
            .expect("an object");
        let before = object.clone();
        change(object);
        if *object != before {
            let spec = object["spec"] != before["spec"];
            bump(object, spec);
        }
    }

    pub fn remove(&self, kind: &str, namespace: &str, name: &str) {
        self.objects
            .lock()
            .unwrap()
            .remove(&id(kind, namespace, name));
    }

    /// Records the call, and fails it when scripted.
    fn call(&self, what: String, body: &Value) -> Result<(), ApiError> {
        let failure = {
            let mut errors = self.errors.lock().unwrap();
            let index = errors
                .iter()
                .position(|(p, _)| what.starts_with(p.as_str()));
            index
                .map(|i| errors.remove(i).1)
                .or_else(|| self.always.lock().unwrap().clone())
        };
        self.calls.lock().unwrap().push(Request {
            what,
            body: body.clone(),
            ok: failure.is_none(),
            at: Instant::now(),
        });
        failure.map_or(Ok(()), Err)
    }

    fn apply(&self, kind: &str, namespace: &str, name: &str, body: &Value) -> Value {
        let mut objects = self.objects.lock().unwrap();
        let key = id(kind, namespace, name);
        if !objects.contains_key(&key) {
            drop(objects);
            let mut created = body.clone();
            created["metadata"]["generation"] = json!(1);
            self.put(kind, created);
            return self.get(kind, namespace, name).unwrap();
        }
        let object = objects.get_mut(&key).unwrap();
        let before = object.clone();
        object["metadata"]["labels"] = body["metadata"]["labels"].clone();
        object["metadata"]["ownerReferences"] = body["metadata"]["ownerReferences"].clone();
        let mut spec = body["spec"].clone();
        // Set by the API server, not by the apply.
        for field in ["clusterIP", "clusterIPs"] {
            if !before["spec"][field].is_null() {
                spec[field] = before["spec"][field].clone();
            }
        }
        object["spec"] = spec;
        if *object != before {
            let spec = object["spec"] != before["spec"];
            bump(object, spec);
        }
        object.clone()
    }
}

pub(super) struct Replica {
    pub api: Arc<FakeApi>,
}

impl Cluster for Replica {
    fn apply<'a>(
        &'a self,
        kind: ObjectKind,
        namespace: &'a str,
        name: &'a str,
        body: &'a Value,
    ) -> ClusterFuture<'a, Value> {
        Box::pin(async move {
            let what = format!("apply {} {namespace}/{name}", kind.as_str());
            self.api.call(what, body)?;
            Ok(self.api.apply(kind.as_str(), namespace, name, body))
        })
    }

    fn get<'a>(
        &'a self,
        kind: ObjectKind,
        namespace: &'a str,
        name: &'a str,
    ) -> ClusterFuture<'a, Option<Value>> {
        Box::pin(async move {
            let what = format!("get {} {namespace}/{name}", kind.as_str());
            self.api.call(what, &Value::Null)?;
            Ok(self.api.get(kind.as_str(), namespace, name))
        })
    }

    fn delete<'a>(
        &'a self,
        kind: ObjectKind,
        namespace: &'a str,
        name: &'a str,
        uid: &'a str,
    ) -> ClusterFuture<'a, ()> {
        Box::pin(async move {
            let what = format!("delete {} {namespace}/{name} {uid}", kind.as_str());
            self.api.call(what, &Value::Null)?;
            let held = self.api.get(kind.as_str(), namespace, name);
            if held.is_some_and(|o| o["metadata"]["uid"] == uid) {
                self.api.remove(kind.as_str(), namespace, name);
            }
            Ok(())
        })
    }

    fn apply_status<'a>(
        &'a self,
        target: StatusTarget<'a>,
        body: &'a Value,
    ) -> ClusterFuture<'a, ()> {
        Box::pin(async move {
            let (kind, namespace, name) = match target {
                StatusTarget::Gateway { namespace, name } => ("Gateway", namespace, name),
                StatusTarget::Class { name } => ("GatewayClass", "", name),
            };
            let what = if namespace.is_empty() {
                format!("status {kind} {name}")
            } else {
                format!("status {kind} {namespace}/{name}")
            };
            self.api.call(what, body)?;
            // An object the store does not hold (another API server feeds the watches): only
            // recorded.
            if self.api.get(kind, namespace, name).is_some() {
                self.api.edit(kind, namespace, name, |o| {
                    o["status"] = body["status"].clone();
                });
            }
            Ok(())
        })
    }
}

/// The relist of one kind into a table.
fn relist<T>(items: Vec<(Key, T)>, feed: impl Fn(Event<T>)) {
    feed(Event::Init);
    for (key, value) in items {
        feed(Event::InitApply(key, value));
    }
    feed(Event::InitDone);
}

pub(super) struct Rig {
    pub api: Arc<FakeApi>,
    pub replicas: Vec<Arc<Reconciler>>,
    pub metrics: Vec<GatewayMetrics>,
    pub registry: Registry,
}

impl Rig {
    /// `n` replicas with `settings`, on an empty store.
    pub fn new(n: usize, settings: &Settings) -> Rig {
        let api = Arc::new(FakeApi::default());
        let mut registry = Registry::with_prefix("mimic");
        let mut metrics = vec![];
        let mut replicas = vec![];
        for id in 0..n {
            let m = if id == 0 {
                GatewayMetrics::register(&mut registry)
            } else {
                GatewayMetrics::register(&mut Registry::default())
            };
            let cluster = Arc::new(Replica { api: api.clone() });
            replicas.push(Reconciler::new(settings.clone(), cluster, m.clone()));
            metrics.push(m);
        }
        Rig {
            api,
            replicas,
            metrics,
            registry,
        }
    }

    fn of(&self, kind: &str) -> Vec<Value> {
        self.api
            .objects
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| id.0 == kind)
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// Replica `i` lists what the store holds now: the CRD, the classes, the Gateways, and the
    /// Deployments and Services with the gateway label.
    pub fn read(&self, i: usize) {
        self.list(i, true);
    }

    /// As `read`, the Services list not complete.
    pub fn read_partial(&self, i: usize) {
        self.list(i, false);
    }

    fn list(&self, i: usize, services_done: bool) {
        let r = &self.replicas[i];
        r.crd(true);
        let dynamic = |v: Value| serde_json::from_value::<DynamicObject>(v).unwrap();
        let labelled = |v: &Value| !v["metadata"]["labels"][GATEWAY_LABEL].is_null();
        relist(
            self.of("GatewayClass")
                .into_iter()
                .filter_map(|v| convert::class(&dynamic(v)))
                .collect(),
            |e| r.feed_classes(e),
        );
        relist(
            self.of("Gateway")
                .into_iter()
                .filter_map(|v| convert::gateway(&dynamic(v)))
                .collect(),
            |e| r.feed_gateways(e),
        );
        relist(
            self.of("Deployment")
                .into_iter()
                .filter(labelled)
                .filter_map(|v| {
                    convert::deployment(&serde_json::from_value::<Deployment>(v).unwrap())
                })
                .collect(),
            |e| r.feed_deployments(e),
        );
        let services: Vec<_> = self
            .of("Service")
            .into_iter()
            .filter(labelled)
            .filter_map(|v| convert::service(&serde_json::from_value::<Service>(v).unwrap()))
            .collect();
        if services_done {
            relist(services, |e| r.feed_services(e));
        } else {
            r.feed_services(Event::Init);
            for (key, value) in services {
                r.feed_services(Event::InitApply(key, value));
            }
        }
    }

    pub async fn pass(&self, i: usize) {
        self.replicas[i].pass(NOW).await;
    }

    /// `read` then `pass` of replica `i`.
    pub async fn round(&self, i: usize) {
        self.read(i);
        self.pass(i).await;
    }
}
