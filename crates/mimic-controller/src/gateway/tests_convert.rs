//! GW-02: `Gateway` and `GatewayClass` objects as the input structs of
//! `mimic_core::gateway::input`.

use kube::api::DynamicObject;
use mimic_core::gateway::input::{
    Gateway, GatewayClass, Infrastructure, Listener, ObservedStatus, RouteKind,
};
use mimic_core::mesh::input::GATEWAY_GROUP;
use serde_json::{Value, json};

use super::convert;

const CANARY: &str = "canary-5be81f2d";

fn object(value: Value) -> DynamicObject {
    serde_json::from_value(value).unwrap()
}

fn condition(kind: &str) -> Value {
    json!({"type": kind, "status": "True", "reason": kind, "message": "",
           "observedGeneration": 4, "lastTransitionTime": "2026-11-20T10:00:00Z"})
}

fn web() -> Value {
    json!({
        "apiVersion": "gateway.networking.k8s.io/v1", "kind": "Gateway",
        "metadata": {"namespace": "shop", "name": "web", "uid": "uid-web", "generation": 4},
        "spec": {
            "gatewayClassName": "mimic",
            "listeners": [
                {"name": "http", "port": 80, "protocol": "HTTP", "hostname": "shop.example.com",
                 "allowedRoutes": {"namespaces": {"from": "All"},
                                   "kinds": [{"kind": "HTTPRoute"}]}},
                {"name": "alt", "port": 8080, "protocol": "HTTP"},
            ],
            "infrastructure": {"annotations": {"mimic.platform.io/replicas": "3",
                                               "example.com/x": CANARY}},
        },
        "status": {
            "conditions": [condition("Accepted")],
            "listeners": [{"name": "http", "attachedRoutes": 0}],
            "addresses": [{"type": "IPAddress", "value": "10.96.0.10"}],
        },
    })
}

#[test]
fn gw_02_a_gateway_converts_to_the_input_gateway() {
    let (key, gw) = convert::gateway(&object(web())).expect("a gateway");
    assert_eq!(key, ("shop".to_string(), "web".to_string()));
    let expected = Gateway {
        namespace: "shop".into(),
        name: "web".into(),
        uid: "uid-web".into(),
        generation: 4,
        class: "mimic".into(),
        listeners: vec![
            Listener {
                name: "http".into(),
                hostname: Some("shop.example.com".into()),
                port: 80,
                protocol: "HTTP".into(),
                allowed_from: Some("All".into()),
                kinds: Some(vec![RouteKind {
                    group: GATEWAY_GROUP.into(),
                    kind: "HTTPRoute".into(),
                }]),
                unknown_fields: vec![],
            },
            Listener {
                name: "alt".into(),
                hostname: None,
                port: 8080,
                protocol: "HTTP".into(),
                allowed_from: None,
                kinds: None,
                unknown_fields: vec![],
            },
        ],
        has_addresses: false,
        has_tls: false,
        allowed_listeners_from: None,
        infrastructure: Infrastructure {
            has_labels: false,
            annotation_keys: vec!["example.com/x".into(), "mimic.platform.io/replicas".into()],
            replicas: Some("3".into()),
            has_parameters_ref: false,
        },
        unknown_fields: vec![],
        oversize: false,
        observed: ObservedStatus {
            conditions: vec![condition("Accepted")],
            listeners: vec![json!({"name": "http", "attachedRoutes": 0})],
            addresses: vec![json!({"type": "IPAddress", "value": "10.96.0.10"})],
        },
    };
    assert_eq!(gw, expected);
    assert!(!format!("{gw:?}").contains(CANARY), "an annotation value");
}

#[test]
fn gw_02_the_other_spec_fields_and_the_defaults() {
    let mut value = web();
    value["metadata"] = json!({"namespace": "shop", "name": "bare"});
    value["spec"] = json!({
        "addresses": [{"type": "IPAddress", "value": "192.0.2.1"}],
        "tls": {"frontend": {}},
        "allowedListeners": {"namespaces": {"from": "Same"}},
        "infrastructure": {"labels": {"team": CANARY}, "parametersRef": {"kind": "X", "name": "y"}},
        "listeners": [{"name": "big", "port": 70000, "protocol": "HTTP"}, {}],
    });
    value.as_object_mut().unwrap().remove("status");
    let (_, gw) = convert::gateway(&object(value)).unwrap();
    assert_eq!(
        (gw.uid.as_str(), gw.generation, gw.class.as_str()),
        ("", 0, "")
    );
    assert!(gw.has_addresses && gw.has_tls);
    assert_eq!(gw.allowed_listeners_from.as_deref(), Some("Same"));
    assert_eq!(
        gw.infrastructure,
        Infrastructure {
            has_labels: true,
            annotation_keys: vec![],
            replicas: None,
            has_parameters_ref: true,
        }
    );
    assert_eq!(gw.listeners[0].port, 0, "out of range");
    assert_eq!(
        (
            gw.listeners[1].name.as_str(),
            gw.listeners[1].port,
            gw.listeners[1].protocol.as_str()
        ),
        ("", 0, "")
    );
    assert_eq!(gw.observed, ObservedStatus::default());
    assert!(!format!("{gw:?}").contains(CANARY), "a label value");
    // Empty addresses, a null tls and empty labels are not set.
    let mut value = web();
    value["spec"]["addresses"] = json!([]);
    value["spec"]["tls"] = Value::Null;
    value["spec"]["infrastructure"]["labels"] = json!({});
    let (_, gw) = convert::gateway(&object(value)).unwrap();
    assert!(!gw.has_addresses && !gw.has_tls && !gw.infrastructure.has_labels);
    assert!(gw.unknown_fields.is_empty(), "{:?}", gw.unknown_fields);
}

#[test]
fn gw_02_unknown_fields_are_kept_as_paths_only() {
    let mut value = web();
    let spec = &mut value["spec"];
    spec["defaultScope"] = json!(CANARY);
    spec["listeners"][1]["filters"] = json!([{"type": CANARY}]);
    spec["listeners"][0]["allowedRoutes"]["x"] = json!(CANARY);
    spec["infrastructure"]["y"] = json!(CANARY);
    spec["allowedListeners"] = json!({"namespaces": {"from": "None", "z": CANARY}});
    let (_, gw) = convert::gateway(&object(value)).unwrap();
    assert_eq!(
        gw.unknown_fields,
        [
            "spec.defaultScope",
            "spec.allowedListeners.namespaces.z",
            "spec.infrastructure.y"
        ]
    );
    assert_eq!(
        gw.listeners[0].unknown_fields,
        ["spec.listeners[0].allowedRoutes.x"]
    );
    assert_eq!(
        gw.listeners[1].unknown_fields,
        ["spec.listeners[1].filters"]
    );
    assert!(!format!("{gw:?}").contains(CANARY), "a value of spec");
}

#[test]
fn gw_02_unknown_fields_of_every_level() {
    let mut value = web();
    let spec = &mut value["spec"];
    spec["allowedListeners"] = json!({"namespaces": {"from": "None"}, "a": 1});
    spec["listeners"][0]["allowedRoutes"]["namespaces"]["b"] = json!(1);
    spec["listeners"][0]["allowedRoutes"]["kinds"][0]["c"] = json!(1);
    spec["listeners"][0]["tls"] = json!({"mode": "Terminate"});
    let (_, gw) = convert::gateway(&object(value)).unwrap();
    assert_eq!(gw.unknown_fields, ["spec.allowedListeners.a"]);
    assert_eq!(
        gw.listeners[0].unknown_fields,
        [
            "spec.listeners[0].allowedRoutes.namespaces.b",
            "spec.listeners[0].allowedRoutes.kinds[0].c"
        ],
        "a listener's tls is known"
    );
}

/// A Gateway whose `spec` serialises to exactly `size` bytes.
fn sized(size: usize) -> Value {
    let mut value = web();
    value["spec"]["tls"] = json!({"pad": ""});
    let base = value["spec"].to_string().len();
    value["spec"]["tls"]["pad"] = json!("p".repeat(size - base));
    assert_eq!(value["spec"].to_string().len(), size);
    value
}

#[test]
fn gw_02_a_spec_over_64_kib_converts_to_key_generation_and_class_only() {
    let (_, at) = convert::gateway(&object(sized(64 * 1024))).unwrap();
    assert!(!at.oversize);
    assert!(at.has_tls && at.listeners.len() == 2);
    let (key, over) = convert::gateway(&object(sized(64 * 1024 + 1))).unwrap();
    assert_eq!(key, ("shop".to_string(), "web".to_string()));
    assert!(over.oversize);
    assert_eq!(
        (over.uid.as_str(), over.generation, over.class.as_str()),
        ("uid-web", 4, "mimic")
    );
    assert!(over.listeners.is_empty());
    assert_eq!(over.infrastructure, Infrastructure::default());
    assert!(over.unknown_fields.is_empty());
    assert!(!over.has_tls && !over.has_addresses);
    assert_eq!(over.allowed_listeners_from, None);
    assert_eq!(over.observed.conditions, [condition("Accepted")]);
    assert_eq!(over.observed.addresses.len(), 1);
}

#[test]
fn gw_02_labels_annotations_and_managed_fields_are_not_read() {
    let plain = web();
    let mut decorated = web();
    decorated["metadata"]["labels"] = json!({"team": CANARY});
    decorated["metadata"]["annotations"] = json!({"note": CANARY});
    decorated["metadata"]["managedFields"] = json!([{"manager": CANARY, "operation": "Apply"}]);
    assert_eq!(
        convert::gateway(&object(plain)),
        convert::gateway(&object(decorated))
    );
}

#[test]
fn gw_02_an_object_without_namespace_or_name_is_skipped() {
    let mut value = web();
    value["metadata"]
        .as_object_mut()
        .unwrap()
        .remove("namespace");
    assert_eq!(convert::gateway(&object(value)), None);
    let mut value = web();
    value["metadata"].as_object_mut().unwrap().remove("name");
    assert_eq!(convert::gateway(&object(value)), None);
}

fn class(name: &str, controller: &str, parameters: bool) -> DynamicObject {
    let mut value = json!({
        "apiVersion": "gateway.networking.k8s.io/v1", "kind": "GatewayClass",
        "metadata": {"name": name, "generation": 2, "labels": {"team": CANARY}},
        "spec": {"controllerName": controller},
        "status": {"conditions": [condition("Accepted")]},
    });
    if parameters {
        value["spec"]["parametersRef"] = json!({"group": "", "kind": "ConfigMap", "name": "p"});
    }
    object(value)
}

#[test]
fn gw_02_only_mimic_classes_convert() {
    let mimic = mimic_core::gateway::CONTROLLER_NAME;
    assert_eq!(
        convert::class(&class("mimic", mimic, false)),
        Some((
            (String::new(), "mimic".to_string()),
            GatewayClass {
                name: "mimic".into(),
                generation: 2,
                has_parameters_ref: false,
                observed: vec![condition("Accepted")],
            }
        ))
    );
    let (_, with) = convert::class(&class("tuned", mimic, true)).unwrap();
    assert!(with.has_parameters_ref);
    assert_eq!(
        convert::class(&class("istio", "example.com/other", false)),
        None
    );
    let mut nameless = class("x", mimic, false);
    nameless.metadata.name = None;
    assert_eq!(convert::class(&nameless), None);
}
