//! GW-05, GW-06 and GW-07: the objects of an accepted Gateway (spec §7.10 "Generated objects").
//! The goldens are written by hand from §7.10, never from the output.

use serde_json::{Value, json};

use super::accept::gateway;
use super::input::Gateway;
use super::objects::{Objects, objects};
use super::testkit::*;
use super::{ServiceType, Settings};

/// `shop/web` with HTTP listeners `http` 80 and `alt` 8080 and the replicas annotation `3`.
fn fixture() -> Gateway {
    let mut g = gw();
    g.listeners.push(http("alt", 8080));
    g.infrastructure.annotation_keys = vec![super::REPLICAS_ANNOTATION.into()];
    g.infrastructure.replicas = Some("3".into());
    g
}

fn build(g: &Gateway, s: &Settings) -> Option<Objects> {
    objects(g, &gateway(g, &class(), s), s)
}

fn built(g: &Gateway) -> Objects {
    build(g, &settings()).expect("an accepted Gateway has objects")
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).expect("a Value serialises") + "\n"
}

fn container(o: &Objects) -> &Value {
    &o.deployment["spec"]["template"]["spec"]["containers"][0]
}

fn env(o: &Objects, name: &str) -> Value {
    container(o)["env"]
        .as_array()
        .and_then(|env| env.iter().find(|e| e["name"] == name))
        .map(|e| e["value"].clone())
        .unwrap_or(Value::Null)
}

fn container_ports(o: &Objects) -> Vec<Value> {
    container(o)["ports"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[test]
fn gw_05_deployment_is_the_golden_json() {
    assert_eq!(
        pretty(&built(&fixture()).deployment),
        include_str!("golden/deployment.json")
    );
}

#[test]
fn gw_05_replicas_default_and_refused_listeners_add_no_port() {
    let mut g = fixture();
    g.infrastructure = Default::default();
    g.listeners.push(listener("tcp", "TCP", 9000));
    let o = built(&g);
    assert_eq!(o.deployment["spec"]["replicas"], json!(2));
    assert_eq!(env(&o, "MIMIC_GATEWAY_PORTS"), json!("80,8080"));
    assert_eq!(
        container_ports(&o),
        vec![
            json!({"containerPort": 80, "protocol": "TCP"}),
            json!({"containerPort": 8080, "protocol": "TCP"}),
            json!({"containerPort": 15100, "name": "mimic-admin", "protocol": "TCP"}),
        ]
    );
    assert!(!o.service.to_string().contains("9000"));
}

#[test]
fn gw_05_the_log_value_is_literal() {
    let mut s = settings();
    s.log = "info,$(X)".into();
    let o = build(&fixture(), &s).expect("accepted");
    assert_eq!(env(&o, "MIMIC_LOG"), json!("info,$$(X)"));
}

#[test]
fn gw_06_service_and_pdb_are_golden() {
    let o = built(&fixture());
    assert_eq!(pretty(&o.service), include_str!("golden/service.json"));
    assert_eq!(
        pretty(&o.pod_disruption_budget),
        include_str!("golden/pod_disruption_budget.json")
    );
}

#[test]
fn gw_06_the_service_type_comes_from_the_settings() {
    let golden: Value =
        serde_json::from_str(include_str!("golden/service.json")).expect("the golden is JSON");
    for (t, name) in [
        (ServiceType::ClusterIP, "ClusterIP"),
        (ServiceType::NodePort, "NodePort"),
        (ServiceType::LoadBalancer, "LoadBalancer"),
    ] {
        assert_eq!(t.as_str(), name);
        let mut s = settings();
        s.service_type = t;
        let mut want = golden.clone();
        want["spec"]["type"] = json!(name);
        assert_eq!(
            build(&fixture(), &s).expect("accepted").service,
            want,
            "{name}"
        );
    }
}

#[test]
fn gw_06_one_port_per_distinct_listener_port() {
    let mut g = gw();
    let mut a = http("a", 80);
    a.hostname = Some("a.example".into());
    let mut b = http("b", 80);
    b.hostname = Some("b.example".into());
    g.listeners = vec![a, b];
    let o = built(&g);
    assert_eq!(
        o.service["spec"]["ports"],
        json!([{"name": "http-80", "port": 80, "protocol": "TCP", "targetPort": 80}])
    );
    assert_eq!(container_ports(&o).len(), 2);
    assert_eq!(
        container_ports(&o)[0],
        json!({"containerPort": 80, "protocol": "TCP"})
    );
    assert_eq!(env(&o, "MIMIC_GATEWAY_PORTS"), json!("80"));
}

#[test]
fn gw_07_listener_ports_ascending_with_the_admin_port_last() {
    let mut g = gw();
    g.listeners = vec![http("top", 65535), http("http", 80)];
    let o = built(&g);
    let ports: Vec<Value> = container_ports(&o)
        .iter()
        .map(|p| p["containerPort"].clone())
        .collect();
    assert_eq!(ports, [json!(80), json!(65535), json!(15100)]);
    assert_eq!(env(&o, "MIMIC_GATEWAY_PORTS"), json!("80,65535"));
}

#[test]
fn gw_07_hashed_names_and_label_values() {
    let mut g = fixture();
    g.name = "web.prod.example".into();
    let o = built(&g);
    for object in [&o.deployment, &o.service, &o.pod_disruption_budget] {
        assert_eq!(
            object["metadata"]["name"],
            json!("web-prod-example-f17b22d722-mimic")
        );
        assert_eq!(
            object["metadata"]["labels"]["mimic.platform.io/gateway"],
            json!("web.prod.example")
        );
        assert_eq!(
            object["metadata"]["labels"]["gateway.networking.k8s.io/gateway-name"],
            json!("web.prod.example")
        );
        assert_eq!(
            object["metadata"]["ownerReferences"][0]["name"],
            json!("web.prod.example")
        );
    }
}

#[test]
fn gw_07_a_refused_gateway_has_no_object() {
    let mut g = fixture();
    g.listeners = vec![listener("tcp", "TCP", 9000)];
    assert_eq!(build(&g, &settings()), None);
    let mut g = fixture();
    g.namespace = "other".into();
    assert_eq!(build(&g, &settings()), None);
}

#[test]
fn gw_07_listener_order_is_not_read() {
    let mut g = fixture();
    let a = built(&g);
    g.listeners.reverse();
    let b = built(&g);
    assert_eq!(a.deployment.to_string(), b.deployment.to_string());
    assert_eq!(a.service.to_string(), b.service.to_string());
}
