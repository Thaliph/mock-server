//! RTE-14 (the entries to write), RTE-16 (two replicas against a fake status API), RTE-17
//! (failures) and RTE-24 (nothing of other controllers touched, nothing but `status.parents`
//! written), without a cluster.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kube::api::DynamicObject;
use mimic_core::mesh::input::{
    BackendRef, GATEWAY_GROUP, GrantFrom, GrantTo, NameMatch, ParentRef, PathMatch, ReferenceGrant,
    Route, RouteFilter, RouteMatch, RouteRule,
};
use mimic_core::mesh::route::compile;
use serde_json::{Value, json};

use super::convert::{self, RouteObserved};
use super::metrics::label;
use super::status::{
    MIMIC_CONTROLLER, PatchError, PatchFuture, Plan, RouteSink, RouteState, plan_route, route_body,
    route_condition, route_pass, route_patch_params, run_routes,
};
use super::tests::{Lines, hub, key, put_grant, put_route, put_service, svc, synced};
use super::{Event, Hub, MeshConfig};

const NOW: &str = "2026-11-14T10:00:00Z";
const LATER: &str = "2026-11-14T11:30:00Z";
const OLD: &str = "2026-11-01T08:00:00Z";
const OTHER: &str = "example.com/other";
const CANARY: &str = "canary-93d0c4aa";

fn parent(name: &str) -> ParentRef {
    ParentRef {
        group: String::new(),
        kind: "Service".into(),
        namespace: None,
        name: name.into(),
        section_name: None,
        port: None,
    }
}

fn route(name: &str, parents: Vec<ParentRef>, rules: Vec<RouteRule>) -> Route {
    Route {
        namespace: "shop".into(),
        name: name.into(),
        uid: format!("uid-{name}"),
        generation: 3,
        created: 100,
        parents,
        rules,
        unknown_fields: vec![],
        oversize: false,
    }
}

fn backend(namespace: Option<&str>, name: &str) -> BackendRef {
    BackendRef {
        group: String::new(),
        kind: "Service".into(),
        namespace: namespace.map(Into::into),
        name: name.into(),
        port: Some(80),
        weight: None,
        has_filters: false,
    }
}

fn to(backends: Vec<BackendRef>) -> Vec<RouteRule> {
    vec![RouteRule {
        backend_refs: backends,
        ..RouteRule::default()
    }]
}

fn services() -> Vec<mimic_core::mesh::input::Service> {
    vec![
        svc("shop", "cart", "10.96.0.1"),
        svc("shop", "pay", "10.96.0.2"),
    ]
}

fn plan(route: &Route, observed: &[Value], now: &str) -> Plan {
    let compiled = compile(route, &services(), Some(&[]));
    plan_route(route, &compiled, observed, now)
}

fn written(plan: Plan) -> Vec<Value> {
    match plan {
        Plan::Write(parents) => parents,
        other => panic!("not a write: {other:?}"),
    }
}

fn cond(kind: &str, ok: bool, reason: &str, message: &str, generation: i64, at: &str) -> Value {
    json!({"type": kind, "status": if ok { "True" } else { "False" }, "reason": reason,
           "message": message, "observedGeneration": generation, "lastTransitionTime": at})
}

fn entry(parent_ref: Value, controller: &str, conditions: Vec<Value>) -> Value {
    json!({"parentRef": parent_ref, "controllerName": controller, "conditions": conditions})
}

fn service_ref(name: &str) -> Value {
    json!({"group": "", "kind": "Service", "name": name})
}

fn mimic(name: &str, conditions: Vec<Value>) -> Value {
    entry(service_ref(name), MIMIC_CONTROLLER, conditions)
}

fn other(name: &str) -> Value {
    json!({"parentRef": {"group": "", "kind": "Service", "name": name}, "controllerName": OTHER,
           "conditions": [cond("Accepted", true, "Accepted", "", 1, OLD)], "futureField": [1, 2]})
}

fn ok_pair(generation: i64, at: &str) -> Vec<Value> {
    vec![
        cond("Accepted", true, "Accepted", "", generation, at),
        cond("ResolvedRefs", true, "ResolvedRefs", "", generation, at),
    ]
}

#[test]
fn rte_14_the_list_keeps_other_controllers_first_and_puts_mimics_after_in_parent_order() {
    let mut cart = parent("cart");
    cart.namespace = Some("shop".into());
    cart.port = Some(80);
    let mut pay = parent("pay");
    pay.section_name = Some("web".into());
    let gateway = ParentRef {
        group: GATEWAY_GROUP.into(),
        kind: "Gateway".into(),
        namespace: Some("infra".into()),
        name: "edge".into(),
        section_name: None,
        port: None,
    };
    let r = route("r", vec![cart, gateway, pay], vec![]);
    let (first, second) = (other("cart"), other("pay"));
    let parents = written(plan(&r, &[first.clone(), second.clone()], NOW));
    assert_eq!(parents.len(), 4);
    assert_eq!((&parents[0], &parents[1]), (&first, &second));
    assert_eq!(
        parents[2],
        entry(
            json!({"group": "", "kind": "Service", "namespace": "shop", "name": "cart", "port": 80}),
            MIMIC_CONTROLLER,
            ok_pair(3, NOW)
        )
    );
    assert_eq!(
        parents[3],
        entry(
            json!({"group": "", "kind": "Service", "name": "pay", "sectionName": "web"}),
            MIMIC_CONTROLLER,
            vec![
                cond(
                    "Accepted",
                    false,
                    "NoMatchingParent",
                    "sectionName is not supported for a Service parent",
                    3,
                    NOW
                ),
                cond("ResolvedRefs", true, "ResolvedRefs", "", 3, NOW)
            ]
        )
    );
}

#[test]
fn rte_14_last_transition_time_is_kept_while_the_status_is_unchanged() {
    let r = route("r", vec![parent("cart")], to(vec![backend(None, "gone")]));
    // Accepted stays True, ResolvedRefs stays False with another reason: both keep the
    // cluster's time, the new reason and generation are written.
    let before = mimic(
        "cart",
        vec![
            cond("Accepted", true, "Accepted", "", 2, OLD),
            cond("ResolvedRefs", false, "InvalidKind", "old words", 2, OLD),
        ],
    );
    let parents = written(plan(&r, &[before], NOW));
    let conditions = &parents[0]["conditions"];
    assert_eq!(
        conditions[0],
        cond("Accepted", true, "Accepted", "", 3, OLD)
    );
    assert_eq!(
        conditions[1],
        cond(
            "ResolvedRefs",
            false,
            "BackendNotFound",
            "shop/gone",
            3,
            OLD
        )
    );
    // ResolvedRefs was True: the status changes, the clock's value is written.
    let before = mimic("cart", ok_pair(2, OLD));
    let parents = written(plan(&r, &[before], NOW));
    let conditions = &parents[0]["conditions"];
    assert_eq!(conditions[0]["lastTransitionTime"], OLD);
    assert_eq!(conditions[1]["lastTransitionTime"], NOW);
    assert_eq!(conditions[1]["status"], "False");
}

#[test]
fn rte_14_only_a_last_transition_time_of_unchanged_conditions_is_no_write() {
    let r = route("r", vec![parent("cart")], vec![]);
    let fresh = written(plan(&r, &[other("x")], NOW));
    // The cluster holds the same entries with older times: nothing to write, whatever the clock.
    let mut held = fresh.clone();
    for c in held[1]["conditions"].as_array_mut().unwrap() {
        c["lastTransitionTime"] = json!(OLD);
    }
    assert_eq!(plan(&r, &held, LATER), Plan::Keep);
    // A generation behind is a write.
    held[1]["conditions"][0]["observedGeneration"] = json!(2);
    let parents = written(plan(&r, &held, LATER));
    assert_eq!(parents[1]["conditions"][0]["observedGeneration"], 3);
    assert_eq!(parents[1]["conditions"][0]["lastTransitionTime"], OLD);
    // Another controller's entry before ours, or after, is no reason to write.
    let mut after = fresh.clone();
    after.reverse();
    assert_eq!(plan(&r, &after, NOW), Plan::Keep);
}

#[test]
fn rte_14_entries_of_parents_gone_services_gone_and_routes_without_parents_are_removed() {
    let kept = other("cart");
    // A parent no longer listed.
    let r = route("r", vec![parent("cart")], vec![]);
    let held = [
        kept.clone(),
        mimic("cart", ok_pair(3, OLD)),
        mimic("old", ok_pair(3, OLD)),
    ];
    let parents = written(plan(&r, &held, NOW));
    assert_eq!(parents, [kept.clone(), mimic("cart", ok_pair(3, OLD))]);
    // A Service that is gone.
    let r = route("r", vec![parent("cart"), parent("missing")], vec![]);
    let held = [
        mimic("cart", ok_pair(3, OLD)),
        mimic("missing", ok_pair(3, OLD)),
    ];
    let parents = written(plan(&r, &held, NOW));
    assert_eq!(parents, [mimic("cart", ok_pair(3, OLD))]);
    // A route without Service parents: mimic's entries go, the others stay.
    let gateway = ParentRef {
        group: GATEWAY_GROUP.into(),
        kind: "Gateway".into(),
        namespace: None,
        name: "edge".into(),
        section_name: None,
        port: None,
    };
    let r = route("r", vec![gateway], vec![]);
    let held = [kept.clone(), mimic("cart", ok_pair(3, OLD))];
    assert_eq!(written(plan(&r, &held, NOW)), std::slice::from_ref(&kept));
    // ... and with none of mimic's there is nothing to write.
    assert_eq!(plan(&r, &[kept], NOW), Plan::Keep);
    assert_eq!(plan(&r, &[], NOW), Plan::Keep);
}

#[test]
fn rte_14_a_list_over_32_entries_is_not_written() {
    let r = route("r", vec![parent("cart"), parent("pay")], vec![]);
    let others = |n: usize| (0..n).map(|i| other(&format!("o{i}"))).collect::<Vec<_>>();
    assert_eq!(plan(&r, &others(31), NOW), Plan::TooMany);
    assert_eq!(written(plan(&r, &others(30), NOW)).len(), 32);
}

#[test]
fn rte_14_messages_are_cut_to_1024_bytes_on_a_character_boundary() {
    let long = mimic_core::mesh::policy::Condition {
        ok: false,
        reason: "BackendNotFound".into(),
        message: "€".repeat(400),
    };
    let c = route_condition("ResolvedRefs", &long, 3, None, NOW);
    let message = c["message"].as_str().unwrap();
    assert_eq!(message.len(), 1023);
    assert!(message.chars().all(|ch| ch == '€'));
    assert_eq!(c["status"], "False");
    assert_eq!(c["observedGeneration"], 3);
}

#[test]
fn rte_14_the_observed_status_converts_with_its_resource_version_and_generation() {
    let object = |status: Value| -> DynamicObject {
        serde_json::from_value(json!({
            "apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute",
            "metadata": {"namespace": "shop", "name": "split", "generation": 4,
                         "resourceVersion": "77"},
            "spec": {}, "status": status}))
        .unwrap()
    };
    let (k, seen) = convert::route_observed(&object(json!({"parents": [other("cart")]}))).unwrap();
    assert_eq!(k, key("shop", "split"));
    assert_eq!(
        seen,
        RouteObserved {
            generation: 4,
            resource_version: "77".into(),
            parents: vec![other("cart")]
        }
    );
    let (_, bare) = convert::route_observed(&object(Value::Null)).unwrap();
    assert!(bare.parents.is_empty());
}

#[test]
fn rte_14_a_change_of_observed_status_wakes_the_writer_and_builds_no_view() {
    let hub = hub(MeshConfig::default());
    synced(&hub);
    put_service(&hub, svc("shop", "cart", "10.96.0.1"));
    put_route(&hub, route("r", vec![parent("cart")], vec![]));
    hub.publish_now();
    let epoch = hub.published().epoch;
    let mut changes = hub.route_changes();
    changes.mark_unchanged();
    let seen = RouteObserved {
        generation: 3,
        resource_version: "5".into(),
        parents: vec![],
    };
    hub.feed_route_status(Event::Apply(key("shop", "r"), seen.clone()));
    assert!(changes.has_changed().unwrap());
    assert_eq!(hub.observed_route(&key("shop", "r")), Some(seen.clone()));
    // The same again: nothing changed.
    changes.mark_unchanged();
    hub.feed_route_status(Event::Apply(key("shop", "r"), seen));
    assert!(!changes.has_changed().unwrap());
    assert_eq!(hub.published().epoch, epoch);
}

#[test]
fn rte_24_the_patch_carries_the_resource_version_and_status_parents_only() {
    let parents = vec![other("cart"), mimic("cart", ok_pair(3, NOW))];
    let body = route_body("42", &parents);
    assert_eq!(
        body,
        json!({"metadata": {"resourceVersion": "42"}, "status": {"parents": parents}})
    );
    assert_eq!(
        route_patch_params().field_manager.as_deref(),
        Some("mimic-mesh")
    );
}

#[test]
fn rte_24_entries_of_other_controllers_are_written_back_as_read() {
    let r = route("r", vec![parent("cart")], vec![]);
    let mut odd = other("cart");
    odd["conditions"][0]["message"] = json!("é \u{1f600}");
    odd["unknown"] = json!({"nested": [null, 1.5, "x"]});
    let held = [odd.clone(), mimic("old", ok_pair(1, OLD)), other("pay")];
    let parents = written(plan(&r, &held, NOW));
    assert_eq!(parents[0], odd);
    assert_eq!(parents[1], other("pay"));
    assert!(
        parents[2..]
            .iter()
            .all(|e| e["controllerName"] == MIMIC_CONTROLLER)
    );
}

// RTE-16: two replicas, a fake status API.

struct Stored {
    route: Route,
    version: u64,
    parents: Vec<Value>,
}

struct Sent {
    replica: usize,
    route: String,
    ok: bool,
}

#[derive(Default)]
struct Api {
    objects: Mutex<BTreeMap<(String, String), Stored>>,
    sent: Mutex<Vec<Sent>>,
}

struct Replica {
    id: usize,
    api: Arc<Api>,
}

impl RouteSink for Replica {
    fn patch<'a>(&'a self, namespace: &'a str, name: &'a str, body: &'a Value) -> PatchFuture<'a> {
        Box::pin(async move {
            let top: Vec<_> = body.as_object().unwrap().keys().collect();
            assert_eq!(top, ["metadata", "status"], "top-level keys of the patch");
            assert_eq!(body["status"].as_object().unwrap().len(), 1);
            let read = body["metadata"]["resourceVersion"]
                .as_str()
                .expect("a resourceVersion")
                .to_string();
            assert_eq!(body["metadata"].as_object().unwrap().len(), 1);
            let mut objects = self.api.objects.lock().unwrap();
            let stored = objects.get_mut(&key(namespace, name)).expect("a route");
            let ok = read == stored.version.to_string();
            if ok {
                stored.version += 1;
                stored.parents = body["status"]["parents"].as_array().unwrap().clone();
            }
            self.api.sent.lock().unwrap().push(Sent {
                replica: self.id,
                route: format!("{namespace}/{name}"),
                ok,
            });
            if ok {
                Ok(())
            } else {
                Err(PatchError::Conflict)
            }
        })
    }
}

struct Pod {
    hub: Arc<Hub>,
    state: RouteState,
    sink: Replica,
}

impl Pod {
    fn new(id: usize, api: &Arc<Api>) -> Pod {
        let hub = hub(MeshConfig::default());
        synced(&hub);
        for s in services() {
            put_service(&hub, s);
        }
        put_service(&hub, svc("other", "pay", "10.96.1.1"));
        Pod {
            hub,
            state: RouteState::default(),
            sink: Replica {
                id,
                api: api.clone(),
            },
        }
    }

    /// The watch delivers what the cluster holds now, and the hub publishes it.
    fn read(&self) {
        let objects = self.sink.api.objects.lock().unwrap();
        for (k, stored) in objects.iter() {
            put_route(&self.hub, stored.route.clone());
            self.hub.feed_route_status(Event::Apply(
                k.clone(),
                RouteObserved {
                    generation: stored.route.generation,
                    resource_version: stored.version.to_string(),
                    parents: stored.parents.clone(),
                },
            ));
        }
        drop(objects);
        self.hub.publish_now();
    }

    async fn pass(&mut self) {
        route_pass(&self.hub, &mut self.state, &self.sink, NOW).await;
    }
}

fn ok_of(api: &Api, route: &str, replica: usize) -> usize {
    api.sent
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.route == route && s.replica == replica && s.ok)
        .count()
}

fn conditions_of(api: &Api, route: &str) -> Vec<(String, String, String)> {
    let (ns, name) = route.split_once('/').unwrap();
    let objects = api.objects.lock().unwrap();
    objects[&key(ns, name)]
        .parents
        .iter()
        .filter(|e| e["controllerName"] == MIMIC_CONTROLLER)
        .flat_map(|e| e["conditions"].as_array().unwrap().clone())
        .map(|c| {
            let text = |k: &str| c[k].as_str().unwrap().to_string();
            (text("type"), text("status"), text("reason"))
        })
        .collect()
}

fn t(kind: &str, status: &str, reason: &str) -> (String, String, String) {
    (kind.into(), status.into(), reason.into())
}

#[tokio::test]
async fn rte_16_two_replicas_converge_on_the_entries_with_at_most_one_success_each() {
    let api = Arc::new(Api::default());
    let rewrite = vec![RouteRule {
        filters: vec![RouteFilter {
            kind: "URLRewrite".into(),
            request_header_modifier: None,
            response_header_modifier: None,
            request_mirror: None,
        }],
        ..RouteRule::default()
    }];
    let routes = [
        route("ok", vec![parent("cart")], vec![]),
        route("rewrite", vec![parent("cart")], rewrite),
        route(
            "cross",
            vec![parent("cart")],
            to(vec![backend(Some("other"), "pay")]),
        ),
        route("shared", vec![parent("cart")], vec![]),
    ];
    let foreign = other("cart");
    for r in routes {
        let parents = if r.name == "shared" {
            vec![foreign.clone()]
        } else {
            vec![]
        };
        api.objects.lock().unwrap().insert(
            key("shop", &r.name),
            Stored {
                route: r,
                version: 10,
                parents,
            },
        );
    }
    let mut pods = [Pod::new(0, &api), Pod::new(1, &api)];
    // Both replicas read the same state, then act one after the other: the second one's
    // `resourceVersion` is behind.
    for _ in 0..5 {
        for pod in &pods {
            pod.read();
        }
        for pod in &mut pods {
            pod.pass().await;
        }
    }
    let sent = api.sent.lock().unwrap().len();
    assert!(sent >= 4, "{sent}");
    for name in ["ok", "rewrite", "cross", "shared"] {
        let route = format!("shop/{name}");
        for replica in 0..2 {
            assert!(ok_of(&api, &route, replica) <= 1, "{route} by {replica}");
        }
        assert_eq!(
            ok_of(&api, &route, 0) + ok_of(&api, &route, 1),
            1,
            "{route}: one write per transition"
        );
    }
    assert_eq!(
        conditions_of(&api, "shop/ok"),
        [
            t("Accepted", "True", "Accepted"),
            t("ResolvedRefs", "True", "ResolvedRefs")
        ]
    );
    assert_eq!(
        conditions_of(&api, "shop/rewrite")[0],
        t("Accepted", "False", "UnsupportedValue")
    );
    assert_eq!(
        conditions_of(&api, "shop/cross")[1],
        t("ResolvedRefs", "False", "RefNotPermitted")
    );
    {
        let objects = api.objects.lock().unwrap();
        let shared = &objects[&key("shop", "shared")].parents;
        assert_eq!(shared.len(), 2);
        assert_eq!(
            shared[0], foreign,
            "the other controller's entry is unchanged"
        );
        assert_eq!(shared[1]["controllerName"], MIMIC_CONTROLLER);
    }
    // Converged: ten more publications send nothing.
    let before = api.sent.lock().unwrap().len();
    for _ in 0..10 {
        for pod in &mut pods {
            pod.read();
            pod.pass().await;
        }
    }
    assert_eq!(
        api.sent.lock().unwrap().len(),
        before,
        "no patch once converged"
    );

    // A grant fed later: ResolvedRefs turns True, with one more patch per replica at most.
    let grant = ReferenceGrant {
        namespace: "other".into(),
        from: vec![GrantFrom {
            group: GATEWAY_GROUP.into(),
            kind: "HTTPRoute".into(),
            namespace: "shop".into(),
        }],
        to: vec![GrantTo {
            group: String::new(),
            kind: "Service".into(),
            name: None,
        }],
    };
    for pod in &pods {
        put_grant(&pod.hub, "g", grant.clone());
    }
    for _ in 0..5 {
        for pod in &pods {
            pod.read();
        }
        for pod in &mut pods {
            pod.pass().await;
        }
    }
    assert_eq!(
        conditions_of(&api, "shop/cross")[1],
        t("ResolvedRefs", "True", "ResolvedRefs")
    );
    for replica in 0..2 {
        assert!(ok_of(&api, "shop/cross", replica) <= 2, "one more at most");
    }
    assert_eq!(
        ok_of(&api, "shop/cross", 0) + ok_of(&api, "shop/cross", 1),
        2
    );
    let sent = api.sent.lock().unwrap();
    assert!(
        sent.iter().filter(|s| !s.ok).all(|s| s.replica == 1),
        "only the replica that read second conflicts"
    );
}

// RTE-17: failures.

#[derive(Default)]
struct Flaky {
    fail: Mutex<bool>,
    attempts: Mutex<Vec<String>>,
}

impl RouteSink for Flaky {
    fn patch<'a>(&'a self, namespace: &'a str, name: &'a str, _body: &'a Value) -> PatchFuture<'a> {
        Box::pin(async move {
            self.attempts
                .lock()
                .unwrap()
                .push(format!("{namespace}/{name}"));
            if *self.fail.lock().unwrap() {
                Err(PatchError::Other("500 Internal Server Error".into()))
            } else {
                Ok(())
            }
        })
    }
}

fn count(hub: &Hub, result: &str) -> u64 {
    hub.metrics()
        .route_status
        .get_or_create(&label("result", result))
        .get()
}

#[tokio::test(start_paused = true)]
async fn rte_17_a_failing_status_api_is_counted_warned_once_and_tried_again_after_30_s() {
    let lines = Lines::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .json()
            .with_writer(lines.clone())
            .finish(),
    );
    let hub = hub(MeshConfig::default());
    synced(&hub);
    put_service(&hub, svc("shop", "cart", "10.96.0.1"));
    let rules = vec![RouteRule {
        matches: vec![RouteMatch {
            path: Some(PathMatch {
                kind: "Exact".into(),
                value: format!("/{CANARY}"),
            }),
            headers: vec![NameMatch {
                kind: "Exact".into(),
                name: "x-env".into(),
                value: CANARY.into(),
            }],
            query_params: vec![NameMatch {
                kind: "Exact".into(),
                name: "q".into(),
                value: CANARY.into(),
            }],
            method: None,
        }],
        ..RouteRule::default()
    }];
    put_route(&hub, route("split", vec![parent("cart")], rules));
    hub.feed_route_status(Event::Apply(
        key("shop", "split"),
        RouteObserved {
            generation: 3,
            resource_version: "1".into(),
            parents: vec![],
        },
    ));
    hub.publish_now();
    let sink = Arc::new(Flaky::default());
    *sink.fail.lock().unwrap() = true;
    tokio::spawn(run_routes(hub.clone(), sink.clone()));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(sink.attempts.lock().unwrap().len(), 1);
    assert_eq!(count(&hub, "error"), 1);
    tokio::time::sleep(Duration::from_secs(28)).await;
    assert_eq!(sink.attempts.lock().unwrap().len(), 1, "not before 30 s");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(sink.attempts.lock().unwrap().len(), 2, "again after 30 s");
    assert_eq!(count(&hub, "error"), 2);
    // The API answers again: the next try succeeds, and nothing more is sent after it.
    *sink.fail.lock().unwrap() = false;
    tokio::time::sleep(Duration::from_secs(31)).await;
    assert_eq!(sink.attempts.lock().unwrap().len(), 3);
    assert_eq!((count(&hub, "ok"), count(&hub, "error")), (1, 2));
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(sink.attempts.lock().unwrap().len(), 3);
    let text = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
    let warns: Vec<_> = text
        .lines()
        .filter(|l| l.contains("httproute status failed"))
        .collect();
    assert_eq!(warns.len(), 1, "one WARN per route and streak");
    assert!(warns[0].contains(r#""route":"shop/split""#) && warns[0].contains("500"));
    assert!(!text.contains(CANARY), "no value of the route in a log");
}

#[tokio::test]
async fn rte_17_a_conflict_is_counted_and_not_tried_again_before_the_next_read() {
    let hub = hub(MeshConfig::default());
    synced(&hub);
    put_service(&hub, svc("shop", "cart", "10.96.0.1"));
    put_route(&hub, route("r", vec![parent("cart")], vec![]));
    let seen = |version: &str| RouteObserved {
        generation: 3,
        resource_version: version.into(),
        parents: vec![],
    };
    hub.feed_route_status(Event::Apply(key("shop", "r"), seen("1")));
    hub.publish_now();
    let api = Arc::new(Api::default());
    api.objects.lock().unwrap().insert(
        key("shop", "r"),
        Stored {
            route: route("r", vec![parent("cart")], vec![]),
            version: 2,
            parents: vec![],
        },
    );
    let sink = Replica { id: 0, api };
    let mut state = RouteState::default();
    route_pass(&hub, &mut state, &sink, NOW).await;
    route_pass(&hub, &mut state, &sink, NOW).await;
    assert_eq!(sink.api.sent.lock().unwrap().len(), 1, "once per read");
    assert_eq!((count(&hub, "conflict"), count(&hub, "error")), (1, 0));
    // The next read: decided again, and it succeeds.
    hub.feed_route_status(Event::Apply(key("shop", "r"), seen("2")));
    route_pass(&hub, &mut state, &sink, NOW).await;
    assert_eq!(count(&hub, "ok"), 1);
}
