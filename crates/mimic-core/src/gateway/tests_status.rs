//! GW-08 and GW-09: the status of a Gateway and of a GatewayClass (spec §7.10 "Status").

use serde_json::{Value, json};

use super::accept::{Acceptance, gateway};
use super::input::*;
use super::status::{self, Plan, programmed};
use super::testkit::*;
use crate::condition::Condition;

const T0: &str = "2026-01-01T00:00:00Z";
const T1: &str = "2026-11-24T10:00:00Z";

fn cond(ok: bool, reason: &str, message: &str) -> Condition {
    Condition {
        ok,
        reason: reason.into(),
        message: message.into(),
    }
}

fn accept(g: &Gateway) -> Acceptance {
    gateway(g, &class(), &settings())
}

fn deployment(generation: i64, observed_generation: i64, available: u32) -> DeploymentState {
    DeploymentState {
        generation,
        observed_generation,
        available_replicas: available,
        replica_failure: false,
    }
}

fn service(kind: &str, cluster_ips: &[&str], ingress: Vec<Ingress>) -> ServiceState {
    ServiceState {
        service_type: kind.into(),
        cluster_ips: cluster_ips.iter().map(|s| s.to_string()).collect(),
        ingress,
    }
}

fn ip(v: &str) -> Ingress {
    Ingress {
        ip: Some(v.into()),
        hostname: None,
    }
}

fn host(v: &str) -> Ingress {
    Ingress {
        ip: None,
        hostname: Some(v.into()),
    }
}

/// One of two replicas available, a ClusterIP Service with `10.96.0.10`.
fn ready() -> Observed {
    Observed {
        deployment: Some(deployment(1, 1, 1)),
        service: Some(service("ClusterIP", &["10.96.0.10"], vec![])),
        unmanaged: None,
    }
}

fn write(g: &Gateway, observed: &Observed, now: &str) -> Value {
    match status::gateway(g, &accept(g), observed, now) {
        Plan::Write(body) => body,
        Plan::Keep => panic!("expected a write"),
    }
}

/// `body.status` as the cluster would hold it.
fn held(body: &Value) -> ObservedStatus {
    let list = |k: &str| body["status"][k].as_array().cloned().unwrap_or_default();
    ObservedStatus {
        conditions: list("conditions"),
        listeners: list("listeners"),
        addresses: list("addresses"),
    }
}

fn find<'a>(conditions: &'a Value, kind: &str) -> &'a Value {
    conditions
        .as_array()
        .and_then(|list| list.iter().find(|c| c["type"] == kind))
        .unwrap_or(&Value::Null)
}

#[test]
fn gw_08_programmed_cases() {
    let refused = {
        let mut g = gw();
        g.has_tls = true;
        accept(&g)
    };
    let ok = accept(&gw());
    let failing = |available| Observed {
        deployment: Some(DeploymentState {
            replica_failure: true,
            ..deployment(1, 1, available)
        }),
        ..ready()
    };
    let cases: Vec<(&str, &Acceptance, Observed, Condition)> = vec![
        (
            "not accepted",
            &refused,
            ready(),
            cond(false, "Invalid", "not accepted"),
        ),
        (
            "unmanaged",
            &ok,
            Observed {
                unmanaged: Some((ObjectKind::Deployment, "web-mimic".into())),
                deployment: None,
                ..ready()
            },
            cond(
                false,
                "Invalid",
                "Deployment web-mimic exists and is not managed by mimic",
            ),
        ),
        (
            "unmanaged Service",
            &ok,
            Observed {
                unmanaged: Some((ObjectKind::Service, "web-mimic".into())),
                ..ready()
            },
            cond(
                false,
                "Invalid",
                "Service web-mimic exists and is not managed by mimic",
            ),
        ),
        (
            "no Deployment",
            &ok,
            Observed {
                deployment: None,
                ..ready()
            },
            cond(false, "Pending", "0 of 2 replicas available"),
        ),
        (
            "observedGeneration behind",
            &ok,
            Observed {
                deployment: Some(deployment(2, 1, 2)),
                ..ready()
            },
            cond(false, "Pending", "2 of 2 replicas available"),
        ),
        (
            "0 available",
            &ok,
            Observed {
                deployment: Some(deployment(1, 1, 0)),
                ..ready()
            },
            cond(false, "Pending", "0 of 2 replicas available"),
        ),
        (
            "replica failure, 0 available",
            &ok,
            failing(0),
            cond(false, "Pending", "0 of 2 replicas available"),
        ),
        (
            "replica failure",
            &ok,
            failing(1),
            cond(false, "NoResources", "replica creation failed"),
        ),
        (
            "LoadBalancer without ingress",
            &ok,
            Observed {
                service: Some(service("LoadBalancer", &["10.96.0.10"], vec![])),
                ..ready()
            },
            cond(false, "AddressNotAssigned", "no address assigned"),
        ),
        (
            "no Service",
            &ok,
            Observed {
                service: None,
                ..ready()
            },
            cond(false, "AddressNotAssigned", "no address assigned"),
        ),
        (
            "no clusterIPs",
            &ok,
            Observed {
                service: Some(service("ClusterIP", &[], vec![])),
                ..ready()
            },
            cond(false, "AddressNotAssigned", "no address assigned"),
        ),
        (
            "programmed",
            &ok,
            ready(),
            cond(true, "Programmed", "1 of 2 replicas available"),
        ),
    ];
    for (case, acceptance, observed, want) in cases {
        assert_eq!(programmed(acceptance, &observed), want, "case {case}");
    }
}

#[test]
fn gw_08_addresses() {
    let addresses = |svc: ServiceState| {
        let observed = Observed {
            service: Some(svc),
            ..ready()
        };
        write(&gw(), &observed, T0)["status"]["addresses"].clone()
    };
    let ipv4 = json!({"type": "IPAddress", "value": "10.96.0.10"});
    let ipv6 = json!({"type": "IPAddress", "value": "fd00::a"});
    for kind in ["ClusterIP", "NodePort"] {
        assert_eq!(
            addresses(service(kind, &["10.96.0.10", "fd00::a"], vec![])),
            json!([ipv4, ipv6]),
            "{kind}"
        );
    }
    assert_eq!(
        addresses(service(
            "LoadBalancer",
            &["10.96.0.10"],
            vec![ip("1.2.3.4"), host("lb.example"), ip("5.6.7.8")]
        )),
        json!([
            {"type": "IPAddress", "value": "1.2.3.4"},
            {"type": "IPAddress", "value": "5.6.7.8"},
            {"type": "Hostname", "value": "lb.example"},
        ])
    );
    let many: Vec<String> = (0..20).map(|i| format!("10.0.0.{i}")).collect();
    let refs: Vec<&str> = many.iter().map(String::as_str).collect();
    let want: Vec<Value> = many[..16]
        .iter()
        .map(|v| json!({"type": "IPAddress", "value": v}))
        .collect();
    assert_eq!(addresses(service("ClusterIP", &refs, vec![])), json!(want));
    let lb: Vec<Ingress> = many.iter().map(|v| ip(v)).collect();
    assert_eq!(addresses(service("LoadBalancer", &[], lb)), json!(want));
    let long = "h".repeat(254);
    assert_eq!(
        addresses(service(
            "LoadBalancer",
            &[],
            vec![ip(""), host(&long), host("lb.example")]
        )),
        json!([{"type": "Hostname", "value": "lb.example"}])
    );
    // Not programmed: no `addresses` key.
    let pending = Observed {
        deployment: None,
        ..ready()
    };
    let body = write(&gw(), &pending, T0);
    assert!(body["status"].get("addresses").is_none());
    assert!(body["status"].get("conditions").is_some());
}

#[test]
fn gw_08_listener_programmed() {
    let mut g = gw();
    g.listeners.push(listener("tcp", "TCP", 9000));
    let programmed = |observed: &Observed, i: usize| {
        let body = write(&g, observed, T0);
        let c = find(&body["status"]["listeners"][i]["conditions"], "Programmed");
        (
            c["status"].clone(),
            c["reason"].clone(),
            c["message"].clone(),
        )
    };
    let pending = Observed {
        deployment: None,
        ..ready()
    };
    assert_eq!(
        programmed(&ready(), 0),
        (json!("True"), json!("Programmed"), json!(""))
    );
    assert_eq!(
        programmed(&ready(), 1),
        (json!("False"), json!("Invalid"), json!("not accepted"))
    );
    assert_eq!(
        programmed(&pending, 0),
        (
            json!("False"),
            json!("Pending"),
            json!("gateway not programmed")
        )
    );
    assert_eq!(
        programmed(&pending, 1),
        (json!("False"), json!("Invalid"), json!("not accepted"))
    );
}

fn at(kind: &str, status: &str, time: &str) -> Value {
    json!({"type": kind, "status": status, "reason": "Old", "message": "old",
           "observedGeneration": 1, "lastTransitionTime": time})
}

#[test]
fn gw_09_last_transition_time_rule() {
    let mut g = gw();
    // The previous conditions are found by type and, for a listener, by name: neither by index.
    g.observed = ObservedStatus {
        conditions: vec![at("Programmed", "True", T0), at("Accepted", "Unknown", T0)],
        listeners: vec![
            json!({"name": "other", "conditions": [at("Conflicted", "False", T0)]}),
            json!({"name": "http", "conditions": [
                at("ResolvedRefs", "True", T0),
                at("Accepted", "True", T0),
                at("Programmed", "Unknown", T0),
            ]}),
        ],
        addresses: vec![],
    };
    let body = write(&g, &ready(), T1);
    let time =
        |conditions: &Value, kind: &str| find(conditions, kind)["lastTransitionTime"].clone();
    let top = &body["status"]["conditions"];
    assert_eq!(time(top, "Accepted"), json!(T1));
    assert_eq!(time(top, "Programmed"), json!(T0));
    let l = &body["status"]["listeners"][0]["conditions"];
    assert_eq!(time(l, "Accepted"), json!(T0));
    assert_eq!(time(l, "ResolvedRefs"), json!(T0));
    assert_eq!(time(l, "Programmed"), json!(T1));
    assert_eq!(time(l, "Conflicted"), json!(T1));
    // No previous condition at all.
    let body = write(&gw(), &ready(), T1);
    assert_eq!(time(&body["status"]["conditions"], "Programmed"), json!(T1));
    // The other status.
    let mut g = gw();
    g.observed.conditions = vec![at("Programmed", "False", T0)];
    let body = write(&g, &ready(), T1);
    assert_eq!(time(&body["status"]["conditions"], "Programmed"), json!(T1));
}

#[test]
fn gw_09_observed_generation_and_message_cut() {
    let mut g = gw();
    g.generation = 7;
    g.listeners.push(listener("tcp", "TCP", 9000));
    let body = write(&g, &ready(), T0);
    let mut all: Vec<Value> = body["status"]["conditions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for l in body["status"]["listeners"].as_array().into_iter().flatten() {
        all.extend(l["conditions"].as_array().cloned().unwrap_or_default());
    }
    assert_eq!(all.len(), 2 + 2 * 4);
    for c in &all {
        assert_eq!(c["observedGeneration"], json!(7), "{c}");
        let keys: Vec<&str> = c
            .as_object()
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        assert_eq!(
            keys,
            [
                "lastTransitionTime",
                "message",
                "observedGeneration",
                "reason",
                "status",
                "type"
            ],
            "{c}"
        );
    }
    // An empty message is written, not omitted.
    assert_eq!(
        find(&body["status"]["listeners"][0]["conditions"], "Accepted")["message"],
        json!("")
    );

    let long = cond(false, "Invalid", &"€".repeat(400));
    let c = long.to_json("Programmed", 7, None, T0);
    let message = c["message"].as_str().unwrap_or_default();
    assert_eq!(message.len(), 1023);
    assert!(message.chars().all(|ch| ch == '€'));
}

#[test]
fn gw_09_write_needed() {
    let keep = |g: &Gateway, observed: &Observed| status::gateway(g, &accept(g), observed, T1);
    let mut g = gw();
    g.listeners.push(listener("tcp", "TCP", 9000));
    let body = write(&g, &ready(), T0);
    g.observed = held(&body);
    assert_eq!(keep(&g, &ready()), Plan::Keep);

    // The same conditions in another order.
    g.observed.conditions.reverse();
    for l in &mut g.observed.listeners {
        if let Some(list) = l["conditions"].as_array_mut() {
            list.reverse();
        }
    }
    assert_eq!(keep(&g, &ready()), Plan::Keep);

    // A new Service address.
    let moved = Observed {
        service: Some(service("ClusterIP", &["10.96.0.11"], vec![])),
        ..ready()
    };
    assert!(matches!(keep(&g, &moved), Plan::Write(_)));

    // A new generation.
    let mut newer = g.clone();
    newer.generation = 2;
    assert!(matches!(keep(&newer, &ready()), Plan::Write(_)));

    // Pending: no `addresses` key written, none held.
    let pending = Observed {
        deployment: None,
        ..ready()
    };
    let mut g = gw();
    let body = write(&g, &pending, T0);
    g.observed = held(&body);
    assert!(g.observed.addresses.is_empty());
    assert_eq!(keep(&g, &pending), Plan::Keep);

    // The write is the whole body.
    let body = write(&gw(), &ready(), T0);
    assert_eq!(body["apiVersion"], json!("gateway.networking.k8s.io/v1"));
    assert_eq!(body["kind"], json!("Gateway"));
    assert_eq!(
        body["metadata"],
        json!({"name": "web", "namespace": "shop"})
    );
    assert_eq!(
        body["status"]["listeners"][0],
        json!({
            "attachedRoutes": 0,
            "conditions": [
                {"lastTransitionTime": T0, "message": "", "observedGeneration": 1,
                 "reason": "Accepted", "status": "True", "type": "Accepted"},
                {"lastTransitionTime": T0, "message": "", "observedGeneration": 1,
                 "reason": "NoConflicts", "status": "False", "type": "Conflicted"},
                {"lastTransitionTime": T0, "message": "", "observedGeneration": 1,
                 "reason": "Programmed", "status": "True", "type": "Programmed"},
                {"lastTransitionTime": T0, "message": "", "observedGeneration": 1,
                 "reason": "ResolvedRefs", "status": "True", "type": "ResolvedRefs"},
            ],
            "name": "http",
            "supportedKinds": [{"group": "gateway.networking.k8s.io", "kind": "HTTPRoute"}],
        })
    );
    let kinds: Vec<Value> = body["status"]["conditions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| c["type"].clone())
        .collect();
    assert_eq!(kinds, [json!("Accepted"), json!("Programmed")]);
}

#[test]
fn gw_09_gateway_class_status() {
    let condition = |status: &str, reason: &str, message: &str, time: &str| {
        json!({"lastTransitionTime": time, "message": message, "observedGeneration": 2,
               "reason": reason, "status": status, "type": "Accepted"})
    };
    let body = |conditions: Vec<Value>| {
        json!({
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "GatewayClass",
            "metadata": {"name": "mimic"},
            "status": {"conditions": conditions},
        })
    };
    let mut c = class();
    c.generation = 2;
    // The CRD's default condition.
    c.observed = vec![
        json!({"type": "Accepted", "status": "Unknown", "reason": "Pending",
                             "message": "Waiting for controller", "observedGeneration": 1,
                             "lastTransitionTime": T0}),
    ];
    let want = body(vec![condition("True", "Accepted", "", T1)]);
    assert_eq!(status::class(&c, T1), Plan::Write(want.clone()));
    c.observed = want["status"]["conditions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(status::class(&c, T1), Plan::Keep);

    c.has_parameters_ref = true;
    assert_eq!(
        status::class(&c, T1),
        Plan::Write(body(vec![condition(
            "False",
            "InvalidParameters",
            "parametersRef is not supported",
            T1
        )]))
    );
    c.has_parameters_ref = false;
    c.name = "a".repeat(64);
    match status::class(&c, T1) {
        Plan::Write(b) => {
            assert_eq!(b["status"]["conditions"][0]["reason"], json!("Unsupported"));
            assert_eq!(b["status"]["conditions"][0]["status"], json!("False"));
        }
        Plan::Keep => panic!("expected a write"),
    }
}
