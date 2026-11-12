//! MSH-20, MSH-22, MSH-25 (hub and statuses), MSH-26 and MSH-27 (relist): the hub of the mesh
//! view (spec §7.2, §7.9), without a cluster.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use mimic_core::Mode;
use mimic_core::mesh::input::{
    Config, Destination, Endpoint, EndpointSlice, GrantFrom, GrantTo, MIMIC_GROUP, ReferenceGrant,
    Service, ServicePort, SlicePort, Target, TargetKind, TargetPort,
};
use mimic_core::mesh::view::{Limits, View};
use tokio::time::Instant;

use super::status::{self, ApplyFuture, StatusSink};
use super::{Answer, Event, Hub, MeshConfig, MeshMetrics};

pub(crate) fn key(ns: &str, name: &str) -> (String, String) {
    (ns.to_string(), name.to_string())
}

pub(crate) fn svc(ns: &str, name: &str, ip: &str) -> Service {
    Service {
        namespace: ns.into(),
        name: name.into(),
        cluster_ips: vec![ip.into()],
        ports: vec![ServicePort {
            port: 80,
            name: Some("http".into()),
            target_port: TargetPort::Name("http".into()),
        }],
    }
}

pub(crate) fn slice(ns: &str, service: &str, name: &str, ips: &[&str]) -> EndpointSlice {
    EndpointSlice {
        namespace: ns.into(),
        name: name.into(),
        service: service.into(),
        generation: 1,
        ports: vec![SlicePort {
            name: Some("http".into()),
            port: 8080,
        }],
        endpoints: ips
            .iter()
            .map(|ip| Endpoint {
                ip: (*ip).into(),
                ready: true,
                serving: true,
                terminating: false,
                zone: None,
            })
            .collect(),
    }
}

pub(crate) fn config(ns: &str, name: &str, deployment: &str, dests: &[(&str, &str)]) -> Config {
    Config {
        namespace: ns.into(),
        name: name.into(),
        created: 100,
        generation: 1,
        targets: vec![Target {
            kind: TargetKind::Deployment,
            name: deployment.into(),
        }],
        destinations: Some(
            dests
                .iter()
                .map(|(namespace, name)| Destination {
                    namespace: (*namespace).into(),
                    name: (*name).into(),
                })
                .collect(),
        ),
        mesh_enabled: None,
        slow_start: None,
    }
}

pub(crate) fn grant(ns: &str, from_ns: &str) -> ReferenceGrant {
    ReferenceGrant {
        namespace: ns.into(),
        from: vec![GrantFrom {
            group: MIMIC_GROUP.into(),
            kind: "MimicConfig".into(),
            namespace: from_ns.into(),
        }],
        to: vec![GrantTo {
            group: String::new(),
            kind: "Service".into(),
            name: None,
        }],
    }
}

pub(crate) fn hub(config: MeshConfig) -> Arc<Hub> {
    Hub::new(config, MeshMetrics::default())
}

/// Every watch listed once, nothing in it, the `ReferenceGrant` CRD present.
pub(crate) fn synced(hub: &Hub) {
    hub.feed_services(Event::Init);
    hub.feed_services(Event::InitDone);
    hub.feed_slices(Event::Init);
    hub.feed_slices(Event::InitDone);
    hub.feed_configs(Event::Init);
    hub.feed_configs(Event::InitDone);
    hub.feed_deployments(Event::Init);
    hub.feed_deployments(Event::InitDone);
    hub.feed_grants(Event::Init);
    hub.feed_grants(Event::InitDone);
}

pub(crate) fn put_service(hub: &Hub, s: Service) {
    hub.feed_services(Event::Apply(key(&s.namespace, &s.name), s));
}

pub(crate) fn put_slice(hub: &Hub, s: EndpointSlice) {
    hub.feed_slices(Event::Apply(key(&s.namespace, &s.name), s));
}

pub(crate) fn put_config(hub: &Hub, c: Config) {
    hub.feed_configs(Event::Apply(key(&c.namespace, &c.name), c));
}

pub(crate) fn put_deployment(hub: &Hub, ns: &str, name: &str) {
    hub.feed_deployments(Event::Apply(key(ns, name), ()));
}

pub(crate) fn put_grant(hub: &Hub, name: &str, g: ReferenceGrant) {
    hub.feed_grants(Event::Apply(key(&g.namespace, name), g));
}

pub(crate) async fn body_of(hub: &Hub, ns: &str, deployment: &str, mode: Mode) -> (String, String) {
    match hub.answer(ns, deployment, &move || mode, &[], &|| {}).await {
        Answer::View(entry) => (entry.etag.clone(), entry.json.to_string()),
        _ => panic!("no view"),
    }
}

#[tokio::test]
async fn msh_20_two_hubs_fed_the_same_objects_in_any_order_serve_the_same_bytes() {
    let objects = |reverse: bool| {
        let hub = hub(MeshConfig::default());
        synced(&hub);
        let mut services = vec![
            svc("shop", "cart", "10.96.0.1"),
            svc("shop", "pay", "10.96.0.2"),
        ];
        let mut slices = vec![
            slice("shop", "cart", "cart-a", &["10.244.0.9", "10.244.0.3"]),
            slice("shop", "pay", "pay-a", &["10.244.1.1"]),
        ];
        if reverse {
            services.reverse();
            slices.reverse();
        }
        services.into_iter().for_each(|s| put_service(&hub, s));
        slices.into_iter().for_each(|s| put_slice(&hub, s));
        put_config(&hub, config("shop", "c", "cart", &[]));
        hub.publish_now();
        hub
    };
    let (a, b) = (objects(false), objects(true));
    let (etag_a, json_a) = body_of(&a, "shop", "cart", Mode::None).await;
    let (etag_b, json_b) = body_of(&b, "shop", "cart", Mode::None).await;
    assert_eq!((&etag_a, &json_a), (&etag_b, &json_b));
    let view = View::from_json(&json_a, &Limits::default()).expect("a view");
    assert_eq!(view.services.len(), 2);
    assert!(view.services["shop/cart"].endpoints[&80].len() == 2);
}

#[tokio::test]
async fn msh_20_the_mode_is_in_the_view_and_a_change_of_mode_changes_the_etag() {
    let hub = hub(MeshConfig::default());
    synced(&hub);
    put_service(&hub, svc("shop", "cart", "10.96.0.1"));
    hub.publish_now();
    let (none_etag, none) = body_of(&hub, "shop", "cart", Mode::None).await;
    let (mock_etag, mock) = body_of(&hub, "shop", "cart", Mode::Mock).await;
    assert!(none.contains(r#""mode":"none""#) && mock.contains(r#""mode":"mock""#));
    assert_ne!(none_etag, mock_etag);
}

/// The instants at which the hub publishes, with the clock of the test.
fn publications(hub: &Arc<Hub>) -> Arc<Mutex<Vec<Duration>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (mut rx, start) = (hub.subscribe(), Instant::now());
    tokio::spawn({
        let seen = seen.clone();
        async move {
            while rx.changed().await.is_ok() {
                seen.lock().unwrap().push(start.elapsed());
            }
        }
    });
    seen
}

#[tokio::test(start_paused = true)]
async fn msh_22_a_change_on_a_quiet_hub_is_published_after_the_debounce_and_a_burst_is_one() {
    let hub = hub(MeshConfig::default());
    synced(&hub);
    tokio::spawn(hub.clone().run());
    let seen = publications(&hub);
    tokio::task::yield_now().await;
    for i in 0..1000 {
        put_service(&hub, svc("shop", &format!("s{i}"), "10.96.0.1"));
    }
    tokio::time::sleep(Duration::from_millis(99)).await;
    assert!(
        seen.lock().unwrap().is_empty(),
        "published before the debounce"
    );
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(*seen.lock().unwrap(), [Duration::from_millis(100)]);
    // Quiet: nothing more.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(seen.lock().unwrap().len(), 1);
    // After a quiet period the next change waits the debounce again: now is 5104 ms.
    put_service(&hub, svc("shop", "late", "10.96.0.2"));
    tokio::time::sleep(Duration::from_millis(105)).await;
    assert_eq!(
        *seen.lock().unwrap(),
        [Duration::from_millis(100), Duration::from_millis(5204)]
    );
}

#[tokio::test(start_paused = true)]
async fn msh_22_while_changes_keep_arriving_the_waits_double_up_to_one_second() {
    let hub = hub(MeshConfig::default());
    synced(&hub);
    tokio::spawn(hub.clone().run());
    let seen = publications(&hub);
    tokio::task::yield_now().await;
    let start = Instant::now();
    // A change every 10 ms for 6 s.
    while start.elapsed() < Duration::from_secs(6) {
        put_service(
            &hub,
            svc(
                "shop",
                "s",
                &format!("10.96.0.{}", start.elapsed().as_millis() % 250),
            ),
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let at = seen.lock().unwrap().clone();
    let gaps: Vec<u128> = at.windows(2).map(|w| (w[1] - w[0]).as_millis()).collect();
    assert_eq!(at[0], Duration::from_millis(100));
    // 100, then about 200, 400, 800, then 1000 and no longer, and no faster.
    assert!(gaps[0] >= 200 && gaps[0] <= 220, "{gaps:?}");
    assert!(gaps[1] >= 400 && gaps[1] <= 440, "{gaps:?}");
    assert!(gaps[2] >= 800 && gaps[2] <= 880, "{gaps:?}");
    assert!(
        gaps[3..].iter().all(|g| (1000..=1020).contains(g)),
        "{gaps:?}"
    );
    assert!(at.len() <= 9, "{at:?}");
    // The changes stop: the next one is back at the base debounce.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let n = seen.lock().unwrap().len();
    let before = Instant::now();
    put_service(&hub, svc("shop", "after", "10.96.9.9"));
    tokio::time::sleep(Duration::from_millis(101)).await;
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), n + 1);
    assert!(before.elapsed() < Duration::from_millis(110));
}

#[derive(Default)]
struct Recorder {
    applied: Mutex<Vec<(String, String, serde_json::Value)>>,
    fail: Mutex<bool>,
}

impl StatusSink for Recorder {
    fn apply<'a>(
        &'a self,
        ns: &'a str,
        name: &'a str,
        body: &'a serde_json::Value,
    ) -> ApplyFuture<'a> {
        Box::pin(async move {
            if *self.fail.lock().unwrap() {
                return Err("boom".to_string());
            }
            self.applied
                .lock()
                .unwrap()
                .push((ns.into(), name.into(), body.clone()));
            Ok(())
        })
    }
}

fn status_of(r: &Recorder, name: &str) -> serde_json::Value {
    let applied = r.applied.lock().unwrap();
    let (_, _, body) = applied
        .iter()
        .rev()
        .find(|(_, n, _)| n == name)
        .expect("applied");
    body["status"]["conditions"].clone()
}

fn resolved(conditions: &serde_json::Value) -> (String, String, String) {
    let c = conditions
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "ResolvedRefs")
        .unwrap();
    let text = |k: &str| c[k].as_str().unwrap_or_default().to_string();
    (text("status"), text("reason"), text("message"))
}

#[tokio::test]
async fn msh_25_a_destination_gains_endpoints_with_a_grant_and_loses_them_without() {
    let hub = hub(MeshConfig::default());
    synced(&hub);
    put_deployment(&hub, "shop", "cart");
    put_service(&hub, svc("shop", "cart", "10.96.0.1"));
    put_service(&hub, svc("risk", "scoring", "10.96.5.5"));
    put_slice(&hub, slice("risk", "scoring", "scoring-a", &["10.244.7.7"]));
    put_config(&hub, config("shop", "c", "cart", &[("risk", "scoring")]));
    hub.publish_now();
    let has_endpoint = |json: &str| json.contains("10.244.7.7");
    assert!(!has_endpoint(
        &body_of(&hub, "shop", "cart", Mode::None).await.1
    ));

    let sink = Recorder::default();
    let mut last = Default::default();
    status::pass(&hub.published().snapshot, &mut last, &sink).await;
    let (ok, reason, message) = resolved(&status_of(&sink, "c"));
    assert_eq!((ok.as_str(), reason.as_str()), ("False", "RefNotPermitted"));
    assert!(message.contains("risk/scoring"), "{message}");

    put_grant(&hub, "g", grant("risk", "shop"));
    hub.publish_now();
    assert!(has_endpoint(
        &body_of(&hub, "shop", "cart", Mode::None).await.1
    ));
    status::pass(&hub.published().snapshot, &mut last, &sink).await;
    assert_eq!(resolved(&status_of(&sink, "c")).0, "True");

    hub.feed_grants(Event::Delete(key("risk", "g")));
    hub.publish_now();
    assert!(!has_endpoint(
        &body_of(&hub, "shop", "cart", Mode::None).await.1
    ));

    // The CRD is not installed: no consent either.
    put_grant(&hub, "g", grant("risk", "shop"));
    hub.grants_absent();
    hub.publish_now();
    assert!(!has_endpoint(
        &body_of(&hub, "shop", "cart", Mode::None).await.1
    ));
}

#[tokio::test]
async fn msh_25_statuses_are_applied_by_mimic_mesh_once_and_a_failed_apply_is_retried() {
    assert_eq!(status::FIELD_MANAGER, "mimic-mesh");
    assert_eq!(
        status::apply_params().field_manager.as_deref(),
        Some("mimic-mesh")
    );
    let hub = hub(MeshConfig::default());
    synced(&hub);
    put_deployment(&hub, "shop", "cart");
    put_config(&hub, config("shop", "c", "cart", &[]));
    hub.publish_now();
    let snapshot = hub.published().snapshot.clone();
    let sink = Recorder::default();
    let mut last = Default::default();
    *sink.fail.lock().unwrap() = true;
    assert!(
        !status::pass(&snapshot, &mut last, &sink).await,
        "a failure is reported"
    );
    assert!(sink.applied.lock().unwrap().is_empty());
    *sink.fail.lock().unwrap() = false;
    assert!(status::pass(&snapshot, &mut last, &sink).await);
    assert!(status::pass(&snapshot, &mut last, &sink).await);
    let applied = sink.applied.lock().unwrap();
    assert_eq!(applied.len(), 1, "an unchanged status is not applied again");
    let (ns, name, body) = &applied[0];
    assert_eq!((ns.as_str(), name.as_str()), ("shop", "c"));
    assert_eq!(body["kind"], "MimicConfig");
    assert_eq!(body["metadata"]["name"], "c");
    let types: Vec<_> = body["status"]["conditions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["Accepted", "ResolvedRefs"]);
    assert_eq!(body["status"]["conditions"][0]["observedGeneration"], 1);
}

fn config_with(cache_bytes: u64) -> MeshConfig {
    MeshConfig {
        cache_bytes,
        ..MeshConfig::default()
    }
}

/// `n` namespaces, each with one Service of `endpoints` endpoints.
fn namespaces(hub: &Hub, n: usize, endpoints: usize) {
    synced(hub);
    let ips: Vec<String> = (0..endpoints).map(|i| format!("10.244.0.{i}")).collect();
    let ips: Vec<&str> = ips.iter().map(String::as_str).collect();
    for i in 0..n {
        let ns = format!("ns{i}");
        put_service(hub, svc(&ns, "web", "10.96.0.1"));
        put_slice(hub, slice(&ns, "web", "web-a", &ips));
    }
    hub.publish_now();
}

#[tokio::test]
async fn msh_26_deployments_with_the_same_inputs_share_one_view_and_one_build() {
    let hub = hub(MeshConfig::default());
    namespaces(&hub, 1, 3);
    for i in 0..50 {
        let (_, _) = body_of(&hub, "ns0", &format!("web{i}"), Mode::Discovery).await;
    }
    let builds = hub
        .metrics()
        .builds
        .get_or_create(&vec![("result".to_string(), "ok".to_string())])
        .get();
    assert_eq!(builds, 1);
    assert!(hub.metrics().cache_bytes.get() > 0);
}

#[tokio::test]
async fn msh_26_the_cache_is_bounded_least_recently_used_first_and_an_evicted_view_is_rebuilt() {
    let probe = hub(MeshConfig::default());
    namespaces(&probe, 1, 20);
    let one = body_of(&probe, "ns0", "a", Mode::None).await.1.len() as u64;
    // Room for two views of that size, not three.
    let hub = hub(config_with(one * 2 + one / 2));
    namespaces(&hub, 3, 20);
    let builds = |h: &Hub| {
        h.metrics()
            .builds
            .get_or_create(&vec![("result".to_string(), "ok".to_string())])
            .get()
    };
    for ns in ["ns0", "ns1"] {
        body_of(&hub, ns, "a", Mode::None).await;
    }
    body_of(&hub, "ns0", "a", Mode::None).await; // ns0 is now the most recent
    body_of(&hub, "ns2", "a", Mode::None).await; // evicts ns1
    assert_eq!(builds(&hub), 3);
    assert!(hub.metrics().cache_bytes.get() as u64 <= one * 2 + one / 2);
    body_of(&hub, "ns0", "a", Mode::None).await;
    assert_eq!(builds(&hub), 3, "ns0 was kept");
    body_of(&hub, "ns1", "a", Mode::None).await;
    assert_eq!(builds(&hub), 4, "ns1 was evicted and is rebuilt");
}

#[tokio::test]
async fn msh_26_a_view_larger_than_the_cache_is_served_and_not_cached() {
    let hub = hub(config_with(10));
    namespaces(&hub, 1, 5);
    let (_, json) = body_of(&hub, "ns0", "a", Mode::None).await;
    assert!(json.len() > 10);
    assert_eq!(hub.metrics().cache_bytes.get(), 0);
    body_of(&hub, "ns0", "a", Mode::None).await;
    let builds = hub
        .metrics()
        .builds
        .get_or_create(&vec![("result".to_string(), "ok".to_string())])
        .get();
    assert_eq!(builds, 2);
}

#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Lines {
    type Writer = Lines;

    fn make_writer(&'a self) -> Lines {
        self.clone()
    }
}

#[tokio::test]
async fn msh_26_a_truncated_view_is_counted_and_warned_once_per_build_not_per_request() {
    let lines = Lines::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .json()
            .with_writer(lines.clone())
            .finish(),
    );
    let hub = hub(MeshConfig {
        limits: Limits {
            max_services: 2,
            ..Limits::default()
        },
        ..MeshConfig::default()
    });
    synced(&hub);
    for name in ["a", "b", "c", "d", "e"] {
        put_service(&hub, svc("shop", name, "10.96.0.1"));
    }
    hub.publish_now();
    for _ in 0..5 {
        let (_, json) = body_of(&hub, "shop", "cart", Mode::None).await;
        assert_eq!(
            View::from_json(&json, &Limits::default())
                .unwrap()
                .services
                .len(),
            2
        );
    }
    assert_eq!(
        hub.metrics().truncated.get(),
        3,
        "the Services left out, once per build"
    );
    let text = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
    let warns: Vec<_> = text
        .lines()
        .filter(|l| l.contains("mesh view truncated"))
        .collect();
    assert_eq!(warns.len(), 1, "one WARN per build");
    assert!(warns[0].contains(r#""left_out":3"#) && warns[0].contains(r#""namespace":"shop""#));
}

#[tokio::test]
async fn msh_27_a_relist_replaces_what_the_watch_held_and_a_delete_removes_its_key() {
    let hub = hub(MeshConfig::default());
    synced(&hub);
    put_service(&hub, svc("shop", "old", "10.96.0.1"));
    put_service(&hub, svc("shop", "gone", "10.96.0.2"));
    hub.publish_now();
    let names = |json: &str| {
        View::from_json(json, &Limits::default())
            .unwrap()
            .services
            .into_keys()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(&body_of(&hub, "shop", "x", Mode::None).await.1),
        ["shop/gone", "shop/old"]
    );
    // The watch restarts: only `old` is listed again; `gone` was deleted while it was down.
    hub.feed_services(Event::Init);
    hub.feed_services(Event::InitApply(
        key("shop", "old"),
        svc("shop", "old", "10.96.0.1"),
    ));
    hub.publish_now();
    assert_eq!(
        names(&body_of(&hub, "shop", "x", Mode::None).await.1),
        ["shop/gone", "shop/old"],
        "the old content is served until the relist is complete"
    );
    hub.feed_services(Event::InitDone);
    hub.publish_now();
    assert_eq!(
        names(&body_of(&hub, "shop", "x", Mode::None).await.1),
        ["shop/old"]
    );
    hub.feed_services(Event::Delete(key("shop", "old")));
    hub.publish_now();
    assert!(names(&body_of(&hub, "shop", "x", Mode::None).await.1).is_empty());
}

#[tokio::test]
async fn msh_26_the_gzip_copy_of_a_cached_view_is_counted_in_the_cache() {
    let hub = hub(MeshConfig::default());
    namespaces(&hub, 1, 20);
    let Answer::View(entry) = hub.answer("ns0", "a", &|| Mode::None, &[], &|| {}).await else {
        panic!("no view");
    };
    let plain = hub.metrics().cache_bytes.get() as usize;
    assert_eq!(plain, entry.json.len());
    hub.compress(&entry);
    hub.compress(&entry);
    assert_eq!(
        hub.metrics().cache_bytes.get() as usize,
        entry.json.len() + entry.gzip().len(),
        "counted once"
    );
    // Over the budget with its copy: the view is served and dropped from the cache.
    let small = super::hub::Hub::new(
        config_with(entry.json.len() as u64 + 1),
        MeshMetrics::default(),
    );
    namespaces(&small, 1, 20);
    let Answer::View(entry) = small.answer("ns0", "a", &|| Mode::None, &[], &|| {}).await else {
        panic!("no view");
    };
    small.compress(&entry);
    assert_eq!(small.metrics().cache_bytes.get(), 0);
}
