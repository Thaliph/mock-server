//! MSH-27: the watched objects as the input structs of `mesh::input`.

use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::api::DynamicObject;
use mimic_core::mesh::input::{Destination, GrantFrom, GrantTo, Target, TargetKind, TargetPort};
use serde_json::json;

use super::convert;

const CANARY: &str = "canary-9d41c07e";

fn service() -> Service {
    serde_json::from_value(json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": {"namespace": "shop", "name": "cart",
                     "labels": {"team": CANARY}, "annotations": {"note": CANARY}},
        "spec": {"clusterIPs": ["10.96.0.7", "fd00::7"], "clusterIP": "10.96.0.7",
                 "selector": {"app": CANARY},
                 "ports": [{"name": "http", "port": 80, "targetPort": "web"},
                           {"port": 9090, "targetPort": 9091},
                           {"name": "dflt", "port": 7}]}
    }))
    .unwrap()
}

#[test]
fn msh_27_a_service_has_its_cluster_ips_and_ports_and_no_label_or_annotation() {
    let (key, s) = convert::service(&service()).expect("a service");
    assert_eq!(key, ("shop".to_string(), "cart".to_string()));
    assert_eq!((s.namespace.as_str(), s.name.as_str()), ("shop", "cart"));
    assert_eq!(s.cluster_ips, ["10.96.0.7", "fd00::7"]);
    assert_eq!(s.ports.len(), 3);
    assert_eq!(s.ports[0].name.as_deref(), Some("http"));
    assert_eq!(s.ports[0].target_port, TargetPort::Name("web".into()));
    assert_eq!(s.ports[1].name, None);
    assert_eq!(s.ports[1].target_port, TargetPort::Number(9091));
    // No targetPort: the port itself.
    assert_eq!(s.ports[2].target_port, TargetPort::Number(7));
    assert!(!format!("{s:?}").contains(CANARY));
}

#[test]
fn msh_27_a_headless_service_has_none_and_one_without_a_name_is_skipped() {
    let mut headless = service();
    headless.spec.as_mut().unwrap().cluster_ips = Some(vec!["None".into()]);
    let (_, s) = convert::service(&headless).unwrap();
    assert!(s.cluster_ips.is_empty());
    let mut unnamed = service();
    unnamed.metadata.name = None;
    assert!(convert::service(&unnamed).is_none());
}

fn slice() -> EndpointSlice {
    serde_json::from_value(json!({
        "apiVersion": "discovery.k8s.io/v1", "kind": "EndpointSlice",
        "metadata": {"namespace": "shop", "name": "cart-x7k2p", "generation": 41,
                     "labels": {"kubernetes.io/service-name": "cart", "team": CANARY},
                     "annotations": {"note": CANARY}},
        "addressType": "IPv4",
        "ports": [{"name": "http", "port": 8080, "protocol": "TCP"}, {"port": 9000}],
        "endpoints": [
            {"addresses": ["10.244.0.1"], "conditions": {"ready": true, "serving": true, "terminating": false},
             "zone": "a", "targetRef": {"kind": "Pod", "name": CANARY}},
            {"addresses": ["10.244.0.2"], "conditions": {"ready": false, "serving": true, "terminating": true}},
            {"addresses": ["10.244.0.3"]}
        ]
    }))
    .unwrap()
}

#[test]
fn msh_27_a_slice_has_its_service_generation_ports_and_endpoints() {
    let (key, s) = convert::slice(&slice()).expect("a slice");
    assert_eq!(key, ("shop".to_string(), "cart-x7k2p".to_string()));
    assert_eq!((s.service.as_str(), s.generation), ("cart", 41));
    assert_eq!(s.ports.len(), 2);
    assert_eq!((s.ports[0].name.as_deref(), s.ports[0].port), (Some("http"), 8080));
    assert_eq!((s.ports[1].name.as_deref(), s.ports[1].port), (None, 9000));
    assert_eq!(s.endpoints.len(), 3);
    assert!(s.endpoints[0].ready && s.endpoints[0].serving);
    assert_eq!(s.endpoints[0].zone.as_deref(), Some("a"));
    assert!(!s.endpoints[1].ready && s.endpoints[1].serving && s.endpoints[1].terminating);
    // `conditions.ready` missing is read as true (the API's own rule).
    assert!(s.endpoints[2].ready);
    assert!(!format!("{s:?}").contains(CANARY));
}

#[test]
fn msh_27_a_slice_without_the_service_label_or_of_another_address_family_kind_is_skipped() {
    let mut unlabelled = slice();
    unlabelled.metadata.labels = None;
    assert!(convert::slice(&unlabelled).is_none());
    let mut fqdn = slice();
    fqdn.address_type = "FQDN".into();
    assert!(convert::slice(&fqdn).is_none());
}

fn dynamic(value: serde_json::Value) -> DynamicObject {
    serde_json::from_value(value).unwrap()
}

#[test]
fn msh_27_a_mimicconfig_has_its_targets_destinations_switches_and_times() {
    let object = dynamic(json!({
        "apiVersion": "mimic.platform.io/v1alpha1", "kind": "MimicConfig",
        "metadata": {"namespace": "shop", "name": "cart-cfg", "generation": 3,
                     "creationTimestamp": "2026-11-12T10:00:00Z",
                     "labels": {"team": CANARY}, "annotations": {"note": CANARY}},
        "spec": {
            "targetRefs": [{"group": "apps", "kind": "Deployment", "name": "cart"},
                           {"group": "", "kind": "Service", "name": "cart"}],
            "destinations": [{"namespace": "risk", "name": "scoring"}],
            "mesh": {"enabled": false},
            "loadBalancer": {"slowStart": "45s"}}
    }));
    let (key, c) = convert::config(&object).expect("a config");
    assert_eq!(key, ("shop".to_string(), "cart-cfg".to_string()));
    assert_eq!(c.generation, 3);
    assert_eq!(c.created, 1_794_477_600);
    assert_eq!(
        c.targets,
        [
            Target { kind: TargetKind::Deployment, name: "cart".into() },
            Target { kind: TargetKind::Service, name: "cart".into() },
        ]
    );
    assert_eq!(
        c.destinations,
        Some(vec![Destination { namespace: "risk".into(), name: "scoring".into() }])
    );
    assert_eq!(c.mesh_enabled, Some(false));
    assert_eq!(c.slow_start, Some(45));
    assert!(!format!("{c:?}").contains(CANARY));
}

#[test]
fn msh_27_a_mimicconfig_that_sets_nothing_has_none_and_a_malformed_one_is_still_listed() {
    let bare = dynamic(json!({
        "apiVersion": "mimic.platform.io/v1alpha1", "kind": "MimicConfig",
        "metadata": {"namespace": "shop", "name": "bare", "generation": 1,
                     "creationTimestamp": "2026-11-12T10:00:00Z"},
        "spec": {"targetRefs": [{"group": "apps", "kind": "Deployment", "name": "cart"}]}
    }));
    let (_, c) = convert::config(&bare).unwrap();
    assert_eq!((c.destinations, c.mesh_enabled, c.slow_start), (None, None, None));
    // A spec the schema would refuse (the object may predate a stricter CRD): no target, so
    // `Invalid` in the statuses, never a panic and never skipped silently.
    let odd = dynamic(json!({
        "apiVersion": "mimic.platform.io/v1alpha1", "kind": "MimicConfig",
        "metadata": {"namespace": "shop", "name": "odd", "creationTimestamp": "2026-11-12T10:00:00Z"},
        "spec": {"targetRefs": "nope", "loadBalancer": {"slowStart": "soon"}}
    }));
    let (_, c) = convert::config(&odd).unwrap();
    assert!(c.targets.is_empty());
}

#[test]
fn msh_27_a_referencegrant_has_its_from_and_to() {
    let object = dynamic(json!({
        "apiVersion": "gateway.networking.k8s.io/v1beta1", "kind": "ReferenceGrant",
        "metadata": {"namespace": "risk", "name": "allow-shop", "labels": {"team": CANARY}},
        "spec": {
            "from": [{"group": "mimic.platform.io", "kind": "MimicConfig", "namespace": "shop"}],
            "to": [{"group": "", "kind": "Service"}, {"group": "", "kind": "Service", "name": "scoring"}]}
    }));
    let (key, g) = convert::grant(&object).expect("a grant");
    assert_eq!(key, ("risk".to_string(), "allow-shop".to_string()));
    assert_eq!(g.namespace, "risk");
    assert_eq!(
        g.from,
        [GrantFrom { group: "mimic.platform.io".into(), kind: "MimicConfig".into(), namespace: "shop".into() }]
    );
    assert_eq!(
        g.to,
        [
            GrantTo { group: String::new(), kind: "Service".into(), name: None },
            GrantTo { group: String::new(), kind: "Service".into(), name: Some("scoring".into()) },
        ]
    );
    assert!(!format!("{g:?}").contains(CANARY));
}
