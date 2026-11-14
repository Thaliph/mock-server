//! RTE-13 (the converter), RTE-15 (views follow routes) and RTE-19 (metrics) for HTTPRoutes,
//! without a cluster.

use std::time::Duration;

use kube::api::DynamicObject;
use mimic_core::Mode;
use mimic_core::mesh::input::{ParentRef, Route, RouteRule};
use mimic_core::mesh::route::compile;
use serde_json::json;

use super::convert;
use super::tests::{body_of, hub, key, put_route, put_service, svc, synced};
use super::{Answer, Event, MeshConfig};

const CANARY: &str = "canary-6e1b7a52";

fn dynamic(value: serde_json::Value) -> DynamicObject {
    serde_json::from_value(value).unwrap()
}

fn object() -> serde_json::Value {
    json!({
        "apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute",
        "metadata": {"namespace": "shop", "name": "split", "uid": "uid-1", "generation": 4,
                     "creationTimestamp": "2026-11-12T10:00:00Z",
                     "labels": {"team": CANARY}, "annotations": {"note": CANARY}},
        "spec": {
            "parentRefs": [
                {"group": "", "kind": "Service", "name": "cart", "port": 80},
                {"name": "edge", "namespace": "infra", "sectionName": "web"}],
            "hostnames": ["cart.example.com"],
            "rules": [{
                "name": "main",
                "matches": [{"path": {"type": "Exact", "value": "/a"},
                             "headers": [{"name": "x-env", "value": "beta"}],
                             "queryParams": [{"type": "RegularExpression", "name": "q", "value": "1"}],
                             "method": "GET"},
                            {}],
                "filters": [
                    {"type": "RequestHeaderModifier", "requestHeaderModifier": {
                        "set": [{"name": "x-a", "value": "1"}], "add": [{"name": "x-b", "value": "2"}],
                        "remove": ["x-c"]}},
                    {"type": "RequestMirror", "requestMirror": {
                        "backendRef": {"name": "shadow", "namespace": "ops", "port": 80},
                        "fraction": {"numerator": 1, "denominator": 10}}},
                    {"type": "RequestMirror", "requestMirror": {
                        "backendRef": {"name": "shadow", "port": 80}, "percent": 5}}],
                "backendRefs": [
                    {"name": "cart-v1", "port": 80, "weight": 90},
                    {"group": "example.com", "kind": "Thing", "name": "x", "namespace": "n",
                     "filters": [{"type": "RequestHeaderModifier"}]}],
                "timeouts": {"request": "5s", "backendRequest": "2s"},
                "retry": {"attempts": 3}}]
        },
        "status": {"parents": [{"controllerName": CANARY}]}
    })
}

#[test]
fn rte_13_an_httproute_converts_to_the_input_route() {
    let (k, r) = convert::route(&dynamic(object())).expect("a route");
    assert_eq!(k, key("shop", "split"));
    assert_eq!(
        (
            r.namespace.as_str(),
            r.name.as_str(),
            r.uid.as_str(),
            r.generation,
            r.created
        ),
        ("shop", "split", "uid-1", 4, 1_794_477_600)
    );
    assert!(
        !r.oversize && r.unknown_fields.is_empty(),
        "{:?}",
        r.unknown_fields
    );
    assert_eq!(
        r.parents,
        [
            ParentRef {
                group: String::new(),
                kind: "Service".into(),
                namespace: None,
                name: "cart".into(),
                section_name: None,
                port: Some(80),
            },
            ParentRef {
                group: "gateway.networking.k8s.io".into(),
                kind: "Gateway".into(),
                namespace: Some("infra".into()),
                name: "edge".into(),
                section_name: Some("web".into()),
                port: None,
            },
        ]
    );
    let rule = &r.rules[0];
    assert_eq!(rule.name.as_deref(), Some("main"));
    assert_eq!(rule.matches.len(), 2);
    let m = &rule.matches[0];
    let path = m.path.as_ref().unwrap();
    assert_eq!((path.kind.as_str(), path.value.as_str()), ("Exact", "/a"));
    assert_eq!(
        (
            m.headers[0].kind.as_str(),
            m.headers[0].name.as_str(),
            m.headers[0].value.as_str()
        ),
        ("Exact", "x-env", "beta")
    );
    assert_eq!(m.query_params[0].kind, "RegularExpression");
    assert_eq!(m.method.as_deref(), Some("GET"));
    assert!(rule.matches[1].path.is_none() && rule.matches[1].headers.is_empty());
    let modifier = rule.filters[0].request_header_modifier.as_ref().unwrap();
    assert_eq!(rule.filters[0].kind, "RequestHeaderModifier");
    assert_eq!(modifier.set, [("x-a".to_string(), "1".to_string())]);
    assert_eq!(modifier.add, [("x-b".to_string(), "2".to_string())]);
    assert_eq!(modifier.remove, ["x-c"]);
    let mirror = rule.filters[1].request_mirror.as_ref().unwrap();
    assert_eq!(
        (
            mirror.name.as_str(),
            mirror.namespace.as_deref(),
            mirror.port,
            mirror.fraction
        ),
        ("shadow", Some("ops"), Some(80), Some((1, Some(10))))
    );
    assert_eq!(
        rule.filters[2].request_mirror.as_ref().unwrap().percent,
        Some(5)
    );
    let b = &rule.backend_refs;
    assert_eq!(
        (
            b[0].group.as_str(),
            b[0].kind.as_str(),
            b[0].name.as_str(),
            b[0].port,
            b[0].weight
        ),
        ("", "Service", "cart-v1", Some(80), Some(90))
    );
    assert!(!b[0].has_filters && b[1].has_filters);
    assert_eq!(
        (b[1].group.as_str(), b[1].kind.as_str()),
        ("example.com", "Thing")
    );
    assert_eq!(rule.request_timeout.as_deref(), Some("5s"));
    assert_eq!(rule.backend_request_timeout.as_deref(), Some("2s"));
    assert!(rule.has_retry);
}

#[test]
fn rte_13_a_field_the_converter_does_not_know_is_kept_as_its_path_only() {
    let mut o = object();
    o["spec"]["rules"][0]["sessionPersistence"] = json!({"sessionName": CANARY, "type": "Cookie"});
    o["spec"]["rules"][0]["matches"][0]["madeUp"] = json!(CANARY);
    o["spec"]["madeUp"] = json!({"a": CANARY});
    let r = convert::route(&dynamic(o)).unwrap().1;
    assert_eq!(
        r.unknown_fields,
        [
            "madeUp",
            "rules[0].sessionPersistence",
            "rules[0].matches[0].madeUp"
        ]
    );
    assert!(!format!("{r:?}").contains(CANARY));
    // It is what makes the route not programmed, naming the first path (RTE-02).
    let compiled = compile(&r, &[], None);
    assert!(!compiled.programmed);
}

#[test]
fn rte_13_a_spec_over_64_kib_converts_to_the_key_generation_and_parents_only() {
    let mut o = object();
    o["spec"]["rules"][0]["matches"][0]["headers"][0]["value"] = json!("v".repeat(70 * 1024));
    let r = convert::route(&dynamic(o)).unwrap().1;
    assert!(r.oversize);
    assert!(r.rules.is_empty() && r.unknown_fields.is_empty());
    assert_eq!((r.generation, r.parents.len()), (4, 2));
    assert!(!compile(&r, &[], None).programmed);
    // Just under the cap converts whole.
    let mut o = object();
    o["spec"]["rules"][0]["matches"][0]["headers"][0]["value"] = json!("v".repeat(60 * 1024));
    assert!(!convert::route(&dynamic(o)).unwrap().1.oversize);
}

#[test]
fn rte_13_labels_annotations_and_status_are_in_no_input_struct() {
    let mut other = object();
    other["metadata"]["labels"] = json!({"team": "other"});
    other["metadata"]["annotations"] = json!({});
    other["metadata"]["resourceVersion"] = json!("999");
    other["metadata"]["managedFields"] = json!([{"manager": "x"}]);
    other["status"] = json!({"parents": []});
    let a = convert::route(&dynamic(object())).unwrap();
    let b = convert::route(&dynamic(other)).unwrap();
    assert_eq!(a, b);
    assert!(!format!("{a:?}").contains(CANARY));
}

#[test]
fn rte_13_an_object_without_namespace_or_name_is_skipped_and_a_bare_spec_converts() {
    let mut o = object();
    o["metadata"].as_object_mut().unwrap().remove("namespace");
    assert!(convert::route(&dynamic(o)).is_none());
    let bare = dynamic(json!({
        "apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute",
        "metadata": {"namespace": "shop", "name": "bare"}, "spec": {}}));
    let r = convert::route(&bare).unwrap().1;
    assert!(r.parents.is_empty() && r.rules.is_empty() && r.unknown_fields.is_empty());
}

pub(crate) fn cart_route(name: &str, created: i64) -> Route {
    Route {
        namespace: "shop".into(),
        name: name.into(),
        uid: format!("uid-{name}"),
        generation: 1,
        created,
        parents: vec![ParentRef {
            group: String::new(),
            kind: "Service".into(),
            namespace: None,
            name: "cart".into(),
            section_name: None,
            port: None,
        }],
        rules: vec![RouteRule {
            backend_refs: vec![mimic_core::mesh::input::BackendRef {
                group: String::new(),
                kind: "Service".into(),
                namespace: None,
                name: "cart".into(),
                port: Some(80),
                weight: None,
                has_filters: false,
            }],
            ..RouteRule::default()
        }],
        unknown_fields: vec![],
        oversize: false,
    }
}

fn builds(hub: &super::Hub) -> u64 {
    hub.metrics()
        .builds
        .get_or_create(&vec![("result".to_string(), "ok".to_string())])
        .get()
}

#[tokio::test]
async fn rte_15_a_route_changes_the_view_and_its_deletion_removes_it() {
    let hub = hub(MeshConfig::default());
    synced(&hub);
    put_service(&hub, svc("shop", "cart", "10.96.0.1"));
    put_service(&hub, svc("other", "cart", "10.96.0.9"));
    hub.publish_now();
    let (before, json_before) = body_of(&hub, "shop", "cart", Mode::None).await;
    assert!(!json_before.contains("shop/split"));

    put_route(&hub, cart_route("split", 100));
    hub.publish_now();
    let (with, json) = body_of(&hub, "shop", "cart", Mode::None).await;
    assert_ne!(with, before, "the route changes the ETag");
    assert!(json.contains(r#""name":"shop/split""#), "{json}");
    // A caller it does not apply to keeps its view (a consumer route of `shop`).
    let (elsewhere, _) = body_of(&hub, "other", "cart", Mode::None).await;
    hub.feed_routes(Event::Delete(key("shop", "split")));
    hub.publish_now();
    let (again, json_again) = body_of(&hub, "shop", "cart", Mode::None).await;
    assert_eq!((again, json_again), (before, json_before));
    let (elsewhere_after, _) = body_of(&hub, "other", "cart", Mode::None).await;
    assert_eq!(elsewhere, elsewhere_after);
}

#[tokio::test(start_paused = true)]
async fn rte_15_a_route_wakes_a_held_request_within_the_debounce_plus_100_ms() {
    let hub = hub(MeshConfig::default());
    synced(&hub);
    put_service(&hub, svc("shop", "cart", "10.96.0.1"));
    hub.publish_now();
    tokio::spawn(hub.clone().run());
    let (etag, _) = body_of(&hub, "shop", "cart", Mode::None).await;
    let start = tokio::time::Instant::now();
    let asking = tokio::spawn({
        let (hub, etag) = (hub.clone(), etag.clone());
        async move {
            let held = [etag.as_str()];
            match hub
                .answer("shop", "pod-b", "cart", &|| Mode::None, &held, &|| {})
                .await
            {
                Answer::View(entry) => (entry.etag.clone(), start.elapsed()),
                _ => panic!("not a view"),
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    put_route(&hub, cart_route("split", 100));
    let (new, waited) = asking.await.unwrap();
    assert_ne!(new, etag, "a new view");
    assert!(waited <= Duration::from_millis(210), "{waited:?}");
}

#[tokio::test]
async fn rte_15_a_change_of_status_alone_builds_no_view() {
    let hub = hub(MeshConfig::default());
    synced(&hub);
    put_service(&hub, svc("shop", "cart", "10.96.0.1"));
    put_route(&hub, cart_route("split", 100));
    hub.publish_now();
    let (etag, _) = body_of(&hub, "shop", "cart", Mode::None).await;
    let (epoch, built) = (hub.published().epoch, builds(&hub));
    // The watch delivers the object again with a new `resourceVersion` and `status`: the
    // converter gives the same route (RTE-13), which the hub does not count as a change.
    let mut changed = object();
    changed["status"] = json!({"parents": [{"controllerName": "mimic.platform.io/mesh"}]});
    changed["metadata"]["resourceVersion"] = json!("77");
    let same = convert::route(&dynamic(object())).unwrap();
    let again = convert::route(&dynamic(changed)).unwrap();
    hub.feed_routes(Event::Apply(same.0.clone(), same.1));
    hub.feed_routes(Event::Apply(again.0, again.1));
    assert_eq!(hub.published().epoch, epoch);
    let (etag_after, _) = body_of(&hub, "shop", "cart", Mode::None).await;
    assert_eq!((etag, builds(&hub)), (etag_after, built));
}

fn state(hub: &super::Hub, state: &str) -> i64 {
    hub.metrics()
        .routes
        .get_or_create(&vec![("state".to_string(), state.to_string())])
        .get()
}

fn objects(hub: &super::Hub, kind: &str) -> i64 {
    hub.metrics()
        .objects
        .get_or_create(&vec![("kind".to_string(), kind.to_string())])
        .get()
}

#[tokio::test]
async fn rte_19_objects_count_the_routes_held_and_routes_count_programmed_and_unsupported() {
    let hub = hub(MeshConfig::default());
    synced(&hub);
    put_service(&hub, svc("shop", "cart", "10.96.0.1"));
    put_route(&hub, cart_route("ok-1", 1));
    put_route(&hub, cart_route("ok-2", 2));
    let mut odd = cart_route("odd", 3);
    odd.unknown_fields = vec!["rules[0].sessionPersistence".into()];
    put_route(&hub, odd);
    // A route with a Gateway parent only: held, but in neither state.
    let mut gateway = cart_route("gw", 4);
    gateway.parents[0].kind = "Gateway".into();
    gateway.parents[0].group = "gateway.networking.k8s.io".into();
    put_route(&hub, gateway);
    hub.publish_now();
    assert_eq!(objects(&hub, "httproute"), 4);
    assert_eq!(state(&hub, "programmed"), 2);
    assert_eq!(state(&hub, "unsupported"), 1);
    hub.feed_routes(Event::Delete(key("shop", "odd")));
    hub.publish_now();
    assert_eq!(
        (
            objects(&hub, "httproute"),
            state(&hub, "programmed"),
            state(&hub, "unsupported")
        ),
        (3, 2, 0)
    );
}

#[tokio::test]
async fn rte_19_the_metrics_are_registered_with_their_names() {
    let mut registry = prometheus_client::registry::Registry::default();
    let metrics = super::MeshMetrics::register(&mut registry);
    metrics.gateway_api_present.set(1);
    metrics
        .routes
        .get_or_create(&vec![("state".to_string(), "programmed".to_string())])
        .set(2);
    metrics
        .route_status
        .get_or_create(&vec![("result".to_string(), "conflict".to_string())])
        .inc();
    let mut text = String::new();
    prometheus_client::encoding::text::encode(&mut text, &registry).unwrap();
    assert!(text.contains("mesh_gateway_api_present 1"), "{text}");
    assert!(
        text.contains(r#"mesh_routes{state="programmed"} 2"#),
        "{text}"
    );
    assert!(
        text.contains(r#"mesh_route_status_total{result="conflict"} 1"#),
        "{text}"
    );
}
