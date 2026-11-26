//! GW-02: `Gateway` and `GatewayClass` objects as the input structs of
//! `mimic_core::gateway::input`; GW-08: the gateway Deployments and Services as their `Seen`.

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

fn deployment(labels: Value) -> k8s_openapi::api::apps::v1::Deployment {
    serde_json::from_value(json!({
        "apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"namespace": "shop", "name": "web-mimic", "uid": "uid-d", "generation": 3,
                     "labels": labels,
                     "ownerReferences": [
                         {"apiVersion": "v1", "kind": "ConfigMap", "name": "c", "uid": "uid-c"},
                         {"apiVersion": "gateway.networking.k8s.io/v1", "kind": "Gateway",
                          "name": "web", "uid": "uid-web", "controller": true}]},
        "spec": {"replicas": 2, "selector": {"matchLabels": {"mimic.platform.io/gateway": "web"}},
                 "template": {"metadata": {"labels": {"mimic.platform.io/gateway": "web"}},
                              "spec": {"containers": [{"name": "gateway", "image": "i"}]}}},
        "status": {"observedGeneration": 2, "availableReplicas": 1, "replicas": 2,
                   "conditions": [{"type": "ReplicaFailure", "status": "True"},
                                  {"type": "Available", "status": "False"}]},
    }))
    .unwrap()
}

fn managed_labels() -> Value {
    json!({"app.kubernetes.io/managed-by": "mimic", "mimic.platform.io/gateway": "web"})
}

#[test]
fn gw_08_deployment_and_service_convert_to_their_seen_state() {
    use mimic_core::gateway::input::{DeploymentState, Ingress, ServiceState};

    let (key, seen) = convert::deployment(&deployment(managed_labels())).unwrap();
    assert_eq!(key, ("shop".to_string(), "web-mimic".to_string()));
    let expected = convert::Seen {
        uid: "uid-d".into(),
        gateway: "web".into(),
        managed: true,
        owner: Some("uid-web".into()),
        digest: seen.digest,
        state: DeploymentState {
            generation: 3,
            observed_generation: 2,
            available_replicas: 1,
            replica_failure: true,
        },
    };
    assert_eq!(seen, expected);

    // The digest: the same without a status change, another with a label change.
    let mut moved = deployment(managed_labels());
    moved.status = None;
    moved.metadata.resource_version = Some("99".into());
    let (_, still) = convert::deployment(&moved).unwrap();
    assert_eq!(still.digest, seen.digest);
    assert_eq!(still.state.available_replicas, 0);
    let mut labels = managed_labels();
    labels["team"] = json!("x");
    let (_, relabelled) = convert::deployment(&deployment(labels)).unwrap();
    assert_ne!(relabelled.digest, seen.digest);
    let mut scaled = deployment(managed_labels());
    scaled.spec.as_mut().unwrap().replicas = Some(3);
    assert_ne!(convert::deployment(&scaled).unwrap().1.digest, seen.digest);

    // Managed only with both labels.
    let (_, unmanaged) =
        convert::deployment(&deployment(json!({"mimic.platform.io/gateway": "web"}))).unwrap();
    assert!(!unmanaged.managed);
    assert!(mimic_core::gateway::managed_for(&managed_labels(), "web"));
    assert!(!mimic_core::gateway::managed_for(&managed_labels(), "shop"));
    assert!(!mimic_core::gateway::managed_for(
        &json!({"mimic.platform.io/gateway": "web"}),
        "web"
    ));

    let service = |spec: Value, status: Value| -> k8s_openapi::api::core::v1::Service {
        serde_json::from_value(json!({
            "apiVersion": "v1", "kind": "Service",
            "metadata": {"namespace": "shop", "name": "web-mimic", "uid": "uid-s",
                         "labels": managed_labels()},
            "spec": spec, "status": status,
        }))
        .unwrap()
    };
    let (_, dual) = convert::service(&service(
        json!({"clusterIPs": ["10.96.0.10", "fd00::10"]}),
        json!({}),
    ))
    .unwrap();
    assert_eq!(
        dual.state,
        ServiceState {
            service_type: "ClusterIP".into(),
            cluster_ips: vec!["10.96.0.10".into(), "fd00::10".into()],
            ingress: vec![],
        }
    );
    assert_eq!(dual.owner, None);
    let (_, balanced) = convert::service(&service(
        json!({"type": "LoadBalancer", "clusterIPs": ["None"]}),
        json!({"loadBalancer": {"ingress": [{"ip": "192.0.2.1"}, {"hostname": "lb.example.com"}]}}),
    ))
    .unwrap();
    assert_eq!(
        balanced.state,
        ServiceState {
            service_type: "LoadBalancer".into(),
            cluster_ips: vec![],
            ingress: vec![
                Ingress {
                    ip: Some("192.0.2.1".into()),
                    hostname: None
                },
                Ingress {
                    ip: None,
                    hostname: Some("lb.example.com".into())
                },
            ],
        }
    );
    assert_eq!(balanced.digest, {
        let again = service(
            json!({"type": "LoadBalancer", "clusterIPs": ["None"]}),
            json!({}),
        );
        convert::service(&again).unwrap().1.digest
    });
}

#[test]
fn gw_15_gateways_mimic_does_not_serve_convert_to_stubs() {
    // Served: class `mimic` in `shop`.
    let serves = |namespace: &str, class: &str| namespace == "shop" && class == "mimic";
    let (key, full) = convert::entry(&object(web()), serves).expect("a gateway");
    assert_eq!(key, ("shop".to_string(), "web".to_string()));
    let (_, want) = convert::gateway(&object(web())).unwrap();
    assert_eq!(full, convert::GatewayEntry::Full(want));

    let stub = |namespace: &str, class: &str| {
        convert::GatewayEntry::Stub(convert::Stub {
            namespace: namespace.into(),
            name: "web".into(),
            class: class.into(),
        })
    };
    // Another class, and the mimic class in a namespace that is not listed: the namespace, the
    // name and the class only, whatever the listeners, hostnames and status hold.
    let mut istio = web();
    istio["spec"]["gatewayClassName"] = json!("istio");
    let mut other = web();
    other["metadata"]["namespace"] = json!("other");
    let mut big = web();
    big["metadata"]["namespace"] = json!("other");
    big["spec"]["listeners"][0]["hostname"] = json!("x".repeat(70 * 1024));
    for (value, want) in [
        (istio, stub("shop", "istio")),
        (other, stub("other", "mimic")),
        (big, stub("other", "mimic")),
    ] {
        let (_, got) = convert::entry(&object(value), serves).expect("a gateway");
        assert_eq!(got, want);
        let debug = format!("{got:?}");
        assert!(
            !debug.contains("example.com") && !debug.contains("10.96.0.10"),
            "{debug}"
        );
    }

    // A spec change that keeps the class keeps the stub equal: no pass starts.
    let mut moved = web();
    moved["spec"]["gatewayClassName"] = json!("istio");
    moved["metadata"]["generation"] = json!(9);
    moved["spec"]["listeners"][0]["port"] = json!(9090);
    moved["status"] = json!({});
    assert_eq!(
        convert::entry(&object(moved), serves).map(|(_, e)| e),
        Some(stub("shop", "istio"))
    );
    let mut value = web();
    value["metadata"].as_object_mut().unwrap().remove("name");
    assert_eq!(convert::entry(&object(value), serves), None);
}
