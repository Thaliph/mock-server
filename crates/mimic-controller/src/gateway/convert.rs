//! `Gateway` and `GatewayClass` objects as the input structs of `mimic_core::gateway::input`
//! (GW-02). Only the fields §7.10 "What is read" names are read: no label, `metadata`
//! annotation, `managedFields` or annotation value (but the replicas one) leaves the object. A
//! field the converter does not know is kept as its path only.

use kube::api::DynamicObject;
use mimic_core::gateway::input::{
    Gateway, GatewayClass, Infrastructure, Listener, ObservedStatus, RouteKind,
};
use mimic_core::gateway::{CONTROLLER_NAME, REPLICAS_ANNOTATION};
use mimic_core::mesh::input::GATEWAY_GROUP;
use serde_json::Value;

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
