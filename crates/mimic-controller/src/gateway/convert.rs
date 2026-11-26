//! `Gateway` and `GatewayClass` objects as the input structs of `mimic_core::gateway::input`
//! (GW-02). Only the fields §7.10 "What is read" names are read: no label, `metadata`
//! annotation, `managedFields` or annotation value (but the replicas one) leaves the object. A
//! field the converter does not know is kept as its path only.

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Service;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::DynamicObject;
use mimic_core::gateway::input::{
    DeploymentState, Gateway, GatewayClass, Infrastructure, Ingress, Listener, ObservedStatus,
    RouteKind, ServiceState,
};
use mimic_core::gateway::{
    CONTROLLER_NAME, GATEWAY_LABEL, MANAGED_BY, MANAGER, REPLICAS_ANNOTATION,
};
use mimic_core::mesh::input::GATEWAY_GROUP;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::mesh::convert::{Key, MAX_SPEC_BYTES, key, list, port, text, unknown};

/// The fields of each level of a Gateway the converter knows; any other is an unknown field.
const SPEC_FIELDS: [&str; 6] = [
    "gatewayClassName",
    "listeners",
    "addresses",
    "tls",
    "allowedListeners",
    "infrastructure",
];
const INFRASTRUCTURE_FIELDS: [&str; 3] = ["labels", "annotations", "parametersRef"];
/// `tls` is known and not read (#458).
const LISTENER_FIELDS: [&str; 6] = [
    "name",
    "hostname",
    "port",
    "protocol",
    "tls",
    "allowedRoutes",
];
const NAMESPACES_FIELDS: [&str; 2] = ["from", "selector"];

/// Present and not `null`.
fn set(value: &Value, field: &str) -> bool {
    value.get(field).is_some_and(|v| !v.is_null())
}

/// What the reconciler keeps of a Gateway (§7.10 "What is read").
#[derive(Debug, Clone, PartialEq)]
pub enum GatewayEntry {
    /// A Gateway of a mimic class in a listed namespace, converted in full (boxed: a stub is
    /// small).
    Full(Box<Gateway>),
    /// Any other Gateway: mimic does no work for it.
    Stub(Stub),
}

/// The namespace, the name and the class of a Gateway that mimic does not serve: no listener,
/// hostname or status of it is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stub {
    pub namespace: String,
    pub name: String,
    pub class: String,
}

impl GatewayEntry {
    pub fn class(&self) -> &str {
        match self {
            GatewayEntry::Full(gw) => &gw.class,
            GatewayEntry::Stub(stub) => &stub.class,
        }
    }

    pub fn full(&self) -> Option<&Gateway> {
        match self {
            GatewayEntry::Full(gw) => Some(gw.as_ref()),
            GatewayEntry::Stub(_) => None,
        }
    }
}

/// A Gateway as the reconciler keeps it: in full when `serves(namespace, class)`, else a stub
/// read from its key and `spec.gatewayClassName` only; `None` without a namespace or a name.
pub fn entry(
    object: &DynamicObject,
    serves: impl Fn(&str, &str) -> bool,
) -> Option<(Key, GatewayEntry)> {
    let key = key(&object.metadata)?;
    let spec = object.data.get("spec").unwrap_or(&Value::Null);
    let class = text(spec, "gatewayClassName").unwrap_or_default();
    if serves(&key.0, &class) {
        let (key, gw) = gateway(object)?;
        return Some((key, GatewayEntry::Full(Box::new(gw))));
    }
    let stub = Stub {
        namespace: key.0.clone(),
        name: key.1.clone(),
        class,
    };
    Some((key, GatewayEntry::Stub(stub)))
}

/// A Gateway converted in full; `None` without a namespace or a name.
pub fn gateway(object: &DynamicObject) -> Option<(Key, Gateway)> {
    let key = key(&object.metadata)?;
    let spec = object.data.get("spec").unwrap_or(&Value::Null);
    let status = object.data.get("status").unwrap_or(&Value::Null);
    let mut gw = Gateway {
        namespace: key.0.clone(),
        name: key.1.clone(),
        uid: object.metadata.uid.clone().unwrap_or_default(),
        generation: object.metadata.generation.unwrap_or(0),
        class: text(spec, "gatewayClassName").unwrap_or_default(),
        listeners: vec![],
        has_addresses: false,
        has_tls: false,
        allowed_listeners_from: None,
        infrastructure: Infrastructure::default(),
        unknown_fields: vec![],
        oversize: spec.to_string().len() > MAX_SPEC_BYTES,
        // Read also when oversize: the status writer compares with it.
        observed: ObservedStatus {
            conditions: list(status, "conditions").to_vec(),
            listeners: list(status, "listeners").to_vec(),
            addresses: list(status, "addresses").to_vec(),
        },
    };
    if gw.oversize {
        return Some((key, gw));
    }
    let out = &mut gw.unknown_fields;
    unknown(spec, "spec", &SPEC_FIELDS, out);
    gw.has_addresses = !list(spec, "addresses").is_empty();
    gw.has_tls = set(spec, "tls");
    if let Some(allowed) = spec.get("allowedListeners") {
        unknown(allowed, "spec.allowedListeners", &["namespaces"], out);
        if let Some(namespaces) = allowed.get("namespaces") {
            let at = "spec.allowedListeners.namespaces";
            unknown(namespaces, at, &NAMESPACES_FIELDS, out);
            gw.allowed_listeners_from = text(namespaces, "from");
        }
    }
    if let Some(infrastructure) = spec.get("infrastructure") {
        unknown(
            infrastructure,
            "spec.infrastructure",
            &INFRASTRUCTURE_FIELDS,
            out,
        );
        let annotations = infrastructure.get("annotations").and_then(Value::as_object);
        let mut annotation_keys: Vec<String> = annotations
            .into_iter()
            .flat_map(|a| a.keys().cloned())
            .collect();
        annotation_keys.sort();
        gw.infrastructure = Infrastructure {
            has_labels: infrastructure
                .get("labels")
                .and_then(Value::as_object)
                .is_some_and(|l| !l.is_empty()),
            annotation_keys,
            replicas: annotations
                .and_then(|a| a.get(REPLICAS_ANNOTATION))
                .and_then(Value::as_str)
                .map(str::to_string),
            has_parameters_ref: set(infrastructure, "parametersRef"),
        };
    }
    gw.listeners = list(spec, "listeners")
        .iter()
        .enumerate()
        .map(|(i, l)| listener(l, &format!("spec.listeners[{i}]")))
        .collect();
    Some((key, gw))
}

fn listener(value: &Value, at: &str) -> Listener {
    let mut unknown_fields = vec![];
    let out = &mut unknown_fields;
    unknown(value, at, &LISTENER_FIELDS, out);
    let mut allowed_from = None;
    let mut kinds = None;
    if let Some(routes) = value.get("allowedRoutes") {
        let at = format!("{at}.allowedRoutes");
        unknown(routes, &at, &["namespaces", "kinds"], out);
        if let Some(namespaces) = routes.get("namespaces") {
            unknown(
                namespaces,
                &format!("{at}.namespaces"),
                &NAMESPACES_FIELDS,
                out,
            );
            allowed_from = text(namespaces, "from");
        }
        kinds = routes.get("kinds").and_then(Value::as_array).map(|list| {
            list.iter()
                .enumerate()
                .map(|(j, k)| {
                    unknown(k, &format!("{at}.kinds[{j}]"), &["group", "kind"], out);
                    RouteKind {
                        group: text(k, "group").unwrap_or_else(|| GATEWAY_GROUP.into()),
                        kind: text(k, "kind").unwrap_or_default(),
                    }
                })
                .collect()
        });
    }
    Listener {
        name: text(value, "name").unwrap_or_default(),
        hostname: text(value, "hostname"),
        port: port(value).unwrap_or(0),
        protocol: text(value, "protocol").unwrap_or_default(),
        allowed_from,
        kinds,
        unknown_fields,
    }
}

/// A GatewayClass of mimic's `controllerName`, keyed `("", name)`; `None` for another
/// controller's, or without a name.
pub fn class(object: &DynamicObject) -> Option<(Key, GatewayClass)> {
    let spec = object.data.get("spec").unwrap_or(&Value::Null);
    if text(spec, "controllerName").as_deref() != Some(CONTROLLER_NAME) {
        return None;
    }
    let name = object.metadata.name.clone().filter(|n| !n.is_empty())?;
    let status = object.data.get("status").unwrap_or(&Value::Null);
    let class = GatewayClass {
        name: name.clone(),
        generation: object.metadata.generation.unwrap_or(0),
        has_parameters_ref: set(spec, "parametersRef"),
        observed: list(status, "conditions").to_vec(),
    };
    Some(((String::new(), name), class))
}

/// What the reconciler keeps of a watched gateway Deployment or Service: never the object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen<S> {
    pub uid: String,
    /// The value of `mimic.platform.io/gateway`.
    pub gateway: String,
    /// `app.kubernetes.io/managed-by` is `mimic`.
    pub managed: bool,
    /// The UID of the controller owner reference of kind `Gateway`.
    pub owner: Option<String>,
    /// [`digest`] of the labels, owner references and `spec`: a status change leaves it.
    pub digest: u64,
    pub state: S,
}

/// The first 8 bytes of the SHA-256 of `{labels, ownerReferences, spec}` of a typed object, so
/// that the watch and the answer of an apply give the same value.
pub fn digest(meta: &ObjectMeta, spec: &impl serde::Serialize) -> u64 {
    let parts = json!({
        "labels": meta.labels,
        "ownerReferences": meta.owner_references,
        "spec": spec,
    });
    let hash = Sha256::digest(parts.to_string().as_bytes());
    let mut first = [0; 8];
    first.copy_from_slice(&hash[..8]);
    u64::from_be_bytes(first)
}

fn seen<S>(meta: &ObjectMeta, digest: u64, state: S) -> Option<(Key, Seen<S>)> {
    let key = key(meta)?;
    let label = |name: &str| meta.labels.as_ref().and_then(|l| l.get(name)).cloned();
    let owner = meta
        .owner_references
        .iter()
        .flatten()
        .find(|o| o.controller == Some(true) && o.kind == "Gateway")
        .map(|o| o.uid.clone());
    let seen = Seen {
        uid: meta.uid.clone().unwrap_or_default(),
        gateway: label(GATEWAY_LABEL).unwrap_or_default(),
        managed: label(MANAGED_BY).as_deref() == Some(MANAGER),
        owner,
        digest,
        state,
    };
    Some((key, seen))
}

/// A gateway Deployment: its replicas and generations.
pub fn deployment(object: &Deployment) -> Option<(Key, Seen<DeploymentState>)> {
    let status = object.status.as_ref();
    let state = DeploymentState {
        generation: object.metadata.generation.unwrap_or(0),
        observed_generation: status.and_then(|s| s.observed_generation).unwrap_or(0),
        available_replicas: status
            .and_then(|s| s.available_replicas)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0),
        replica_failure: status
            .and_then(|s| s.conditions.as_ref())
            .into_iter()
            .flatten()
            .any(|c| c.type_ == "ReplicaFailure" && c.status == "True"),
    };
    seen(
        &object.metadata,
        digest(&object.metadata, &object.spec),
        state,
    )
}

/// A gateway Service: its type and addresses.
pub fn service(object: &Service) -> Option<(Key, Seen<ServiceState>)> {
    let spec = object.spec.as_ref();
    let state = ServiceState {
        service_type: spec
            .and_then(|s| s.type_.clone())
            .unwrap_or_else(|| "ClusterIP".into()),
        cluster_ips: spec
            .and_then(|s| s.cluster_ips.as_ref())
            .into_iter()
            .flatten()
            // `None` is how a headless Service says it has no address.
            .filter(|ip| *ip != "None")
            .cloned()
            .collect(),
        ingress: object
            .status
            .as_ref()
            .and_then(|s| s.load_balancer.as_ref())
            .and_then(|l| l.ingress.as_ref())
            .into_iter()
            .flatten()
            .map(|i| Ingress {
                ip: i.ip.clone(),
                hostname: i.hostname.clone(),
            })
            .collect(),
    };
    seen(
        &object.metadata,
        digest(&object.metadata, &object.spec),
        state,
    )
}
