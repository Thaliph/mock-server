//! EXP-23 and EXP-24: the Map tab's route and script (spec §7.5, §12.17). The route tests need
//! a Valkey >= 9.0 (`MIMIC_TEST_VALKEY_URL`), except the one with Valkey stopped.

use mimic_core::discovery::report::Via;
use mimic_core::discovery::scope::ScopeKey;

use super::*;
use crate::mesh::tests as mesh;

/// The one stored authority that holds markup characters: a redacted host (spec §6.1).
const REDACTED: &str = "<redacted>:443";

/// A namespace that sorts before the others of the shared Valkey: the map keeps the first 1000
/// deployments by name.
fn first(prefix: &str) -> String {
    format!("0{}", unique(prefix))
}

/// Exchanges that win a place among the other nodes of a shared Valkey (the map keeps the
/// heaviest 200).
const HEAVY: usize = 1000;

/// One v2 push of `dep`: an inbound `/a` and, per `(authority, status, times)`, that many
/// outbound exchanges.
async fn seed_calls(store: &Arc<Store>, ns: &str, dep: &str, calls: &[(&str, u16, usize)]) {
    let mut push = push_of("front-1", 1, &[("/a", 200)]);
    let mut report = Report::from_json(&push.json).expect("reads");
    for (authority, status, times) in calls {
        for _ in 0..*times {
            report.record_at(
                ScopeKey::Authority((*authority).to_string()),
                &Via {
                    tls: false,
                    sni: None,
                    dst: None,
                },
                &LIMITS,
                &Redactor::default(),
                &Exchange {
                    direction: Direction::Outbound,
                    port: 80,
                    method: "GET",
                    path: "/x",
                    status: *status,
                    head_ms: 3,
                    host: None,
                    grpc: false,
                    observation: Observation::default(),
                },
            );
        }
    }
    let today = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / DAY);
    report.stamp(today);
    push.json = report.to_json().expect("serialises");
    store.push(ns, dep, push).await.expect("push applied");
}

/// A hub that has listed everything, with the given Services and configs.
fn hub_with(
    services: Vec<mimic_core::mesh::input::Service>,
    configs: Vec<mimic_core::mesh::input::Config>,
) -> Arc<crate::mesh::Hub> {
    let hub = mesh::hub(crate::mesh::MeshConfig::default());
    mesh::synced(&hub);
    services
        .into_iter()
        .for_each(|s| mesh::put_service(&hub, s));
    configs.into_iter().for_each(|c| mesh::put_config(&hub, c));
    hub.publish_now();
    hub
}

fn app_with(store: &Arc<Store>, hub: Option<Arc<crate::mesh::Hub>>) -> (Router, Metrics) {
    let mut metrics = Metrics::new("controller", "test");
    let ui = UiMetrics::register(metrics.registry_mut());
    (
        router_mesh(store.clone(), ui, ResourceCache::new(), hub),
        metrics,
    )
}

fn json_of(answer: &Full) -> Value {
    serde_json::from_slice(&answer.body).expect("a JSON body")
}

fn node<'a>(body: &'a Value, id: &str) -> Option<&'a Value> {
    body["nodes"].as_array()?.iter().find(|n| n["id"] == id)
}

#[tokio::test]
async fn exp_23_the_map_route_answers_the_graph() {
    let store = store(&valkey_url());
    let ns = first("exp23");
    seed_calls(
        &store,
        &ns,
        "front",
        &[
            (&format!("users.{ns}.svc:8080"), 200, 3 * HEAVY),
            (&format!("users.{ns}.svc:8080"), 500, HEAVY),
            ("api.example.com:443", 200, HEAVY),
        ],
    )
    .await;
    seed(&store, &ns, "users", &["users-1"]).await;
    let hub = hub_with(vec![mesh::svc(&ns, "users", "10.96.0.7")], vec![]);
    let (app, _) = app_with(&store, Some(hub));
    let answer = fetch(&app, "/v1/map").await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.headers["content-type"], "application/json");
    assert_eq!(answer.headers["x-content-type-options"], "nosniff");
    let body = json_of(&answer);
    let (front, users) = (format!("dep:{ns}/front"), format!("dep:{ns}/users"));
    assert!(node(&body, &front).is_some(), "no node for front");
    assert!(node(&body, &users).is_some());
    assert_eq!(
        node(&body, &front).unwrap()["href"],
        format!("/ui/workloads/{ns}/front")
    );
    assert!(node(&body, "ext:api.example.com:443").is_some());
    let edge = body["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["from"] == front.as_str() && e["to"] == users.as_str())
        .expect("the edge");
    assert_eq!(edge["exchanges"], 4 * HEAVY);
    assert_eq!(edge["errors"], HEAVY);
    assert_eq!(edge["error_permille"], 250);
    assert_eq!(edge["declared"], true);
    assert_eq!(
        edge["href"],
        format!("/ui/endpoints?deployment={ns}%2Ffront")
    );
    assert!(body["dropped"].is_object() && body["width"].is_number());

    let svg = fetch(&app, "/v1/map?format=svg").await;
    assert_eq!(svg.status, 200);
    assert_eq!(svg.headers["content-type"], "image/svg+xml");
    let text = String::from_utf8(svg.body).unwrap();
    assert!(
        text.starts_with("<svg ") && text.contains("api.example.com:443"),
        "the SVG does not start as an svg element"
    );
    assert!(text.contains(&format!("{ns}/front")));
}

#[tokio::test]
async fn exp_23_a_redacted_host_is_escaped_in_the_svg_and_unchanged_in_the_json() {
    let store = store(&valkey_url());
    let ns = first("exp23h");
    seed_calls(&store, &ns, "front", &[(REDACTED, 200, HEAVY)]).await;
    let (app, _) = app_with(&store, None);
    let svg = String::from_utf8(fetch(&app, "/v1/map?format=svg").await.body).unwrap();
    assert!(!svg.contains("<redacted"), "an unescaped name");
    assert!(svg.contains("&lt;redacted&gt;:443"));
    let body = json_of(&fetch(&app, "/v1/map").await);
    assert!(node(&body, &format!("ext:{REDACTED}")).is_some());
}

#[tokio::test]
async fn exp_23_not_declared_needs_a_ready_hub() {
    let store = store(&valkey_url());
    let ns = first("exp23n");
    let other = unique("exp23o");
    let authority = format!("scoring.{other}.svc:80");
    seed_calls(&store, &ns, "front", &[(&authority, 200, HEAVY)]).await;
    let to = format!("svc:{other}/scoring");
    let declared = |body: &Value| {
        body["edges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["from"] == format!("dep:{ns}/front").as_str() && e["to"] == to.as_str())
            .map(|e| e["declared"].clone())
    };
    let hub = hub_with(vec![mesh::svc(&other, "scoring", "10.96.0.9")], vec![]);
    let (app, _) = app_with(&store, Some(hub));
    assert_eq!(
        declared(&json_of(&fetch(&app, "/v1/map").await)),
        Some(json!(false))
    );
    let (app, _) = app_with(&store, None);
    assert_eq!(
        declared(&json_of(&fetch(&app, "/v1/map").await)),
        Some(json!(true))
    );
    // A hub whose lists are not complete flags nothing either.
    let unlisted = mesh::hub(crate::mesh::MeshConfig::default());
    unlisted.publish_now();
    let (app, _) = app_with(&store, Some(unlisted));
    assert_eq!(
        declared(&json_of(&fetch(&app, "/v1/map").await)),
        Some(json!(true))
    );
}

#[tokio::test]
async fn exp_23_a_deployment_without_outbound_data_is_a_node_without_edge() {
    let store = store(&valkey_url());
    let ns = first("exp23i");
    seed(&store, &ns, "quiet", &["quiet-1"]).await;
    let (app, _) = app_with(&store, None);
    let body = json_of(&fetch(&app, "/v1/map").await);
    assert!(node(&body, &format!("dep:{ns}/quiet")).is_some());
    assert!(
        body["edges"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["from"] != format!("dep:{ns}/quiet").as_str())
    );
}

#[tokio::test]
async fn exp_23_a_query_other_than_format_svg_is_400() {
    let (app, _) = app_with(&store("redis://127.0.0.1:1"), None);
    for path in [
        "/v1/map?format=png",
        "/v1/map?format=json",
        "/v1/map?format=",
        "/v1/map?x=1",
        "/v1/map?format=svg&format=json",
    ] {
        assert_eq!(fetch(&app, path).await.status, 400, "{path}");
    }
}

#[tokio::test]
async fn exp_23_only_the_first_deployments_by_name_are_read() {
    let store = store(&valkey_url());
    let ns = first("exp23c");
    for dep in ["d", "a", "c", "b"] {
        seed_calls(&store, &ns, dep, &[("x.example.com:443", 200, 1)]).await;
    }
    let mut metrics = Metrics::new("controller", "test");
    let ui = Ui {
        store: store.clone(),
        metrics: UiMetrics::register(metrics.registry_mut()),
        renders: Arc::new(Semaphore::new(MAX_RENDERS)),
        hook: None,
        resources: ResourceCache::new(),
        mesh: None,
    };
    let (summaries, _) = store.list().await.expect("lists");
    let ours: Vec<_> = summaries
        .into_iter()
        .filter(|s| s.namespace == ns)
        .collect();
    assert_eq!(ours.len(), 4);
    let read = read_sources(&ui, ours, 2).await.expect("reads");
    let names: Vec<&str> = read.iter().map(|s| s.deployment.as_str()).collect();
    assert_eq!(names, ["a", "b"]);
    assert_eq!(read[0].calls.len(), 1);
}

#[tokio::test]
async fn exp_23_the_map_is_503_with_valkey_stopped() {
    let (app, _) = app_with(&store("redis://127.0.0.1:1"), None);
    for path in ["/v1/map", "/v1/map?format=svg"] {
        assert_eq!(fetch(&app, path).await.status, 503, "{path}");
    }
}

#[tokio::test]
async fn exp_23_head_405_403_and_the_headers_hold_and_the_route_counts_by_pattern() {
    let store = store("redis://127.0.0.1:1");
    let (app, metrics) = app_with(&store, None);
    for method in [Method::POST, Method::DELETE] {
        let answer = send(&app, method, "/v1/map", "127.0.0.1", Some("localhost:8080")).await;
        assert_eq!(answer.status, 405);
        assert_eq!(answer.headers["allow"], "GET, HEAD");
    }
    let head = send(
        &app,
        Method::HEAD,
        "/ui/map",
        "127.0.0.1",
        Some("localhost"),
    )
    .await;
    assert_eq!((head.status, head.body.len()), (200, 0));
    for (peer, host) in [
        ("10.1.2.3", Some("localhost:8080")),
        ("127.0.0.1", Some("evil.example")),
        ("127.0.0.1", None),
    ] {
        let answer = send(&app, Method::GET, "/v1/map", peer, host).await;
        assert_eq!(answer.status, 403, "{peer} {host:?}");
        assert!(answer.headers.contains_key("content-security-policy"));
    }
    let answer = fetch(&app, "/v1/map").await;
    assert_eq!(answer.status, 503);
    assert!(answer.headers.contains_key("content-security-policy"));
    assert_eq!(answer.headers["cache-control"], "no-store");
    assert_eq!(fetch(&app, "/ui/map").await.status, 200);
    assert_eq!(requests(&metrics, "/v1/map", 503), Some(1));
    assert_eq!(requests(&metrics, "/v1/map", 405), Some(2));
    assert_eq!(requests(&metrics, "/ui/map", 200), Some(2));
    let text = metrics.encode();
    assert!(
        !text
            .lines()
            .any(|l| l.starts_with("mimic_ui_") && l.contains("format"))
    );
}

fn mimic_js() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/mimic.js")).unwrap()
}

#[test]
fn exp_24_the_map_tab_reads_the_svg_and_copies_an_allowlist() {
    let js = mimic_js();
    assert!(
        !js.contains("not available yet"),
        "the placeholder is still there"
    );
    for needle in [
        "/v1/map?format=svg",
        "DOMParser",
        "image/svg+xml",
        "No calls recorded",
        "createElementNS",
        "\"/ui/\"",
    ] {
        assert!(js.contains(needle), "mimic.js lacks {needle}");
    }
    for rule in [
        "\\\\",
        "\"..\"",
        "decodeURIComponent",
        "indexOf(\"//\")",
        "namespaceURI",
        "SVG_NAMESPACE",
    ] {
        assert!(js.contains(rule), "mimic.js lacks the rule {rule}");
    }
    // The one URL allowed is the SVG namespace constant.
    let rest = js.replace("http://www.w3.org/2000/svg", "");
    for banned in [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
        "eval(",
        "Function(",
        "http://",
        "https://",
    ] {
        assert!(!rest.contains(banned), "mimic.js holds {banned}");
    }
}
