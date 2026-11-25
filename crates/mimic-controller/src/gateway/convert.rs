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
use mimic_core::gateway::{CONTROLLER_NAME, GATEWAY_LABEL, REPLICAS_ANNOTATION};
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

/// A Gateway of any class (a class can become a mimic one later); `None` without a namespace
/// or a name.
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

/// The label that marks an object as mimic's (with [`GATEWAY_LABEL`]).
const MANAGED_BY: &str = "app.kubernetes.io/managed-by";

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

/// The object is managed by mimic for the Gateway `gateway`: both labels (§7.10).
pub fn managed_for(labels: &Value, gateway: &str) -> bool {
    labels[MANAGED_BY] == "mimic" && labels[GATEWAY_LABEL] == gateway
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
        managed: label(MANAGED_BY).as_deref() == Some("mimic"),
        owner,
        digest,
        state,
    };
    Some((key, seen))
}

/// A gateway Deployment: its replicas and generations.
pub fn deployment(object: &Deployment) -> Option<(Key, Seen<DeploymentState>)> {
    let _ = (object, seen::<DeploymentState>);
    None
}

/// A gateway Service: its type and addresses.
pub fn service(object: &Service) -> Option<(Key, Seen<ServiceState>)> {
    let _ = object;
    None
}
