//! CTL-13: the read API (spec §7.5). The integration tests need a Valkey >= 9.0 whose URL is
//! in `MIMIC_TEST_VALKEY_URL`.

use std::net::SocketAddr;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request};
use http_body_util::BodyExt;
use mimic_core::discovery::observed::Observation;
use mimic_core::discovery::redact::Redactor;
use mimic_core::discovery::report::{Direction, Exchange, Limits, Report};
use mimic_runtime::metrics::Metrics;
use serde_json::Value;
use tower::ServiceExt;

use super::*;
use crate::store::{Push, SeqId, StoreConfig, StoreError, StoreMetrics, keys};

const SESSION: &str = "0123456789abcdef0123456789abcdef";
const DAY: u64 = 86_400;
const LIMITS: Limits = Limits {
    max_literals: 32,
    max_endpoints: 256,
    max_authorities: 64,
    sample_budget: 1 << 20,
};

fn valkey_url() -> String {
    std::env::var("MIMIC_TEST_VALKEY_URL").unwrap_or_else(|_| {
        panic!(
            "MIMIC_TEST_VALKEY_URL is not set: start a Valkey >= 9.0 (image of \
             `scripts/pinned-image.sh valkey`) and set it, e.g. redis://127.0.0.1:6379"
        )
    })
}

fn unique(prefix: &str) -> String {
    let mut bytes = [0u8; 6];
    rand::fill(&mut bytes[..]);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}-{hex}")
}

fn store(url: &str) -> Arc<Store> {
    let mut metrics = Metrics::new("controller", "test");
    Store::new(
        redis::Client::open(url).expect("a valid URL"),
        StoreConfig {
            ttl: Duration::from_secs(30 * DAY),
            limits: LIMITS,
            max_deployments: 50,
        },
        StoreMetrics::register(metrics.registry_mut()),
    )
}

fn push_of(pod: &str, seq: u64, paths: &[(&str, u16)]) -> Push {
    let mut report = Report::new(Some(pod.to_string()));
    for (path, status) in paths {
        report.record(
            &LIMITS,
            &Redactor::default(),
            &Exchange {
                direction: Direction::Inbound,
                port: 80,
                method: "GET",
                path,
                status: *status,
                head_ms: 3,
                host: Some("svc"),
                grpc: false,
                observation: Observation::default(),
            },
        );
    }
    let today = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / DAY);
    report.stamp(today);
    Push {
        pod: pod.to_string(),
        seq: Some(SeqId::parse(SESSION, seq).expect("valid seq")),
        json: report.to_json().expect("serialises"),
    }
}

struct Answer {
    status: u16,
    body: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("a JSON body")
    }
}

async fn call(store: &Arc<Store>, method: Method, path: &str, peer: &str) -> Answer {
    call_host(store, method, path, peer, Some("localhost:8080")).await
}

async fn call_host(
    store: &Arc<Store>,
    method: Method,
    path: &str,
    peer: &str,
    host: Option<&str>,
) -> Answer {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(host) = host {
        builder = builder.header("host", host);
    }
    let mut request = builder.body(Body::empty()).unwrap();
    let peer: SocketAddr = format!("{peer}:40000").parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(peer));
    let response = router(store.clone(), UiMetrics::default())
        .oneshot(request)
        .await
        .unwrap();
    Answer {
        status: response.status().as_u16(),
        body: response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    }
}

async fn get(store: &Arc<Store>, path: &str) -> Answer {
    call(store, Method::GET, path, "127.0.0.1").await
}

/// One push per pod; the pod `i` sees `1 + i % 2` endpoints.
async fn seed(store: &Arc<Store>, ns: &str, dep: &str, pods: &[&str]) {
    for (i, pod) in pods.iter().enumerate() {
        let push = push_of(pod, 1, &[("/a", 200), ("/b", 200)][..1 + i % 2]);
        store.push(ns, dep, push).await.expect("push applied");
    }
}

async fn connection() -> redis::aio::MultiplexedConnection {
    redis::Client::open(valkey_url().as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap()
}

#[tokio::test]
async fn ctl_13_list_is_sorted_with_the_summary_of_each_deployment() {
    let store = store(&valkey_url());
    let (a, b) = (unique("ctl13a"), unique("ctl13b"));
    // Pushed out of order, and with names whose byte order differs from the tuple order.
    seed(&store, &b, "alpha", &["p1"]).await;
    seed(&store, &a, "beta-x", &["p1", "p2"]).await;
    seed(&store, &a, "beta", &["p1"]).await;
    let answer = get(&store, "/v1/workloads").await;
    assert_eq!(answer.status, 200);
    let body = answer.json();
    let all = body["workloads"].as_array().expect("a list");
    let mine: Vec<(&str, &str)> = all
        .iter()
        .map(|w| {
            (
                w["namespace"].as_str().unwrap(),
                w["deployment"].as_str().unwrap(),
            )
        })
        .filter(|(ns, _)| *ns == a || *ns == b)
        .collect();
    let (a, b) = (a.as_str(), b.as_str());
    let mut want = vec![(a, "beta"), (a, "beta-x"), (b, "alpha")];
    want.sort();
    assert_eq!(mine, want, "{body}");
    let entry = all
        .iter()
        .find(|w| w["namespace"] == a && w["deployment"] == "beta-x")
        .unwrap();
    let keys: Vec<&str> = entry
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "deployment",
            "endpoints",
            "exchanges",
            "last_push",
            "namespace",
            "sample_bytes"
        ]
    );
    assert_eq!(entry["endpoints"], 2);
    assert_eq!(entry["exchanges"], 3);
    let last = entry["last_push"].as_str().unwrap();
    assert!(last.starts_with("20") && last.ends_with('Z'), "{last}");
}

#[tokio::test]
async fn ctl_13_workload_adds_the_pods_map() {
    let store = store(&valkey_url());
    let ns = unique("ctl13p");
    seed(&store, &ns, "front", &["front-1", "front-2"]).await;
    let answer = get(&store, &format!("/v1/workloads/{ns}/front")).await;
    assert_eq!(answer.status, 200);
    let body = answer.json();
    assert_eq!(body["namespace"], ns.as_str());
    assert_eq!(body["deployment"], "front");
    assert_eq!(body["endpoints"], 2);
    let pods = body["pods"].as_object().expect("a pods map");
    assert_eq!(pods.keys().collect::<Vec<_>>(), ["front-1", "front-2"]);
    for stamp in pods.values() {
        let stamp = stamp.as_str().unwrap();
        assert!(stamp.starts_with("20") && stamp.ends_with('Z'), "{stamp}");
    }
    assert_eq!(pods["front-2"], body["last_push"]);
}

#[tokio::test]
async fn ctl_13_report_bytes_equal_the_stored_report() {
    let store = store(&valkey_url());
    let ns = unique("ctl13r");
    seed(&store, &ns, "front", &["front-1"]).await;
    let k = keys(&ns, "front").unwrap();
    let stored: Vec<u8> = redis::cmd("GET")
        .arg(&k.report)
        .query_async(&mut connection().await)
        .await
        .unwrap();
    let answer = get(&store, &format!("/v1/workloads/{ns}/front/report")).await;
    assert_eq!(answer.status, 200);
    assert!(!stored.is_empty());
    assert_eq!(answer.body, stored);
}

#[tokio::test]
async fn ctl_13_not_found_and_invalid_names() {
    let store = store(&valkey_url());
    let ns = unique("ctl13n");
    for path in [
        format!("/v1/workloads/{ns}/ghost"),
        format!("/v1/workloads/{ns}/ghost/report"),
    ] {
        assert_eq!(get(&store, &path).await.status, 404, "{path}");
    }
    for path in [
        "/v1/workloads/Bad_Name/front",
        "/v1/workloads/shop/Front/report",
        "/v1/workloads/shop/-front",
        "/v1/workloads/shop/front%3Areport",
    ] {
        assert_eq!(get(&store, path).await.status, 400, "{path}");
    }
}

#[tokio::test]
async fn ctl_13_only_get_is_served() {
    let store = store(&valkey_url());
    for method in [Method::POST, Method::DELETE, Method::PUT] {
        for path in [
            "/v1/workloads",
            "/v1/workloads/shop/front",
            "/v1/workloads/shop/front/report",
        ] {
            let answer = call(&store, method.clone(), path, "127.0.0.1").await;
            assert_eq!(answer.status, 405, "{method} {path}");
        }
    }
}

#[tokio::test]
async fn ctl_13_a_peer_that_is_not_loopback_gets_403() {
    let store = store(&valkey_url());
    for peer in ["10.1.2.3", "192.168.1.9"] {
        for path in ["/v1/workloads", "/v1/workloads/shop/front"] {
            let answer = call(&store, Method::GET, path, peer).await;
            assert_eq!(answer.status, 403, "{peer} {path}");
        }
    }
    let answer = call(&store, Method::GET, "/v1/workloads", "127.0.0.1").await;
    assert_eq!(answer.status, 200);
}

#[tokio::test]
async fn ctl_13_a_host_other_than_loopback_gets_403() {
    let store = store(&valkey_url());
    for host in [
        Some("evil.example"),
        Some("localhost.evil.example"),
        Some("127.0.0.1.evil.example:8080"),
        Some("10.0.0.1:8080"),
        Some("127.0.0.1:"),
        Some("localhost:"),
        Some("[::1]:"),
        None,
    ] {
        let answer = call_host(&store, Method::GET, "/v1/workloads", "127.0.0.1", host).await;
        assert_eq!(answer.status, 403, "{host:?}");
    }
    for host in [
        "localhost:8080",
        "localhost",
        "127.0.0.1",
        "127.0.0.1:8080",
        "[::1]:8080",
        "LOCALHOST:8080",
    ] {
        let answer = call_host(
            &store,
            Method::GET,
            "/v1/workloads",
            "127.0.0.1",
            Some(host),
        )
        .await;
        assert_eq!(answer.status, 200, "{host}");
    }
}

#[tokio::test]
async fn ctl_13_503_while_valkey_is_stopped() {
    // Nothing listens on port 1.
    let store = store("redis://127.0.0.1:1");
    for path in [
        "/v1/workloads",
        "/v1/workloads/shop/front",
        "/v1/workloads/shop/front/report",
    ] {
        assert_eq!(get(&store, path).await.status, 503, "{path}");
    }
}

#[tokio::test]
async fn ctl_13_a_corrupted_report_is_a_500_with_one_error_and_is_not_overwritten() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let store = store(&valkey_url());
    let ns = unique("ctl13c");
    seed(&store, &ns, "front", &["front-1"]).await;
    let k = keys(&ns, "front").unwrap();
    let mut conn = connection().await;
    let _: () = redis::cmd("SET")
        .arg(&k.report)
        .arg("{not json")
        .query_async(&mut conn)
        .await
        .unwrap();
    let answer = get(&store, &format!("/v1/workloads/{ns}/front/report")).await;
    assert_eq!(answer.status, 500);
    assert!(!String::from_utf8_lossy(&answer.body).contains("not json"));
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    let errors: Vec<Value> = logs
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["level"] == "ERROR" && v["message"] == "stored report unreadable")
        .filter(|v| v["namespace"] == ns.as_str())
        .collect();
    assert_eq!(errors.len(), 1, "{errors:?}");
    // The summary does not need the report.
    let summary = get(&store, &format!("/v1/workloads/{ns}/front")).await;
    assert_eq!(summary.status, 200);
    // A push is refused and leaves the bytes alone.
    let refused = store
        .push(&ns, "front", push_of("front-1", 2, &[("/c", 200)]))
        .await;
    assert_eq!(refused, Err(StoreError::Unavailable));
    let stored: String = redis::cmd("GET")
        .arg(&k.report)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(stored, "{not json");
}

#[test]
fn ctl_13_a_timestamp_out_of_range_is_still_a_timestamp() {
    assert_eq!(rfc3339(1), "1970-01-01T00:00:00.001Z");
    assert_eq!(rfc3339(u64::MAX), "9999-12-31T23:59:59.999Z");
}

fn errors(capture: &crate::webhook::tests::Capture, ns: &str, message: &str) -> usize {
    String::from_utf8(capture.0.lock().unwrap().clone())
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["level"] == "ERROR" && v["message"] == message && v["namespace"] == ns)
        .count()
}

async fn hset(key: &str, field: &str, value: &str) {
    let _: () = redis::cmd("HSET")
        .arg(key)
        .arg(field)
        .arg(value)
        .query_async(&mut connection().await)
        .await
        .unwrap();
}

#[tokio::test]
async fn ctl_13_a_corrupted_meta_is_a_500_with_one_error() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let store = store(&valkey_url());
    let ns = unique("ctl13m");
    seed(&store, &ns, "front", &["front-1"]).await;
    hset(&keys(&ns, "front").unwrap().meta, "exchanges", "many").await;
    let answer = get(&store, &format!("/v1/workloads/{ns}/front")).await;
    assert_eq!(answer.status, 500);
    assert_eq!(errors(&capture, &ns, "stored meta unreadable"), 1);
    // The report does not need `:meta`.
    let report = get(&store, &format!("/v1/workloads/{ns}/front/report")).await;
    assert_eq!(report.status, 200);
}

#[tokio::test]
async fn ctl_13_corrupted_pods_are_a_500_with_one_error() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let store = store(&valkey_url());
    let ns = unique("ctl13q");
    seed(&store, &ns, "front", &["front-1"]).await;
    hset(&keys(&ns, "front").unwrap().pods, "front-1", "yesterday").await;
    let answer = get(&store, &format!("/v1/workloads/{ns}/front")).await;
    assert_eq!(answer.status, 500);
    assert_eq!(errors(&capture, &ns, "stored pods unreadable"), 1);
    assert_eq!(errors(&capture, &ns, "stored meta unreadable"), 0);
}

#[tokio::test]
async fn ctl_13_the_list_skips_an_unreadable_meta_with_one_error() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let store = store(&valkey_url());
    let ns = unique("ctl13l");
    seed(&store, &ns, "good", &["p1"]).await;
    seed(&store, &ns, "bad", &["p1"]).await;
    hset(&keys(&ns, "bad").unwrap().meta, "updated", "never").await;
    let answer = get(&store, "/v1/workloads").await;
    assert_eq!(answer.status, 200);
    let body = answer.json();
    let mine: Vec<&str> = body["workloads"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| w["namespace"] == ns.as_str())
        .map(|w| w["deployment"].as_str().unwrap())
        .collect();
    assert_eq!(mine, ["good"]);
    assert_eq!(errors(&capture, &ns, "stored meta unreadable"), 1);
}

#[tokio::test]
async fn ctl_13_every_read_logs_its_error_although_the_status_pass_logged_the_streak() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let store = store(&valkey_url());
    let ns = unique("ctl13s");
    seed(&store, &ns, "front", &["front-1"]).await;
    hset(&keys(&ns, "front").unwrap().meta, "exchanges", "many").await;
    // The status pass logs the streak once, however often it reads.
    for _ in 0..2 {
        assert!(store.status_input(&ns, "front").await.is_err());
    }
    assert_eq!(errors(&capture, &ns, "stored meta unreadable"), 1);
    // The read API logs one line per read (spec 7.5).
    for want in [2, 3] {
        let answer = get(&store, &format!("/v1/workloads/{ns}/front")).await;
        assert_eq!(answer.status, 500);
        assert_eq!(errors(&capture, &ns, "stored meta unreadable"), want);
    }
}

// ---- F6: UI-07 and UI-10 ----

struct Full {
    status: u16,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

async fn send(app: &Router, method: Method, path: &str, peer: &str, host: Option<&str>) -> Full {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(host) = host {
        builder = builder.header("host", host);
    }
    let mut request = builder.body(Body::empty()).unwrap();
    let peer: SocketAddr = format!("{peer}:40000").parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(peer));
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    Full {
        status: parts.status.as_u16(),
        headers: parts.headers,
        body: body.collect().await.unwrap().to_bytes().to_vec(),
    }
}

async fn fetch(app: &Router, path: &str) -> Full {
    send(app, Method::GET, path, "127.0.0.1", Some("localhost:8080")).await
}

fn app(store: &Arc<Store>) -> (Router, Metrics) {
    let mut metrics = Metrics::new("controller", "test");
    let ui = UiMetrics::register(metrics.registry_mut());
    (router(store.clone(), ui), metrics)
}

/// The report and `:meta` `version` as stored, read straight from Valkey.
async fn stored(ns: &str, dep: &str) -> (Report, u64) {
    let k = keys(ns, dep).unwrap();
    let mut conn = connection().await;
    let bytes: Vec<u8> = redis::cmd("GET")
        .arg(&k.report)
        .query_async(&mut conn)
        .await
        .unwrap();
    let version: u64 = redis::cmd("HGET")
        .arg(&k.meta)
        .arg("version")
        .query_async(&mut conn)
        .await
        .unwrap();
    let report = Report::from_json(std::str::from_utf8(&bytes).unwrap()).unwrap();
    (report, version)
}

fn bytes_of(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}

#[tokio::test]
async fn ui_07_new_routes_answer_the_core_output_for_the_stored_report() {
    use mimic_core::discovery::openapi;
    let store = store(&valkey_url());
    let (app, _) = app(&store);
    let ns = unique("ui07a");
    seed(&store, &ns, "front", &["front-1", "front-2"]).await;
    let (report, version) = stored(&ns, "front").await;
    let base = format!("/v1/workloads/{ns}/front");
    let endpoints = fetch(&app, &format!("{base}/endpoints")).await;
    assert_eq!(endpoints.status, 200);
    assert_eq!(endpoints.headers["content-type"], "application/json");
    assert_eq!(endpoints.body, bytes_of(&openapi::endpoint_list(&report)));
    let index = fetch(&app, &format!("{base}/openapi")).await;
    assert_eq!(index.status, 200);
    assert_eq!(index.headers["content-type"], "application/json");
    assert_eq!(index.body, bytes_of(&openapi::index(&report)));
    let document = fetch(&app, &format!("{base}/openapi/inbound-80")).await;
    assert_eq!(document.status, 200);
    assert_eq!(document.headers["content-type"], "application/json");
    let want = openapi::document(&report, &ns, "front", version, "inbound-80").unwrap();
    assert_eq!(document.body, bytes_of(&want));
}

#[tokio::test]
async fn ui_07_bad_ids_are_400_and_absent_ones_404() {
    let store = store(&valkey_url());
    let (app, _) = app(&store);
    let ns = unique("ui07b");
    seed(&store, &ns, "front", &["front-1"]).await;
    for id in ["inbound-0", "inbound-65536", "inbound-08080", "foo"] {
        let answer = fetch(&app, &format!("/v1/workloads/{ns}/front/openapi/{id}")).await;
        assert_eq!(answer.status, 400, "{id}");
    }
    let absent = fetch(
        &app,
        &format!("/v1/workloads/{ns}/front/openapi/inbound-81"),
    )
    .await;
    assert_eq!(absent.status, 404);
    for suffix in ["endpoints", "openapi", "openapi/inbound-80"] {
        let ghost = fetch(&app, &format!("/v1/workloads/{ns}/ghost/{suffix}")).await;
        assert_eq!(ghost.status, 404, "{suffix}");
        let bad = fetch(&app, &format!("/v1/workloads/Bad_Name/front/{suffix}")).await;
        assert_eq!(bad.status, 400, "{suffix}");
    }
}

#[tokio::test]
async fn ui_07_503_while_valkey_is_stopped() {
    let store = store("redis://127.0.0.1:1");
    let (app, _) = app(&store);
    for suffix in ["endpoints", "openapi", "openapi/inbound-80"] {
        let answer = fetch(&app, &format!("/v1/workloads/shop/front/{suffix}")).await;
        assert_eq!(answer.status, 503, "{suffix}");
    }
}

#[tokio::test]
async fn ui_07_a_corrupted_report_is_a_500_with_one_error_per_read() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let store = store(&valkey_url());
    let (app, _) = app(&store);
    let ns = unique("ui07c");
    seed(&store, &ns, "front", &["front-1"]).await;
    let _: () = redis::cmd("SET")
        .arg(&keys(&ns, "front").unwrap().report)
        .arg("{not json")
        .query_async(&mut connection().await)
        .await
        .unwrap();
    for (i, suffix) in ["endpoints", "openapi", "openapi/inbound-80"]
        .iter()
        .enumerate()
    {
        let answer = fetch(&app, &format!("/v1/workloads/{ns}/front/{suffix}")).await;
        assert_eq!(answer.status, 500, "{suffix}");
        assert!(!String::from_utf8_lossy(&answer.body).contains("not json"));
        assert_eq!(errors(&capture, &ns, "stored report unreadable"), i + 1);
    }
    assert_eq!(errors(&capture, &ns, "openapi render failed"), 0);
}

#[tokio::test]
async fn ui_07_head_answers_like_get_without_a_body_on_every_route() {
    let store = store(&valkey_url());
    let (app, _) = app(&store);
    let ns = unique("ui07h");
    seed(&store, &ns, "front", &["front-1"]).await;
    let base = format!("/v1/workloads/{ns}/front");
    let paths = [
        "/v1/workloads".to_string(),
        base.clone(),
        format!("{base}/report"),
        format!("{base}/endpoints"),
        format!("{base}/openapi"),
        format!("{base}/openapi/inbound-80"),
        format!("{base}/openapi/inbound-81"),
        format!("{base}/openapi/foo"),
        format!("/v1/workloads/{ns}/ghost/endpoints"),
    ];
    for path in paths {
        let got = fetch(&app, &path).await;
        let head = send(&app, Method::HEAD, &path, "127.0.0.1", Some("localhost")).await;
        assert_eq!(head.status, got.status, "{path}");
        assert_eq!(
            head.headers.get("content-type"),
            got.headers.get("content-type"),
            "{path}"
        );
        assert!(head.body.is_empty(), "{path}");
    }
}

#[tokio::test]
async fn ui_07_other_methods_are_405_with_allow_get_head() {
    let store = store(&valkey_url());
    let (app, _) = app(&store);
    let base = "/v1/workloads/shop/front";
    let paths = [
        "/v1/workloads".to_string(),
        base.to_string(),
        format!("{base}/report"),
        format!("{base}/endpoints"),
        format!("{base}/openapi"),
        format!("{base}/openapi/inbound-80"),
    ];
    for method in [Method::POST, Method::DELETE, Method::PUT] {
        for path in &paths {
            let answer = send(&app, method.clone(), path, "127.0.0.1", Some("localhost")).await;
            assert_eq!(answer.status, 405, "{method} {path}");
            assert_eq!(answer.headers["allow"], "GET, HEAD", "{method} {path}");
        }
    }
}

#[tokio::test]
async fn ui_07_the_403_cases_hold_on_every_new_route() {
    let store = store(&valkey_url());
    let (app, _) = app(&store);
    let ns = unique("ui07f");
    seed(&store, &ns, "front", &["front-1"]).await;
    let base = format!("/v1/workloads/{ns}/front");
    for suffix in ["endpoints", "openapi", "openapi/inbound-80"] {
        let path = format!("{base}/{suffix}");
        for method in [Method::GET, Method::HEAD] {
            let cases = [
                ("10.1.2.3", Some("localhost")),
                ("127.0.0.1", Some("evil.example")),
                ("127.0.0.1", None),
            ];
            for (peer, host) in cases {
                let answer = send(&app, method.clone(), &path, peer, host).await;
                assert_eq!(answer.status, 403, "{method} {path} {peer} {host:?}");
                assert!(answer.body.is_empty());
            }
        }
        assert_eq!(fetch(&app, &path).await.status, 200, "{path}");
    }
}

fn series(metrics: &Metrics, name: &str, route: &str) -> Option<u64> {
    let text = metrics.encode();
    text.lines()
        .find(|l| l.starts_with(name) && l.contains(&format!("route=\"{route}\"")))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
}

fn requests(metrics: &Metrics, route: &str, code: u16) -> Option<u64> {
    metrics
        .encode()
        .lines()
        .find(|l| {
            l.starts_with("mimic_ui_requests_total{")
                && l.contains(&format!("route=\"{route}\""))
                && l.contains(&format!("code=\"{code}\""))
        })
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
}

#[tokio::test]
async fn ui_10_metrics_count_answers_and_renders_without_content_or_log_lines() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let store = store(&valkey_url());
    let (app, metrics) = app(&store);
    let ns = unique("ui10m");
    seed(&store, &ns, "front", &["front-1"]).await;
    // The list route may report unreadable data of other tests; a request says nothing of itself.
    let own_lines = || {
        String::from_utf8_lossy(&capture.0.lock().unwrap())
            .lines()
            .filter(|l| l.contains(ns.as_str()))
            .count()
    };
    let base = format!("/v1/workloads/{ns}/front");
    let doc = "/v1/workloads/{namespace}/{deployment}/openapi/{id}";
    for _ in 0..3 {
        let ok = fetch(&app, &format!("{base}/openapi/inbound-80")).await;
        assert_eq!(ok.status, 200);
    }
    let absent = fetch(&app, &format!("{base}/openapi/inbound-81")).await;
    assert_eq!(absent.status, 404);
    assert_eq!(
        fetch(&app, &format!("{base}/openapi/foo")).await.status,
        400
    );
    assert_eq!(fetch(&app, &format!("{base}/endpoints")).await.status, 200);
    assert_eq!(fetch(&app, "/v1/workloads").await.status, 200);
    let local = Some("localhost");
    let head = send(&app, Method::HEAD, "/v1/workloads", "127.0.0.1", local).await;
    assert_eq!(head.status, 200);
    let post = send(&app, Method::POST, "/v1/workloads", "127.0.0.1", local).await;
    assert_eq!(post.status, 405);
    assert_eq!(fetch(&app, "/nope/secret-path").await.status, 404);
    let denied = send(&app, Method::GET, "/v1/workloads", "10.1.2.3", local).await;
    assert_eq!(denied.status, 403);

    assert_eq!(requests(&metrics, doc, 200), Some(3));
    assert_eq!(requests(&metrics, doc, 404), Some(1));
    assert_eq!(requests(&metrics, doc, 400), Some(1));
    let ep = "/v1/workloads/{namespace}/{deployment}/endpoints";
    assert_eq!(requests(&metrics, ep, 200), Some(1));
    // GET and HEAD, then the 405 and the 403 of the list route.
    assert_eq!(requests(&metrics, "/v1/workloads", 200), Some(2));
    assert_eq!(requests(&metrics, "/v1/workloads", 405), Some(1));
    assert_eq!(requests(&metrics, "/v1/workloads", 403), Some(1));
    assert_eq!(requests(&metrics, "other", 404), Some(1));
    // The 404 renders (the report is parsed, the scope looked up); the 400 never does.
    let count = "mimic_ui_render_duration_seconds_count";
    assert_eq!(series(&metrics, count, doc), Some(4));
    assert_eq!(series(&metrics, count, ep), Some(1));
    let text = metrics.encode();
    for leaked in [ns.as_str(), "front", "secret-path", "inbound-80"] {
        assert!(!text.contains(leaked), "a series carries request content");
    }
    assert_eq!(own_lines(), 0, "a request wrote a log line");
}

type Gate = (
    Hook,
    Arc<std::sync::atomic::AtomicUsize>,
    Arc<dyn Fn() + Send + Sync>,
);

/// A hook that blocks each render until `release` is called once for it, and counts entries.
fn gate() -> Gate {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};
    let entered = Arc::new(AtomicUsize::new(0));
    let permits = Arc::new((Mutex::new(0usize), Condvar::new()));
    let hook: Hook = {
        let (entered, permits) = (entered.clone(), permits.clone());
        Arc::new(move || {
            entered.fetch_add(1, Ordering::SeqCst);
            let (lock, cv) = &*permits;
            let mut open = lock.lock().unwrap();
            while *open == 0 {
                open = cv.wait(open).unwrap();
            }
            *open -= 1;
        })
    };
    let release: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let (lock, cv) = &*permits;
        *lock.lock().unwrap() += 1;
        cv.notify_one();
    });
    (hook, entered, release)
}

async fn until(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..500 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn ui_10_at_most_two_renders_run_at_once_and_other_routes_are_not_blocked() {
    use std::sync::atomic::Ordering;
    let store = store(&valkey_url());
    let mut metrics = Metrics::new("controller", "test");
    let (hook, entered, release) = gate();
    let ui = UiMetrics::register(metrics.registry_mut());
    let app = router_with_hook(store.clone(), ui, hook);
    let ns = unique("ui10g");
    seed(&store, &ns, "front", &["front-1"]).await;
    let path = format!("/v1/workloads/{ns}/front/openapi/inbound-80");
    let tasks: Vec<_> = (0..3)
        .map(|_| {
            let (app, path) = (app.clone(), path.clone());
            tokio::spawn(async move { fetch(&app, &path).await.status })
        })
        .collect();
    until("two renders", || entered.load(Ordering::SeqCst) == 2).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(entered.load(Ordering::SeqCst), 2, "a third render started");
    let list = tokio::time::timeout(Duration::from_secs(5), fetch(&app, "/v1/workloads"))
        .await
        .expect("the list is answered while renders are held");
    assert_eq!(list.status, 200);
    release();
    until("the third render", || entered.load(Ordering::SeqCst) == 3).await;
    release();
    release();
    for task in tasks {
        assert_eq!(task.await.unwrap(), 200);
    }
}

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
                   font-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; \
                   frame-ancestors 'none'";

async fn raw(
    store: &Arc<Store>,
    method: Method,
    path: &str,
    peer: &str,
    host: Option<&str>,
) -> axum::response::Response {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("origin", "https://evil.example");
    if let Some(host) = host {
        builder = builder.header("host", host);
    }
    let mut request = builder.body(Body::empty()).unwrap();
    let peer: SocketAddr = format!("{peer}:40000").parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(peer));
    router(store.clone(), UiMetrics::default())
        .oneshot(request)
        .await
        .unwrap()
}

fn assert_secure(response: &axum::response::Response, what: &str) {
    let h = response.headers();
    for (name, value) in [
        ("content-security-policy", CSP),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("x-frame-options", "DENY"),
        ("cross-origin-opener-policy", "same-origin"),
        ("cross-origin-resource-policy", "same-origin"),
        ("cache-control", "no-store"),
    ] {
        assert_eq!(
            h.get(name).and_then(|v| v.to_str().ok()),
            Some(value),
            "{what}: {name}"
        );
    }
    assert!(
        !h.keys().any(|k| k.as_str().starts_with("access-control-")),
        "{what}: CORS header"
    );
}

#[tokio::test]
async fn ui_08_every_answer_carries_the_security_headers_and_no_cors() {
    // Nothing listens on port 1: the store answers 503 without Valkey.
    let store = store("redis://127.0.0.1:1");
    let local = Some("localhost:8080");
    let cases = [
        (Method::GET, "/v1/workloads", "127.0.0.1", local, 503),
        (
            Method::GET,
            "/v1/workloads/Bad_Ns/front",
            "127.0.0.1",
            local,
            400,
        ),
        (Method::GET, "/nothing", "127.0.0.1", local, 404),
        (Method::POST, "/v1/workloads", "127.0.0.1", local, 405),
        (Method::GET, "/v1/workloads", "10.1.2.3", local, 403),
        (
            Method::GET,
            "/v1/workloads",
            "127.0.0.1",
            Some("evil.example"),
            403,
        ),
        (Method::GET, "/v1/workloads", "127.0.0.1", None, 403),
        (Method::GET, "/", "127.0.0.1", local, 308),
    ];
    for (method, path, peer, host, status) in cases {
        let response = raw(&store, method.clone(), path, peer, host).await;
        assert_eq!(response.status().as_u16(), status, "{method} {path}");
        assert_secure(&response, &format!("{method} {path} {status}"));
    }
}

#[tokio::test]
async fn ui_08_success_and_server_error_carry_the_security_headers() {
    let store = store(&valkey_url());
    let ns = unique("ui08");
    seed(&store, &ns, "front", &["front-1"]).await;
    let k = keys(&ns, "front").unwrap();
    for path in [
        "/v1/workloads".to_string(),
        format!("/v1/workloads/{ns}/front"),
        format!("/v1/workloads/{ns}/front/report"),
        format!("/v1/workloads/{ns}/front/endpoints"),
        format!("/v1/workloads/{ns}/missing"),
    ] {
        let response = raw(&store, Method::GET, &path, "127.0.0.1", Some("localhost")).await;
        assert_secure(&response, &path);
    }
    let _: () = redis::cmd("SET")
        .arg(&k.report)
        .arg("{not json")
        .query_async(&mut connection().await)
        .await
        .unwrap();
    let path = format!("/v1/workloads/{ns}/front/report");
    let response = raw(&store, Method::GET, &path, "127.0.0.1", Some("localhost")).await;
    assert_eq!(response.status().as_u16(), 500);
    assert_secure(&response, "500");
}

#[tokio::test]
async fn ui_08_root_redirects_with_308_to_ui() {
    let store = store("redis://127.0.0.1:1");
    let response = raw(
        &store,
        Method::GET,
        "/",
        "127.0.0.1",
        Some("localhost:8080"),
    )
    .await;
    assert_eq!(response.status().as_u16(), 308);
    assert_eq!(response.headers()["location"], "/ui/");
}

/// Opens the gate for good when a test ends, so a failed assertion cannot hang the runtime
/// on held blocking threads.
struct OpenOnDrop(Arc<dyn Fn() + Send + Sync>);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        for _ in 0..16 {
            (self.0)();
        }
    }
}

#[tokio::test]
async fn ui_10_aborted_requests_do_not_free_the_slots_of_running_renders() {
    use std::sync::atomic::Ordering;
    let store = store(&valkey_url());
    let mut metrics = Metrics::new("controller", "test");
    let (hook, entered, release) = gate();
    let ui = UiMetrics::register(metrics.registry_mut());
    let app = router_with_hook(store.clone(), ui, hook);
    let _open = OpenOnDrop(release.clone());
    let ns = unique("ui10a");
    seed(&store, &ns, "front", &["front-1"]).await;
    let path = format!("/v1/workloads/{ns}/front/openapi/inbound-80");
    let spawn = || {
        let (app, path) = (app.clone(), path.clone());
        tokio::spawn(async move { fetch(&app, &path).await.status })
    };
    let first: Vec<_> = (0..2).map(|_| spawn()).collect();
    until("two renders", || entered.load(Ordering::SeqCst) == 2).await;
    for task in &first {
        task.abort();
    }
    for task in first {
        assert!(task.await.unwrap_err().is_cancelled());
    }
    let second: Vec<_> = (0..2).map(|_| spawn()).collect();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        entered.load(Ordering::SeqCst),
        2,
        "a render started while two still ran"
    );
    for _ in 0..4 {
        release();
    }
    for task in second {
        assert_eq!(task.await.unwrap(), 200);
    }
    assert_eq!(entered.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn ui_10_invalid_and_unknown_requests_do_not_wait_for_a_render_slot() {
    use std::sync::atomic::Ordering;
    let store = store(&valkey_url());
    let mut metrics = Metrics::new("controller", "test");
    let (hook, entered, release) = gate();
    let ui = UiMetrics::register(metrics.registry_mut());
    let app = router_with_hook(store.clone(), ui, hook);
    let _open = OpenOnDrop(release.clone());
    let ns = unique("ui10w");
    seed(&store, &ns, "front", &["front-1"]).await;
    let path = format!("/v1/workloads/{ns}/front/openapi/inbound-80");
    let held: Vec<_> = (0..2)
        .map(|_| {
            let (app, path) = (app.clone(), path.clone());
            tokio::spawn(async move { fetch(&app, &path).await.status })
        })
        .collect();
    until("two renders", || entered.load(Ordering::SeqCst) == 2).await;
    for (path, status) in [
        (format!("/v1/workloads/{ns}/front/openapi/bad"), 400),
        (format!("/v1/workloads/{ns}/Bad_Name/endpoints"), 400),
        (format!("/v1/workloads/{ns}/absent/endpoints"), 404),
        (format!("/v1/workloads/{ns}/absent/openapi"), 404),
    ] {
        let answer = tokio::time::timeout(Duration::from_secs(5), fetch(&app, &path))
            .await
            .unwrap_or_else(|_| panic!("{path} waited for a slot"));
        assert_eq!(answer.status, status, "{path}");
    }
    release();
    release();
    for task in held {
        assert_eq!(task.await.unwrap(), 200);
    }
}

#[tokio::test]
async fn ui_10_a_panicking_render_is_a_500_with_one_error() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let store = store(&valkey_url());
    let mut metrics = Metrics::new("controller", "test");
    let ui = UiMetrics::register(metrics.registry_mut());
    let hook: Hook = Arc::new(|| panic!("render blew up"));
    let app = router_with_hook(store.clone(), ui, hook);
    let ns = unique("ui10p");
    seed(&store, &ns, "front", &["front-1"]).await;
    let answer = fetch(&app, &format!("/v1/workloads/{ns}/front/endpoints")).await;
    assert_eq!(answer.status, 500);
    assert_eq!(errors(&capture, &ns, "openapi render failed"), 1);
    // The slot is back: a second request is answered too.
    let again = fetch(&app, &format!("/v1/workloads/{ns}/front/openapi")).await;
    assert_eq!(again.status, 500);
    assert_eq!(errors(&capture, &ns, "openapi render failed"), 2);
}

mod loadtests;
mod outbound;
mod pages;

fn skipped(metrics: &Metrics) -> u64 {
    metrics
        .encode()
        .lines()
        .find(|l| {
            l.starts_with("mimic_ui_skipped_total{") && l.contains("reason=\"meta-unreadable\"")
        })
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

#[tokio::test]
async fn ctl_13_a_list_entry_skipped_for_an_unreadable_meta_is_counted_by_reason() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let store = store(&valkey_url());
    let (app, metrics) = app(&store);
    let ns = unique("ctl13k");
    seed(&store, &ns, "good", &["p1"]).await;
    assert_eq!(fetch(&app, "/v1/workloads").await.status, 200);
    let before = skipped(&metrics);
    seed(&store, &ns, "bad", &["p1"]).await;
    hset(&keys(&ns, "bad").unwrap().meta, "updated", "never").await;
    assert_eq!(fetch(&app, "/v1/workloads").await.status, 200);
    // Other tests share the Valkey and the counter has no namespace: the entry is the one logged
    // under this test's namespace, and the count rose by at least that one.
    assert_eq!(errors(&capture, &ns, "stored meta unreadable"), 1);
    assert!(
        skipped(&metrics) > before,
        "the skipped entry is not counted"
    );
    assert!(
        !metrics.encode().contains(ns.as_str()),
        "a series carries a name"
    );
}
