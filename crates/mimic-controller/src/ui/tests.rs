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
        report,
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
    let response = router(store.clone()).oneshot(request).await.unwrap();
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
