//! MSH-27: the watched objects as the input structs of `mesh::input`.

use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::api::DynamicObject;
use mimic_core::mesh::input::{
    AbortSpec, AdmissionSpec, BreakerSpec, DelaySpec, Destination, GrantFrom, GrantTo, OutlierSpec,
    RetryBudgetSpec, Target, TargetKind, TargetPort,
};
use mimic_core::mesh::policy::validate;
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
    assert_eq!(
        (s.ports[0].name.as_deref(), s.ports[0].port),
        (Some("http"), 8080)
    );
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
            Target {
                kind: TargetKind::Deployment,
                name: "cart".into()
            },
            Target {
                kind: TargetKind::Service,
                name: "cart".into()
            },
        ]
    );
    assert_eq!(
        c.destinations,
        Some(vec![Destination {
            namespace: "risk".into(),
            name: "scoring".into()
        }])
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
    assert_eq!(
        (c.destinations, c.mesh_enabled, c.slow_start),
        (None, None, None)
    );
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
        [GrantFrom {
            group: "mimic.platform.io".into(),
            kind: "MimicConfig".into(),
            namespace: "shop".into()
        }]
    );
    assert_eq!(
        g.to,
        [
            GrantTo {
                group: String::new(),
                kind: "Service".into(),
                name: None
            },
            GrantTo {
                group: String::new(),
                kind: "Service".into(),
                name: Some("scoring".into())
            },
        ]
    );
    assert!(!format!("{g:?}").contains(CANARY));
}

fn service_config(spec: serde_json::Value) -> mimic_core::mesh::input::Config {
    let object = dynamic(json!({
        "apiVersion": "mimic.platform.io/v1alpha1", "kind": "MimicConfig",
        "metadata": {"namespace": "shop", "name": "r", "generation": 1,
                     "creationTimestamp": "2026-11-12T10:00:00Z"},
        "spec": spec
    }));
    convert::config(&object).expect("a config").1
}

#[test]
fn rty_46_retries_and_the_retry_budget_convert_and_absent_fields_are_none() {
    let target = json!([{"group": "", "kind": "Service", "name": "cart"}]);
    let c = service_config(json!({
        "targetRefs": target, "retries": 0,
        "retryBudget": {"percent": 50, "minPerSecond": 0, "window": "10s"}
    }));
    assert_eq!(c.retries, Some(0));
    assert_eq!(
        c.retry_budget,
        Some(RetryBudgetSpec {
            percent: Some(50),
            min_per_second: Some(0),
            window_seconds: Some(10)
        })
    );
    let c = service_config(json!({"targetRefs": target, "retryBudget": {"window": "60s"}}));
    assert_eq!(c.retries, None);
    assert_eq!(
        c.retry_budget,
        Some(RetryBudgetSpec {
            percent: None,
            min_per_second: None,
            window_seconds: Some(60)
        })
    );
    let c = service_config(json!({"targetRefs": target, "retries": 10}));
    assert_eq!((c.retries, c.retry_budget), (Some(10), None));
    let c = service_config(json!({"targetRefs": target}));
    assert_eq!((c.retries, c.retry_budget), (None, None));
}

#[test]
fn rty_46_a_window_that_does_not_parse_makes_the_config_invalid_and_never_panics() {
    let target = json!([{"group": "", "kind": "Service", "name": "cart"}]);
    for window in [
        json!("soon"),
        json!("10"),
        json!(10),
        json!("-1s"),
        json!("1e1s"),
    ] {
        let c = service_config(json!({"targetRefs": target, "retryBudget": {"window": window}}));
        assert!(
            validate(&c).is_err(),
            "window {window} must not validate: {c:?}"
        );
    }
    let c = service_config(json!({"targetRefs": target, "retries": "3"}));
    assert!(validate(&c).is_err());
    let c = service_config(json!({"targetRefs": target, "retries": -1}));
    assert!(validate(&c).is_err());
}

#[test]
fn cbr_03_outlier_detection_and_the_circuit_breaker_convert_and_absent_fields_are_none() {
    let target = json!([{"group": "", "kind": "Service", "name": "cart"}]);
    let c = service_config(json!({
        "targetRefs": target,
        "outlierDetection": {"consecutiveFailures": 3, "consecutive5xx": 0,
                             "baseEjectionTime": "30s", "maxEjectionPercent": 100},
        "circuitBreaker": {"maxRequests": 8, "maxConnections": 4}
    }));
    assert_eq!(
        c.outlier_detection,
        Some(OutlierSpec {
            consecutive_failures: Some(3),
            consecutive_5xx: Some(0),
            base_ejection_seconds: Some(30),
            max_ejection_percent: Some(100)
        })
    );
    assert_eq!(
        c.circuit_breaker,
        Some(BreakerSpec {
            max_requests: Some(8),
            max_connections: Some(4)
        })
    );
    assert!(validate(&c).is_ok());
    let c = service_config(json!({
        "targetRefs": target, "outlierDetection": {"baseEjectionTime": "300s"},
        "circuitBreaker": {}
    }));
    assert_eq!(
        c.outlier_detection,
        Some(OutlierSpec {
            consecutive_failures: None,
            consecutive_5xx: None,
            base_ejection_seconds: Some(300),
            max_ejection_percent: None
        })
    );
    assert_eq!(
        c.circuit_breaker,
        Some(BreakerSpec {
            max_requests: None,
            max_connections: None
        })
    );
    let c = service_config(json!({"targetRefs": target}));
    assert_eq!((c.outlier_detection, c.circuit_breaker), (None, None));
}

#[test]
fn cbr_03_a_base_ejection_time_that_does_not_parse_makes_the_config_invalid_and_never_panics() {
    let target = json!([{"group": "", "kind": "Service", "name": "cart"}]);
    for time in [
        json!("30"),
        json!("1m"),
        json!("-1s"),
        json!(30),
        json!("soon"),
        json!("1e1s"),
        json!(""),
    ] {
        let c = service_config(
            json!({"targetRefs": target, "outlierDetection": {"baseEjectionTime": time}}),
        );
        let err = validate(&c).expect_err("must not validate");
        assert!(err.contains("baseEjectionTime"), "{time}: {err}");
    }
    let c = service_config(json!({"targetRefs": target, "circuitBreaker": {"maxRequests": "8"}}));
    assert!(validate(&c).is_err());
    let c = service_config(
        json!({"targetRefs": target, "outlierDetection": {"consecutiveFailures": -1}}),
    );
    assert!(validate(&c).is_err());
}

fn fault_config(faults: serde_json::Value) -> mimic_core::mesh::input::Config {
    let target = json!([{"group": "", "kind": "Service", "name": "cart"}]);
    service_config(json!({"targetRefs": target, "faults": faults}))
}

#[test]
fn flt_03_faults_convert_durations_to_milliseconds_and_absent_members_are_none() {
    let c = fault_config(json!([
        {"match": {"path": {"type": "PathPrefix", "value": "/checkout"}, "method": "POST",
                   "headers": [{"name": "X-Trace", "value": "1"}],
                   "queryParams": [{"name": "q", "value": "2"}]},
         "abort": {"percent": 10, "status": 503},
         "delay": {"percent": 20, "fixed": "500ms"}},
        {"delay": {"percent": 1, "fixed": "1s"}},
        {"abort": {"percent": 0, "status": 599}, "delay": {"percent": 100, "fixed": "1m"}}
    ]));
    let faults = c.faults.clone().expect("faults");
    assert_eq!(faults.len(), 3);
    assert_eq!(
        faults[0].delay,
        Some(DelaySpec {
            percent: 20,
            fixed_ms: 500
        })
    );
    assert_eq!(
        faults[0].abort,
        Some(AbortSpec {
            percent: 10,
            status: 503
        })
    );
    let m = faults[0].r#match.clone().expect("match");
    assert_eq!(m.method.as_deref(), Some("POST"));
    assert_eq!(
        m.path.map(|p| (p.kind, p.value)),
        Some(("PathPrefix".into(), "/checkout".into()))
    );
    assert_eq!((m.headers.len(), m.query_params.len()), (1, 1));
    assert_eq!(
        faults[1].delay,
        Some(DelaySpec {
            percent: 1,
            fixed_ms: 1000
        })
    );
    assert_eq!(
        (faults[1].r#match.clone(), faults[1].abort.clone()),
        (None, None)
    );
    assert_eq!(
        faults[2].delay,
        Some(DelaySpec {
            percent: 100,
            fixed_ms: 60_000
        })
    );
    assert_eq!(validate(&c), Ok(()));
    let c =
        service_config(json!({"targetRefs": [{"group": "", "kind": "Service", "name": "cart"}]}));
    assert_eq!(c.faults, None);
}

#[test]
fn flt_03_a_fixed_that_does_not_parse_or_is_over_60s_makes_the_config_invalid_never_a_panic() {
    for fixed in [
        json!("500"),
        json!(500),
        json!("-1s"),
        json!("1.5s"),
        json!("soon"),
        json!(""),
        json!("61s"),
        json!("1h"),
        json!("0s"),
        json!("60001ms"),
    ] {
        let c = fault_config(json!([{"delay": {"percent": 1, "fixed": fixed}}]));
        let err = validate(&c).expect_err("must not validate");
        assert!(err.contains("faults[0].delay.fixed"), "{fixed}: {err}");
    }
    let c = fault_config(json!([{"delay": {"percent": 1}}]));
    assert!(validate(&c).is_err(), "a delay without fixed");
    let c = fault_config(json!([{"abort": {"status": 503}}]));
    assert!(validate(&c).is_err(), "an abort without percent");
    let c = fault_config(json!([{"abort": {"percent": "1", "status": -1}}]));
    assert!(validate(&c).is_err());
}

fn admission_config(admission: serde_json::Value) -> mimic_core::mesh::input::Config {
    service_config(json!({
        "targetRefs": [{"group": "apps", "kind": "Deployment", "name": "cart"}],
        "admission": admission
    }))
}

#[test]
fn adm_11_admission_converts_both_fields_and_is_none_when_absent() {
    for (concurrency, rps) in [(64, 200), (1, 1), (100_000, 1_000_000)] {
        let c = admission_config(json!({"concurrency": concurrency, "rps": rps}));
        assert_eq!(
            c.admission,
            Some(AdmissionSpec {
                concurrency: Some(concurrency),
                rps: Some(rps)
            })
        );
        assert_eq!(validate(&c), Ok(()));
    }
    let c = service_config(
        json!({"targetRefs": [{"group": "apps", "kind": "Deployment", "name": "cart"}]}),
    );
    assert_eq!(c.admission, None);
    assert_eq!(validate(&c), Ok(()));
}

#[test]
fn adm_11_a_value_the_api_server_did_not_check_makes_the_config_invalid_never_a_panic() {
    let values = [
        json!(0),
        json!(100_001),
        json!(-1),
        json!("64"),
        json!(1.5),
        json!(null),
        json!(10_000_000_000u64),
    ];
    for bad in &values {
        let c = admission_config(json!({"concurrency": bad, "rps": 200}));
        let err = validate(&c).expect_err("must not validate");
        assert!(err.contains("admission.concurrency"), "{bad}: {err}");
    }
    for bad in values.iter().filter(|v| **v != json!(100_001)) {
        let c = admission_config(json!({"concurrency": 64, "rps": bad}));
        let err = validate(&c).expect_err("must not validate");
        assert!(err.contains("admission.rps"), "{bad}: {err}");
    }
    let c = admission_config(json!({"concurrency": 64, "rps": 1_000_001}));
    assert!(
        validate(&c)
            .expect_err("rps over")
            .contains("admission.rps")
    );
    let c = admission_config(json!({"rps": 5}));
    assert!(
        validate(&c)
            .expect_err("no concurrency")
            .contains("admission.concurrency")
    );
    let c = admission_config(json!({"concurrency": 5}));
    assert!(validate(&c).expect_err("no rps").contains("admission.rps"));
    for shape in [json!({}), json!(5), json!([]), json!(null)] {
        let c = admission_config(shape.clone());
        let err = validate(&c).expect_err("must not validate");
        assert!(err.contains("admission."), "{shape}: {err}");
    }
    let c = service_config(json!({
        "targetRefs": [{"group": "", "kind": "Service", "name": "cart"}],
        "admission": {"concurrency": 64, "rps": 200}
    }));
    assert_eq!(
        validate(&c),
        Err("admission fits a Deployment target only".into())
    );
}
