//! GW-15 (classes and refused Gateways), GW-17 (their counts): the passes of the gateway
//! reconciler against a recording cluster, without a cluster.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kube::api::DynamicObject;
use mimic_core::gateway::input::{Gateway, GatewayClass, Infrastructure, Listener};
use mimic_core::gateway::{ServiceType, Settings};
use prometheus_client::registry::Registry;
use serde_json::{Value, json};
use tokio::time::Instant;

use super::cluster::{ApiError, Cluster, ClusterFuture, StatusTarget};
use super::convert;
use super::metrics::GatewayMetrics;
use super::reconcile::Reconciler;
use crate::mesh::Event;
use crate::mesh::metrics::label;
use crate::mesh::tests::Lines;

pub(super) const NOW: &str = "2026-11-25T10:00:00Z";

/// A request the reconciler sent: `status <target>` with its body.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Call {
    pub what: String,
    pub body: Value,
}

/// Records each request and answers it; `fail` makes every request fail with a 500.
#[derive(Default)]
pub(super) struct Recorder {
    pub calls: Mutex<Vec<Call>>,
    pub fail: Mutex<bool>,
}

impl Recorder {
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    pub fn whats(&self) -> Vec<String> {
        self.calls().into_iter().map(|c| c.what).collect()
    }
}

impl Cluster for Recorder {
    fn apply_status<'a>(
        &'a self,
        target: StatusTarget<'a>,
        body: &'a Value,
    ) -> ClusterFuture<'a, ()> {
        Box::pin(async move {
            let what = match target {
                StatusTarget::Gateway { namespace, name } => {
                    format!("status gateway {namespace}/{name}")
                }
                StatusTarget::Class { name } => format!("status class {name}"),
            };
            self.calls.lock().unwrap().push(Call {
                what,
                body: body.clone(),
            });
            if *self.fail.lock().unwrap() {
                Err(ApiError {
                    reason: "InternalError".into(),
                    code: 500,
                })
            } else {
                Ok(())
            }
        })
    }
}

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

pub(super) fn key(namespace: &str, name: &str) -> (String, String) {
    (namespace.to_string(), name.to_string())
}

/// A reconciler with the CRD present and both tables listed with `classes` and `gateways`.
pub(super) fn reconciler(
    settings: Settings,
    cluster: &Arc<Recorder>,
    classes: Vec<GatewayClass>,
    gateways: Vec<Gateway>,
) -> (Arc<Reconciler>, GatewayMetrics) {
    let metrics = GatewayMetrics::register(&mut Registry::default());
    let r = Reconciler::new(settings, cluster.clone(), metrics.clone());
    r.crd(true);
    r.feed_classes(Event::Init);
    for c in classes {
        r.feed_classes(Event::InitApply(key("", &c.name), c));
    }
    r.feed_classes(Event::InitDone);
    r.feed_gateways(Event::Init);
    for g in gateways {
        r.feed_gateways(Event::InitApply(key(&g.namespace, &g.name), g));
    }
    r.feed_gateways(Event::InitDone);
    (r, metrics)
}

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

/// What the cluster holds once the body of a Gateway status apply is written.
fn written(mut gw: Gateway, body: &Value) -> Gateway {
    let list = |f: &str| body["status"][f].as_array().cloned().unwrap_or_default();
    gw.observed.conditions = list("conditions");
    gw.observed.listeners = list("listeners");
    gw.observed.addresses = list("addresses");
    gw
}

#[tokio::test]
async fn gw_15_a_not_listed_gateway_gets_its_status_and_no_object_request() {
    let cluster = Arc::new(Recorder::default());
    let web = gateway("other", "web", vec![listener("http", 80, "HTTP")]);
    let (r, _) = reconciler(
        settings(&["shop"]),
        &cluster,
        vec![class("mimic")],
        vec![web.clone()],
    );
    assert_eq!(r.pass(NOW).await, None);
    assert_eq!(
        cluster.whats(),
        ["status class mimic", "status gateway other/web"]
    );
    let calls = cluster.calls();
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
    let mut mimic = class("mimic");
    mimic.observed = calls[0].body["status"]["conditions"]
        .as_array()
        .unwrap()
        .clone();
    r.feed_classes(Event::Apply(key("", "mimic"), mimic));
    r.feed_gateways(Event::Apply(key("other", "web"), written(web, body)));
    cluster.calls.lock().unwrap().clear();
    assert_eq!(r.pass(NOW).await, None);
    assert_eq!(cluster.whats(), Vec::<String>::new());
}

fn object(value: Value) -> DynamicObject {
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn gw_15_other_controllers_get_no_request() {
    let cluster = Arc::new(Recorder::default());
    let (r, _) = reconciler(settings(&["shop"]), &cluster, vec![], vec![]);
    let istio = object(json!({
        "apiVersion": "gateway.networking.k8s.io/v1", "kind": "GatewayClass",
        "metadata": {"name": "istio", "generation": 1},
        "spec": {"controllerName": "istio.io/gateway-controller"},
    }));
    if let Some((k, c)) = convert::class(&istio) {
        r.feed_classes(Event::Apply(k, c));
    }
    let gw = object(json!({
        "apiVersion": "gateway.networking.k8s.io/v1", "kind": "Gateway",
        "metadata": {"namespace": "shop", "name": "edge", "uid": "u", "generation": 1},
        "spec": {"gatewayClassName": "istio",
                 "listeners": [{"name": "tcp", "port": 9000, "protocol": "TCP"}]},
    }));
    let (k, g) = convert::gateway(&gw).expect("every Gateway converts");
    r.feed_gateways(Event::Apply(k, g));
    assert_eq!(r.pass(NOW).await, None);
    assert_eq!(cluster.whats(), Vec::<String>::new());
}

#[tokio::test]
async fn gw_15_no_pass_without_the_crd_or_a_full_list() {
    let cluster = Arc::new(Recorder::default());
    let refused = gateway("shop", "web", vec![listener("tcp", 9000, "TCP")]);
    let metrics = GatewayMetrics::register(&mut Registry::default());
    let r = Reconciler::new(settings(&["shop"]), cluster.clone(), metrics);
    r.crd(true);
    r.feed_classes(Event::Init);
    r.feed_classes(Event::InitApply(key("", "mimic"), class("mimic")));
    r.feed_classes(Event::InitDone);
    r.feed_gateways(Event::Init);
    r.feed_gateways(Event::InitApply(key("shop", "web"), refused.clone()));
    r.pass(NOW).await;
    assert_eq!(cluster.whats(), Vec::<String>::new(), "gateways not listed");
    r.feed_gateways(Event::InitDone);
    r.crd(false);
    r.pass(NOW).await;
    assert_eq!(cluster.whats(), Vec::<String>::new(), "the CRD absent");
    // Back: the table was emptied, and a pass waits for a new full list.
    r.crd(true);
    r.pass(NOW).await;
    assert_eq!(cluster.whats(), Vec::<String>::new(), "no list since");
    r.feed_gateways(Event::Init);
    r.feed_gateways(Event::InitApply(key("shop", "web"), refused));
    r.feed_gateways(Event::InitDone);
    r.pass(NOW).await;
    assert_eq!(
        cluster.whats(),
        ["status class mimic", "status gateway shop/web"]
    );
}

#[tokio::test]
async fn gw_15_an_accepted_gateway_or_one_of_an_unknown_class_gets_nothing_here() {
    let cluster = Arc::new(Recorder::default());
    let accepted = gateway("shop", "web", vec![listener("http", 80, "HTTP")]);
    let mut stray = gateway("shop", "stray", vec![listener("tcp", 9000, "TCP")]);
    stray.class = "unknown".into();
    let mut mimic = class("mimic");
    mimic.observed = vec![
        json!({"type": "Accepted", "status": "True", "reason": "Accepted",
        "message": "", "observedGeneration": 1, "lastTransitionTime": NOW}),
    ];
    let (r, metrics) = reconciler(
        settings(&["shop"]),
        &cluster,
        vec![mimic],
        vec![accepted, stray],
    );
    r.pass(NOW).await;
    assert_eq!(cluster.whats(), Vec::<String>::new());
    assert_eq!(count(&metrics, "invalid"), 0);
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
    let cluster = Arc::new(Recorder::default());
    let mut registry = Registry::with_prefix("mimic");
    let metrics = GatewayMetrics::register(&mut registry);
    let r = Reconciler::new(settings(&["shop"]), cluster.clone(), metrics.clone());
    r.crd(true);
    r.feed_classes(Event::Init);
    r.feed_classes(Event::InitApply(key("", "mimic"), class("mimic")));
    r.feed_classes(Event::InitDone);
    r.feed_gateways(Event::Init);
    r.feed_gateways(Event::InitApply(
        key("shop", "web"),
        gateway("shop", "web", vec![listener("tcp", 9000, "TCP")]),
    ));
    r.feed_gateways(Event::InitDone);
    r.pass(NOW).await;
    assert_eq!(
        cluster.whats(),
        ["status class mimic", "status gateway shop/web"]
    );
    let results: Vec<u64> = ["ok", "invalid", "conflict", "error"]
        .iter()
        .map(|r| count(&metrics, r))
        .collect();
    assert_eq!(results, [0, 1, 0, 0]);
    let states: Vec<i64> = ["programmed", "pending", "not_accepted"]
        .iter()
        .map(|s| state(&metrics, s))
        .collect();
    assert_eq!(states, [0, 0, 1]);
    let mut encoded = String::new();
    prometheus_client::encoding::text::encode(&mut encoded, &registry).unwrap();
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
    r.feed_gateways(Event::Delete(key("shop", "web")));
    r.pass(NOW).await;
    assert_eq!(state(&metrics, "not_accepted"), 0);
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

#[tokio::test(start_paused = true)]
async fn gw_15_a_failed_status_apply_is_logged_at_debug_and_tried_again_after_30_s() {
    let lines = Lines::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(lines.clone())
            .finish(),
    );
    let cluster = Arc::new(Recorder::default());
    *cluster.fail.lock().unwrap() = true;
    let (r, _) = reconciler(
        settings(&["shop"]),
        &cluster,
        vec![],
        vec![gateway("other", "web", vec![])],
    );
    r.feed_classes(Event::Apply(key("", "mimic"), class("mimic")));
    let start = Instant::now();
    let due = r.pass(NOW).await.expect("a retry");
    assert_eq!(due - start, Duration::from_secs(30));
    let text = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
    let debug: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("gateway status not applied"))
        .collect();
    assert_eq!(debug.len(), 2, "the class and the Gateway");
    assert!(debug.iter().all(|l| l.contains(r#""level":"DEBUG""#)));
    assert!(debug[1].contains(r#""gateway":"other/web""#), "the key");
    assert!(
        debug[1].contains(r#""error":"InternalError 500""#),
        "the error"
    );
}
