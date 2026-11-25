//! GW-10 to GW-14 (objects, ownership, deletion, failures), GW-15 (classes and refused Gateways)
//! and GW-17 (counts and lines): the passes of the gateway reconciler against the fake API of
//! `fake.rs`, without a cluster.

use std::collections::BTreeSet;
use std::time::Duration;

use mimic_core::gateway::input::Gateway;
use mimic_core::gateway::{accept, objects};
use prometheus_client::registry::Registry;
use serde_json::{Value, json};
use tokio::time::Instant;

use super::convert;
use super::fake::{NOW, Request, Rig, class, gateway, key, listener, settings};
use super::metrics::GatewayMetrics;
use crate::mesh::Event;
use crate::mesh::metrics::label;
use crate::mesh::tests::Lines;

/// `(type, status, reason)` of the top-level conditions of a Gateway status body.
fn conditions(body: &Value) -> Vec<(String, String, String)> {
    body["status"]["conditions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let s = |f: &str| c[f].as_str().unwrap().to_string();
            (s("type"), s("status"), s("reason"))
        })
        .collect()
}

fn triple(kind: &str, status: &str, reason: &str) -> (String, String, String) {
    (kind.into(), status.into(), reason.into())
}

/// A rig of one replica with the namespaces `{shop}`, the class `mimic` and no Gateway.
fn mimic_only() -> Rig {
    let rig = Rig::new(1, &settings(&["shop"]));
    rig.api.put(
        "GatewayClass",
        class_object("mimic", mimic_core::gateway::CONTROLLER_NAME),
    );
    rig
}

fn tcp_only() -> Value {
    json!([{"name": "tcp", "port": 9000, "protocol": "TCP"}])
}

#[tokio::test]
async fn gw_15_a_not_listed_gateway_gets_its_status_and_no_object_request() {
    let rig = mimic_only();
    rig.api
        .put("Gateway", gateway_object("other", "web", http(80)));
    rig.round(0).await;
    assert_eq!(
        rig.api.whats(),
        s(&["status GatewayClass mimic", "status Gateway other/web"])
    );
    let calls = rig.api.calls();
    assert_eq!(
        calls[0].body["status"]["conditions"][0]["status"], "True",
        "the class is accepted"
    );
    let body = &calls[1].body;
    assert_eq!(
        conditions(body),
        [
            triple("Accepted", "False", "NamespaceNotAllowed"),
            triple("Programmed", "False", "Invalid"),
        ]
    );
    assert_eq!(
        body["status"]["conditions"][0]["message"],
        "namespace other is not in gateway.namespaces"
    );
    assert_eq!(
        body["metadata"],
        json!({"name": "web", "namespace": "other"})
    );

    // The cluster holds what was written: Keep, no request.
    rig.api.clear();
    rig.round(0).await;
    assert_eq!(rig.api.whats(), Vec::<String>::new());
}

#[tokio::test]
async fn gw_15_other_controllers_get_no_request() {
    let rig = Rig::new(1, &settings(&["shop"]));
    rig.api.put(
        "GatewayClass",
        class_object("istio", "istio.io/gateway-controller"),
    );
    let mut edge = gateway_object("shop", "edge", tcp_only());
    edge["spec"]["gatewayClassName"] = json!("istio");
    rig.api.put("Gateway", edge);
    rig.round(0).await;
    assert_eq!(rig.api.whats(), Vec::<String>::new());
}

/// Lists no Deployment and no Service on replica 0.
fn no_objects(rig: &Rig) {
    let r = &rig.replicas[0];
    r.feed_deployments(Event::Init);
    r.feed_deployments(Event::InitDone);
    r.feed_services(Event::Init);
    r.feed_services(Event::InitDone);
}

#[tokio::test]
async fn gw_15_no_pass_without_the_crd_or_a_full_list() {
    let rig = mimic_only();
    let r = &rig.replicas[0];
    let refused = gateway("shop", "web", vec![listener("tcp", 9000, "TCP")]);
    no_objects(&rig);
    r.crd(true);
    r.feed_classes(Event::Init);
    r.feed_classes(Event::InitApply(key("", "mimic"), class("mimic")));
    r.feed_classes(Event::InitDone);
    r.feed_gateways(Event::Init);
    r.feed_gateways(Event::InitApply(key("shop", "web"), refused.clone()));
    rig.pass(0).await;
    assert_eq!(rig.api.whats(), Vec::<String>::new(), "gateways not listed");
    r.feed_gateways(Event::InitDone);
    r.crd(false);
    rig.pass(0).await;
    assert_eq!(rig.api.whats(), Vec::<String>::new(), "the CRD absent");
    // Back: the table was emptied, and a pass waits for a new full list.
    r.crd(true);
    rig.pass(0).await;
    assert_eq!(rig.api.whats(), Vec::<String>::new(), "no list since");
    r.feed_gateways(Event::Init);
    r.feed_gateways(Event::InitApply(key("shop", "web"), refused));
    r.feed_gateways(Event::InitDone);
    rig.pass(0).await;
    assert_eq!(
        rig.api.whats(),
        s(&["status GatewayClass mimic", "status Gateway shop/web"])
    );
}

#[tokio::test]
async fn gw_15_a_gateway_of_an_unknown_class_gets_nothing() {
    let rig = mimic_only();
    let mut stray = gateway_object("shop", "stray", tcp_only());
    stray["spec"]["gatewayClassName"] = json!("unknown");
    rig.api.put("Gateway", stray);
    rig.round(0).await;
    assert_eq!(rig.api.whats(), s(&["status GatewayClass mimic"]));
    assert_eq!(count(&rig.metrics[0], "invalid"), 0);
    assert_eq!(state(&rig.metrics[0], "not_accepted"), 0);
}

fn count(metrics: &GatewayMetrics, result: &str) -> u64 {
    metrics
        .reconciles
        .get_or_create(&label("result", result))
        .get()
}

fn state(metrics: &GatewayMetrics, state: &str) -> i64 {
    metrics.gateways.get_or_create(&label("state", state)).get()
}

#[tokio::test]
async fn gw_17_refused_gateways_are_counted_without_names() {
    let rig = mimic_only();
    rig.api
        .put("Gateway", gateway_object("shop", "web", tcp_only()));
    rig.round(0).await;
    assert_eq!(
        rig.api.whats(),
        s(&["status GatewayClass mimic", "status Gateway shop/web"])
    );
    let metrics = &rig.metrics[0];
    let results: Vec<u64> = ["ok", "invalid", "conflict", "error"]
        .iter()
        .map(|r| count(metrics, r))
        .collect();
    assert_eq!(results, [0, 1, 0, 0]);
    let states: Vec<i64> = ["programmed", "pending", "not_accepted"]
        .iter()
        .map(|s| state(metrics, s))
        .collect();
    assert_eq!(states, [0, 0, 1]);
    let mut encoded = String::new();
    prometheus_client::encoding::text::encode(&mut encoded, &rig.registry).unwrap();
    assert!(
        encoded.contains(r#"mimic_gateway_reconciles_total{result="invalid"} 1"#),
        "the counter series"
    );
    assert!(encoded.contains(r#"mimic_gateway_gateways{state="not_accepted"} 1"#));
    assert!(encoded.contains(r#"mimic_gateway_gateways{state="programmed"} 0"#));
    for name in ["web", "shop", "namespace="] {
        assert!(!encoded.contains(name), "no series names {name}");
    }

    // The Gateway is deleted: the gauge follows.
    rig.api.remove("Gateway", "shop", "web");
    rig.round(0).await;
    assert_eq!(state(metrics, "not_accepted"), 0);
}

#[tokio::test(start_paused = true)]
async fn gw_14_an_api_that_fails_every_request_is_warned_once_per_key() {
    let lines = Lines::default();
    let _guard = json_lines(&lines);
    let rig = fixture(1);
    let versions = rig.api.versions();
    rig.api.fail_all("InternalError", 500);
    rig.round(0).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    rig.round(0).await;
    assert_eq!(rig.api.versions(), versions, "nothing written");
    assert_eq!(count(&rig.metrics[0], "error"), 2, "web, at 0 s and 2 s");
    let warns: Vec<String> = lines_of(&lines)
        .lines()
        .filter(|l| l.contains("gateway reconcile failed"))
        .map(str::to_string)
        .collect();
    assert_eq!(warns.len(), 2, "the class and the Gateway, once each");
    assert!(warns.iter().any(|l| l.contains(r#""kind":"GatewayClass""#)));
    assert!(
        warns
            .iter()
            .any(|l| l.contains(r#""gateway":"shop/web""#) && l.contains(r#""kind":"Deployment""#))
    );
}

#[tokio::test]
async fn gw_17_the_series_exist_before_the_first_pass() {
    let mut registry = Registry::with_prefix("mimic");
    GatewayMetrics::register(&mut registry);
    let mut encoded = String::new();
    prometheus_client::encoding::text::encode(&mut encoded, &registry).unwrap();
    let series: BTreeSet<&str> = encoded.lines().filter(|l| !l.starts_with('#')).collect();
    assert_eq!(
        series,
        BTreeSet::from([
            r#"mimic_gateway_gateways{state="not_accepted"} 0"#,
            r#"mimic_gateway_gateways{state="pending"} 0"#,
            r#"mimic_gateway_gateways{state="programmed"} 0"#,
            r#"mimic_gateway_reconciles_total{result="conflict"} 0"#,
            r#"mimic_gateway_reconciles_total{result="error"} 0"#,
            r#"mimic_gateway_reconciles_total{result="invalid"} 0"#,
            r#"mimic_gateway_reconciles_total{result="ok"} 0"#,
        ])
    );
}

// Step 6 (GW-10 to GW-14, GW-15, GW-17): the objects of accepted Gateways, against the fake API of
// `fake.rs`. Fixture: class `mimic`, namespaces `{shop}`, Gateway `shop/web` (`uid-web`, HTTP 80).

const CANARY: &str = "canary-4e0b77c1";

fn class_object(name: &str, controller: &str) -> Value {
    json!({"metadata": {"name": name}, "spec": {"controllerName": controller}})
}

fn gateway_object(namespace: &str, name: &str, listeners: Value) -> Value {
    json!({"metadata": {"namespace": namespace, "name": name, "uid": format!("uid-{name}")},
           "spec": {"gatewayClassName": "mimic", "listeners": listeners}})
}

fn http(port: u16) -> Value {
    json!([{"name": "http", "port": port, "protocol": "HTTP"}])
}

fn fixture(replicas: usize) -> Rig {
    let rig = Rig::new(replicas, &settings(&["shop"]));
    rig.api.put(
        "GatewayClass",
        class_object("mimic", mimic_core::gateway::CONTROLLER_NAME),
    );
    rig.api
        .put("Gateway", gateway_object("shop", "web", http(80)));
    rig
}

/// The Deployment `name` of `shop` has `available` replicas, current.
fn ready(rig: &Rig, name: &str, available: u32) {
    rig.api.edit("Deployment", "shop", name, |d| {
        let generation = d["metadata"]["generation"].clone();
        d["status"] = json!({"observedGeneration": generation, "replicas": 2,
                             "availableReplicas": available});
    });
}

/// The calls but the class status.
fn whats(rig: &Rig) -> Vec<String> {
    rig.api
        .whats()
        .into_iter()
        .filter(|w| !w.starts_with("status GatewayClass"))
        .collect()
}

/// The object applies and gets.
fn applies(rig: &Rig) -> Vec<String> {
    rig.api
        .whats()
        .into_iter()
        .filter(|w| w.starts_with("apply ") || w.starts_with("get "))
        .collect()
}

fn s(list: &[&str]) -> Vec<String> {
    list.iter().map(|w| w.to_string()).collect()
}

/// The objects `mimic_core` generates for the Gateway the store holds.
fn generated(rig: &Rig, name: &str) -> objects::Objects {
    let stored = rig.api.get("Gateway", "shop", name).unwrap();
    let (_, gw) = convert::gateway(&serde_json::from_value(stored).unwrap()).unwrap();
    let acceptance = accept::gateway(&gw, &class("mimic"), &settings(&["shop"]));
    objects::objects(&gw, &acceptance, &settings(&["shop"])).expect("accepted")
}

/// The `Programmed` condition of the last status apply of `gateway`.
fn programmed_of(rig: &Rig, gateway: &str) -> Value {
    let what = format!("status Gateway {gateway}");
    let call = rig
        .api
        .calls()
        .into_iter()
        .rev()
        .find(|c| c.what == what)
        .expect("a status apply");
    call.body["status"]["conditions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "Programmed")
        .cloned()
        .unwrap()
}

/// Converged: the objects applied, the Deployment available, the status written and read.
async fn converged(rig: &Rig) {
    rig.round(0).await;
    ready(rig, "web-mimic", 2);
    rig.round(0).await;
    rig.round(0).await;
    rig.api.clear();
}

fn lines_of(lines: &Lines) -> String {
    String::from_utf8(lines.0.lock().unwrap().clone()).unwrap()
}

fn json_lines(lines: &Lines) -> tracing::subscriber::DefaultGuard {
    tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .json()
            .with_writer(lines.clone())
            .finish(),
    )
}

#[tokio::test]
async fn gw_10_an_accepted_gateway_gets_three_applies_and_its_status() {
    let params = super::cluster::apply_params();
    assert_eq!(params.field_manager.as_deref(), Some("mimic-gateway"));
    assert!(params.force, "server-side apply with force");
    let rig = fixture(1);
    rig.round(0).await;
    assert_eq!(
        whats(&rig),
        s(&[
            "get Deployment shop/web-mimic",
            "get Service shop/web-mimic",
            "get PodDisruptionBudget shop/web-mimic",
            "apply Deployment shop/web-mimic",
            "apply Service shop/web-mimic",
            "apply PodDisruptionBudget shop/web-mimic",
            "status Gateway shop/web",
        ])
    );
    let expected = generated(&rig, "web");
    let body = |what: &str| {
        rig.api
            .calls()
            .into_iter()
            .find(|c| c.what == what)
            .unwrap()
            .body
    };
    assert_eq!(body("apply Deployment shop/web-mimic"), expected.deployment);
    assert_eq!(body("apply Service shop/web-mimic"), expected.service);
    assert_eq!(
        body("apply PodDisruptionBudget shop/web-mimic"),
        expected.pod_disruption_budget
    );
    let programmed = programmed_of(&rig, "shop/web");
    assert_eq!(
        (
            &programmed["status"],
            &programmed["reason"],
            &programmed["message"]
        ),
        (
            &json!("False"),
            &json!("Pending"),
            &json!("0 of 2 replicas available")
        )
    );
}

#[tokio::test]
async fn gw_10_programmed_with_the_address_once_ready() {
    let lines = Lines::default();
    let _guard = json_lines(&lines);
    let rig = fixture(1);
    rig.round(0).await;
    ready(&rig, "web-mimic", 2);
    rig.api.clear();
    rig.round(0).await;
    assert_eq!(whats(&rig), s(&["status Gateway shop/web"]));
    let programmed = programmed_of(&rig, "shop/web");
    assert_eq!(
        (
            &programmed["status"],
            &programmed["reason"],
            &programmed["message"]
        ),
        (
            &json!("True"),
            &json!("Programmed"),
            &json!("2 of 2 replicas available")
        )
    );
    let status = rig.api.get("Gateway", "shop", "web").unwrap()["status"].clone();
    assert_eq!(
        status["addresses"],
        json!([{"type": "IPAddress", "value": "10.96.0.10"}])
    );
    for _ in 0..3 {
        rig.round(0).await;
    }
    // A status rewritten while it stays programmed: no new line.
    rig.api.edit("Gateway", "shop", "web", |g| {
        g["status"]["conditions"][0]["message"] = json!("edited");
    });
    rig.api.clear();
    rig.round(0).await;
    assert_eq!(whats(&rig), s(&["status Gateway shop/web"]));
    let text = lines_of(&lines);
    let info: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("gateway programmed"))
        .collect();
    assert_eq!(info.len(), 1, "once per transition");
    assert!(info[0].contains(r#""gateway":"shop/web""#) && info[0].contains(r#""generation":1"#));
}

#[tokio::test]
async fn gw_10_republications_change_nothing() {
    let rig = fixture(1);
    converged(&rig).await;
    let versions = rig.api.versions();
    for _ in 0..10 {
        rig.round(0).await;
    }
    assert_eq!(rig.api.whats(), Vec::<String>::new());
    assert_eq!(rig.api.versions(), versions);
}

#[tokio::test]
async fn gw_10_a_port_change_applies_the_deployment_and_service() {
    let rig = fixture(1);
    converged(&rig).await;
    rig.api.edit("Gateway", "shop", "web", |g| {
        g["spec"]["listeners"] = http(8080);
    });
    rig.round(0).await;
    let calls = whats(&rig);
    assert_eq!(
        calls[..2],
        s(&[
            "apply Deployment shop/web-mimic",
            "apply Service shop/web-mimic"
        ])
    );
    assert!(
        calls[2..].iter().all(|w| w.starts_with("status ")),
        "{calls:?}"
    );
    let service = rig.api.get("Service", "shop", "web-mimic").unwrap();
    assert_eq!(service["spec"]["ports"][0]["port"], 8080);
    let deployment = rig.api.get("Deployment", "shop", "web-mimic").unwrap();
    assert_eq!(
        deployment["spec"]["template"]["spec"]["containers"][0]["env"][0]["value"],
        "8080"
    );
}

#[tokio::test]
async fn gw_10_a_gateway_status_change_alone_applies_no_object() {
    let rig = fixture(1);
    converged(&rig).await;
    rig.api.edit("Gateway", "shop", "web", |g| {
        g["status"]["conditions"][1]["message"] = json!("edited");
    });
    rig.round(0).await;
    assert_eq!(
        whats(&rig),
        s(&["status Gateway shop/web"]),
        "the status restored"
    );
}

#[tokio::test]
async fn gw_10_an_edit_of_the_deployment_spec_or_labels_applies_it_again() {
    let rig = fixture(1);
    converged(&rig).await;
    rig.api.edit("Deployment", "shop", "web-mimic", |d| {
        d["spec"]["replicas"] = json!(5);
    });
    rig.round(0).await;
    assert_eq!(applies(&rig), s(&["apply Deployment shop/web-mimic"]));
    rig.api.clear();
    rig.api.edit("Service", "shop", "web-mimic", |d| {
        d["metadata"]["labels"]["team"] = json!("x");
    });
    rig.round(0).await;
    assert_eq!(applies(&rig), s(&["apply Service shop/web-mimic"]));
}

#[tokio::test(start_paused = true)]
async fn gw_10_the_resync_applies_again() {
    let rig = fixture(1);
    converged(&rig).await;
    let versions = rig.api.versions();
    tokio::time::sleep(Duration::from_secs(301)).await;
    rig.round(0).await;
    let applies: Vec<String> = whats(&rig)
        .into_iter()
        .filter(|w| w.starts_with("apply "))
        .collect();
    assert_eq!(
        applies,
        s(&[
            "apply Deployment shop/web-mimic",
            "apply Service shop/web-mimic",
            "apply PodDisruptionBudget shop/web-mimic",
        ])
    );
    assert_eq!(
        rig.api.versions(),
        versions,
        "an equal apply changes nothing"
    );
}

#[tokio::test]
async fn gw_11_two_replicas_converge() {
    let single = fixture(1);
    converged(&single).await;
    let pair = fixture(2);
    for i in [0, 1] {
        pair.round(i).await;
    }
    ready(&pair, "web-mimic", 2);
    let mut quiet = 0;
    let mut rounds = 0;
    while quiet < 2 {
        let before = pair.api.calls().len();
        for i in [0, 1] {
            pair.round(i).await;
        }
        quiet = if pair.api.calls().len() == before {
            quiet + 1
        } else {
            0
        };
        rounds += 1;
        assert!(rounds < 10, "no convergence: {:?}", pair.api.whats());
    }
    let store = |rig: &Rig| rig.api.objects.lock().unwrap().clone();
    assert_eq!(store(&pair), store(&single));
    // Every status write of a replica that changed the store: one per transition.
    let gateway = pair.api.get("Gateway", "shop", "web").unwrap();
    assert_eq!(
        gateway["metadata"]["resourceVersion"],
        single.api.get("Gateway", "shop", "web").unwrap()["metadata"]["resourceVersion"]
    );
    let versions = pair.api.versions();
    pair.api.clear();
    for _ in 0..10 {
        for i in [0, 1] {
            pair.round(i).await;
        }
    }
    assert_eq!(pair.api.whats(), Vec::<String>::new());
    assert_eq!(pair.api.versions(), versions);
}

/// A Deployment `name` of `shop` that is not mimic's.
fn squatter(rig: &Rig, name: &str) {
    rig.api.put(
        "Deployment",
        json!({"metadata": {"namespace": "shop", "name": name, "labels": {"app": "other"}},
               "spec": {"selector": {"matchLabels": {"app": "other"}},
                        "template": {"metadata": {"labels": {"app": "other"}},
                                     "spec": {"containers": [{"name": "a", "image": "a"}]}}}}),
    );
}

#[tokio::test]
async fn gw_12_an_unmanaged_object_blocks_the_gateway() {
    let lines = Lines::default();
    let _guard = json_lines(&lines);
    let rig = fixture(1);
    squatter(&rig, "web-mimic");
    let squatted = rig.api.get("Deployment", "shop", "web-mimic").unwrap();
    for _ in 0..3 {
        rig.round(0).await;
    }
    let writes: Vec<String> = whats(&rig)
        .into_iter()
        .filter(|w| w.starts_with("apply ") || w.starts_with("delete "))
        .collect();
    assert_eq!(writes, Vec::<String>::new());
    assert_eq!(
        rig.api.get("Deployment", "shop", "web-mimic").unwrap(),
        squatted
    );
    let programmed = programmed_of(&rig, "shop/web");
    assert_eq!(
        (
            &programmed["status"],
            &programmed["reason"],
            &programmed["message"]
        ),
        (
            &json!("False"),
            &json!("Invalid"),
            &json!("Deployment web-mimic exists and is not managed by mimic")
        )
    );
    assert_eq!(count(&rig.metrics[0], "conflict"), 3);
    let text = lines_of(&lines);
    let warns: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("gateway object not managed"))
        .collect();
    assert_eq!(warns.len(), 1, "one WARN per streak");
    for field in [
        r#""gateway":"shop/web""#,
        r#""kind":"Deployment""#,
        r#""name":"web-mimic""#,
        r#""level":"WARN""#,
    ] {
        assert!(warns[0].contains(field), "{field}");
    }

    rig.api.remove("Deployment", "shop", "web-mimic");
    rig.api.clear();
    rig.round(0).await;
    let applies: Vec<String> = whats(&rig)
        .into_iter()
        .filter(|w| w.starts_with("apply "))
        .collect();
    assert_eq!(applies.len(), 3, "{applies:?}");
    assert_eq!(count(&rig.metrics[0], "ok"), 1);
}

#[tokio::test]
async fn gw_12_an_object_with_the_gateway_label_only_is_not_managed() {
    let rig = fixture(1);
    let mut taken = generated(&rig, "web").service;
    taken["metadata"]["labels"] = json!({"mimic.platform.io/gateway": "web"});
    rig.api.put("Service", taken);
    rig.round(0).await;
    let writes: Vec<String> = whats(&rig)
        .into_iter()
        .filter(|w| w.starts_with("apply ") || w.starts_with("delete "))
        .collect();
    assert_eq!(writes, Vec::<String>::new());
    assert_eq!(
        programmed_of(&rig, "shop/web")["message"],
        "Service web-mimic exists and is not managed by mimic"
    );
}

#[tokio::test]
async fn gw_12_objects_of_an_older_gateway_are_adopted() {
    let rig = fixture(1);
    let old = generated(&rig, "web");
    let owned = |mut o: Value| {
        o["metadata"]["ownerReferences"][0]["uid"] = json!("uid-old");
        o
    };
    rig.api.put("Deployment", owned(old.deployment));
    rig.api.put("Service", owned(old.service));
    rig.api
        .put("PodDisruptionBudget", owned(old.pod_disruption_budget));
    rig.round(0).await;
    let applies: Vec<Request> = rig
        .api
        .calls()
        .into_iter()
        .filter(|c| c.what.starts_with("apply "))
        .collect();
    assert_eq!(applies.len(), 3);
    for kind in ["Deployment", "Service", "PodDisruptionBudget"] {
        let held = rig.api.get(kind, "shop", "web-mimic").unwrap();
        let owners = held["metadata"]["ownerReferences"].as_array().unwrap();
        assert_eq!(owners.len(), 1, "{kind}");
        assert_eq!(owners[0]["uid"], "uid-web", "{kind}");
    }
    assert_eq!(count(&rig.metrics[0], "conflict"), 0);
}

/// The deletes of the three objects `name` of `shop`, with their UIDs.
fn deletes_of(rig: &Rig, name: &str) -> Vec<String> {
    ["Deployment", "Service", "PodDisruptionBudget"]
        .iter()
        .map(|kind| {
            let uid = rig.api.get(kind, "shop", name).unwrap()["metadata"]["uid"]
                .as_str()
                .unwrap()
                .to_string();
            format!("delete {kind} shop/{name} {uid}")
        })
        .collect()
}

fn deletes(rig: &Rig) -> Vec<String> {
    rig.api
        .whats()
        .into_iter()
        .filter(|w| w.starts_with("delete "))
        .collect()
}

#[tokio::test]
async fn gw_13_not_accepted_or_another_class_deletes_the_managed_objects() {
    // (a) The only listener changed to TCP.
    let rig = fixture(1);
    converged(&rig).await;
    let expected = deletes_of(&rig, "web-mimic");
    rig.api.edit("Gateway", "shop", "web", |g| {
        g["spec"]["listeners"] = json!([{"name": "tcp", "port": 9000, "protocol": "TCP"}]);
    });
    rig.round(0).await;
    assert_eq!(deletes(&rig), expected);
    assert!(
        rig.api
            .whats()
            .contains(&"get PodDisruptionBudget shop/web-mimic".to_string())
    );
    assert_eq!(rig.api.get("Deployment", "shop", "web-mimic"), None);

    // (b) The class changed to one of another controller.
    let rig = fixture(1);
    converged(&rig).await;
    let expected = deletes_of(&rig, "web-mimic");
    rig.api.put(
        "GatewayClass",
        class_object("istio", "istio.io/gateway-controller"),
    );
    rig.api.edit("Gateway", "shop", "web", |g| {
        g["spec"]["gatewayClassName"] = json!("istio");
    });
    rig.round(0).await;
    assert_eq!(deletes(&rig), expected);
    assert_eq!(rig.api.get("Service", "shop", "web-mimic"), None);
}

/// Managed Deployment, Service and PodDisruptionBudget `<gateway>-mimic` of `namespace`, owned
/// by a Gateway of UID `owner`.
fn managed(rig: &Rig, namespace: &str, gateway: &str, owner: &str) {
    let gw: Gateway = Gateway {
        uid: owner.into(),
        ..super::fake::gateway(namespace, gateway, vec![listener("http", 80, "HTTP")])
    };
    let all = settings(&["shop", namespace]);
    let acceptance = accept::gateway(&gw, &class("mimic"), &all);
    let objects = objects::objects(&gw, &acceptance, &all).unwrap();
    rig.api.put("Deployment", objects.deployment);
    rig.api.put("Service", objects.service);
    rig.api
        .put("PodDisruptionBudget", objects.pod_disruption_budget);
}

#[tokio::test]
async fn gw_13_orphans_are_deleted_with_their_pod_disruption_budget() {
    let rig = fixture(1);
    converged(&rig).await;
    managed(&rig, "shop", "ghost", "uid-x");
    let expected = deletes_of(&rig, "ghost-mimic");
    rig.round(0).await;
    assert_eq!(deletes(&rig), expected);
    assert_eq!(
        rig.api.get("PodDisruptionBudget", "shop", "ghost-mimic"),
        None
    );
    assert!(rig.api.get("Deployment", "shop", "web-mimic").is_some());
}

#[tokio::test]
async fn gw_13_what_is_never_deleted() {
    // An unmanaged object of a generated name, and managed objects in a namespace not listed.
    let rig = fixture(1);
    squatter(&rig, "web-mimic");
    managed(&rig, "other", "edge", "uid-x");
    rig.api
        .put("Gateway", gateway_object("other", "edge", json!([])));
    for _ in 0..3 {
        rig.round(0).await;
    }
    assert_eq!(deletes(&rig), Vec::<String>::new());

    // A deleted Gateway: the garbage collector deletes its objects.
    let rig = fixture(1);
    converged(&rig).await;
    rig.api.remove("Gateway", "shop", "web");
    rig.replicas[0].feed_gateways(Event::Delete(key("shop", "web")));
    for _ in 0..3 {
        rig.round(0).await;
    }
    assert_eq!(rig.api.whats(), Vec::<String>::new());
}

#[tokio::test]
async fn gw_13_no_delete_before_the_first_list_or_while_the_crd_is_absent() {
    let rig = fixture(1);
    managed(&rig, "shop", "ghost", "uid-x");
    let r = &rig.replicas[0];
    // (a) Everything listed but the Services.
    rig.read_partial(0);
    r.pass(NOW).await;
    assert_eq!(rig.api.whats(), Vec::<String>::new(), "services not listed");
    // (b) Listed, the CRD absent.
    rig.read(0);
    r.crd(false);
    r.pass(NOW).await;
    assert_eq!(rig.api.whats(), Vec::<String>::new(), "the CRD absent");
    // Listed and present: the deletes.
    let expected = deletes_of(&rig, "ghost-mimic");
    rig.round(0).await;
    assert_eq!(deletes(&rig), expected);
}

#[tokio::test(start_paused = true)]
async fn gw_14_failures_back_off_and_warn_once_per_streak() {
    let lines = Lines::default();
    let _guard = json_lines(&lines);
    let rig = fixture(1);
    rig.api.edit("Gateway", "shop", "web", |g| {
        g["spec"]["listeners"][0]["hostname"] = json!(format!("{CANARY}.example.com"));
    });
    let mut bad = gateway_object("shop", "bad", http(81));
    bad["spec"]["infrastructure"] = json!({"annotations": {"example.com/x": CANARY}});
    rig.api.put("Gateway", bad);
    let apply = "apply Deployment shop/web-mimic";
    rig.api.fail(apply, "InternalError", 500);
    rig.api.fail(apply, "Invalid", 422);
    for _ in 0..6 {
        rig.api.fail(apply, "InternalError", 500);
    }
    rig.api
        .fail("status Gateway shop/bad", "InternalError", 500);
    let start = Instant::now();
    rig.read(0);
    tokio::spawn(rig.replicas[0].clone().run());
    tokio::time::sleep(Duration::from_secs(130)).await;

    let tries: Vec<(u64, bool)> = rig
        .api
        .calls()
        .into_iter()
        .filter(|c| c.what == apply)
        .map(|c| ((c.at - start).as_millis() as u64, c.ok))
        .collect();
    let expected = [0, 1, 3, 7, 15, 31, 61, 91, 121];
    assert_eq!(tries.len(), expected.len(), "{tries:?}");
    for ((at, ok), want) in tries.iter().zip(expected) {
        assert!(at.abs_diff(want * 1000) <= 10, "{tries:?}");
        assert_eq!(*ok, want == 121, "{tries:?}");
    }
    // Skipped whole while failing: no status before the objects are applied.
    let statuses: Vec<u64> = rig
        .api
        .calls()
        .into_iter()
        .filter(|c| c.what == "status Gateway shop/web")
        .map(|c| (c.at - start).as_millis() as u64)
        .collect();
    assert!(
        !statuses.is_empty() && statuses.iter().all(|at| *at >= 121_000),
        "{statuses:?}"
    );
    // 8 for `web`, 1 for `bad`.
    assert_eq!(count(&rig.metrics[0], "error"), 9);
    let text = lines_of(&lines);
    let warns = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|l| l.contains("gateway reconcile failed"))
            .map(str::to_string)
            .collect()
    };
    let first = warns(&text);
    assert_eq!(first.len(), 2, "one per Gateway and streak");
    let web = first
        .iter()
        .find(|l| l.contains(r#""gateway":"shop/web""#))
        .unwrap();
    assert!(
        web.contains(r#""kind":"Deployment""#) && web.contains(r#""error":"InternalError 500""#)
    );
    let bad = first
        .iter()
        .find(|l| l.contains(r#""gateway":"shop/bad""#))
        .unwrap();
    assert!(bad.contains(r#""kind":"Gateway""#));

    // A new failure after the success: a new streak, a new WARN.
    rig.api
        .fail("apply Service shop/web-mimic", "InternalError", 500);
    rig.api.edit("Gateway", "shop", "web", |g| {
        g["spec"]["listeners"][0]["port"] = json!(8080);
    });
    rig.read(0);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let text = lines_of(&lines);
    assert_eq!(warns(&text).len(), 3);
    assert!(
        !text.contains(CANARY),
        "no hostname or annotation value in a log line"
    );
}

#[tokio::test(start_paused = true)]
async fn gw_14_a_retry_left_untaken_does_not_spin() {
    let rig = fixture(1);
    let mut stray = generated(&rig, "web").deployment;
    stray["metadata"]["name"] = json!("stray");
    stray["metadata"]["labels"]["mimic.platform.io/gateway"] = json!("ghost");
    rig.api.put("Deployment", stray);
    rig.api
        .fail("delete Deployment shop/stray", "InternalError", 500);
    rig.round(0).await;
    assert!(
        rig.api.get("Deployment", "shop", "stray").is_some(),
        "the delete failed"
    );
    // `ghost` now names a Gateway of another class: `stray` is no longer swept.
    rig.api.put(
        "GatewayClass",
        class_object("istio", "istio.io/gateway-controller"),
    );
    let mut ghost = gateway_object("shop", "ghost", http(80));
    ghost["spec"]["gatewayClassName"] = json!("istio");
    rig.api.put("Gateway", ghost);
    tokio::time::sleep(Duration::from_secs(2)).await;
    rig.read(0);
    let next = rig.replicas[0].pass(NOW).await.expect("the resync");
    assert!(next > Instant::now(), "a due instant in the past");
}

#[tokio::test]
async fn gw_15_no_object_request_outside_the_listed_namespaces() {
    let rig = fixture(1);
    rig.api.remove("Gateway", "shop", "web");
    rig.api
        .put("Gateway", gateway_object("other", "web", http(80)));
    managed(&rig, "other", "ghost", "uid-x");
    rig.round(0).await;
    assert_eq!(whats(&rig), s(&["status Gateway other/web"]));
}

#[tokio::test]
async fn gw_17_states_results_and_programmed_lines() {
    let lines = Lines::default();
    let _guard = json_lines(&lines);
    let rig = fixture(1);
    rig.api.edit("Gateway", "shop", "web", |g| {
        g["spec"]["listeners"][0]["hostname"] = json!("shop.example.com");
    });
    rig.api
        .put("Gateway", gateway_object("shop", "slow", http(80)));
    rig.api.put(
        "Gateway",
        gateway_object(
            "shop",
            "tcp",
            json!([{"name": "tcp", "port": 9000, "protocol": "TCP"}]),
        ),
    );
    rig.round(0).await;
    ready(&rig, "web-mimic", 2);
    ready(&rig, "slow-mimic", 0);
    rig.round(0).await;
    rig.round(0).await;
    let states: Vec<i64> = ["programmed", "pending", "not_accepted"]
        .iter()
        .map(|s| state(&rig.metrics[0], s))
        .collect();
    assert_eq!(states, [1, 1, 1]);
    let mut encoded = String::new();
    prometheus_client::encoding::text::encode(&mut encoded, &rig.registry).unwrap();
    let results: BTreeSet<&str> = encoded
        .lines()
        .filter(|l| l.starts_with("mimic_gateway_reconciles_total{"))
        .filter_map(|l| l.split('"').nth(1))
        .collect();
    assert_eq!(
        results,
        BTreeSet::from(["conflict", "error", "invalid", "ok"])
    );
    for name in ["web", "slow", "tcp", "shop", "example.com", "namespace="] {
        assert!(!encoded.contains(name), "no series names {name}");
    }

    // Programmed True, False, True: two INFO lines.
    ready(&rig, "web-mimic", 0);
    rig.round(0).await;
    rig.round(0).await;
    ready(&rig, "web-mimic", 2);
    rig.round(0).await;
    rig.round(0).await;
    let text = lines_of(&lines);
    let info: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("gateway programmed"))
        .collect();
    assert_eq!(info.len(), 2);
    assert!(info.iter().all(|l| l.contains(r#""gateway":"shop/web""#)));
}
