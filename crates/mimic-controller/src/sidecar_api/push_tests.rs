//! CTL-07, CTL-08, CTL-11 and CTL-12: the push route of the sidecar API (spec §7.2). The
//! integration tests need a Valkey >= 9.0 whose URL is in `MIMIC_TEST_VALKEY_URL`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{Method, Request};
use http_body_util::BodyExt;
use mimic_core::discovery::observed::Observation;
use mimic_core::discovery::redact::{MARKER, Redactor};
use mimic_core::discovery::report::{Direction, Exchange, Limits, Report};
use mimic_core::discovery::store::merge_bounded;
use mimic_runtime::metrics::Metrics;
use serde_json::{Value, json};
use tower::ServiceExt;
use xxhash_rust::xxh3::xxh3_64;

use super::tests::{Reviews, api_server};
use super::*;
use crate::store::{Store, StoreConfig, StoreMetrics, keys};
use crate::workloads::WorkloadCache;

const SESSION: &str = "0123456789abcdef0123456789abcdef";
const SESSION_2: &str = "fedcba9876543210fedcba9876543210";
const ROUTE: &str = "/v1/workloads/{deployment}/report";
const DAY: u64 = 86_400;
const PEER: &str = "10.1.1.1";
const LIMITS: Limits = Limits {
    max_literals: 32,
    max_endpoints: 256,
    sample_budget: 1 << 20,
};

fn today() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / DAY)
}

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

fn pod_of(deployment: &str) -> String {
    format!("{deployment}-7d9f8b6c4d-x2k9p")
}

/// A token the fake API server accepts for the pod of `deployment` in `namespace`; `tag`
/// makes it unique.
fn token(namespace: &str, deployment: &str, tag: impl std::fmt::Display) -> String {
    format!("ns|{namespace}|{}|{tag}", pod_of(deployment))
}

struct Options {
    rate: u32,
    max_report: usize,
    budget: u64,
    max_push: usize,
    max_endpoints: usize,
    max_deployments: usize,
    valkey: String,
    redact: &'static [&'static str],
}

impl Default for Options {
    fn default() -> Options {
        Options {
            rate: 10_000,
            max_report: 1 << 20,
            budget: 1 << 30,
            max_push: 256,
            max_endpoints: 256,
            max_deployments: 50,
            valkey: valkey_url(),
            redact: &[],
        }
    }
}

struct Setup {
    api: Arc<SidecarApi>,
    router: Router,
    metrics: Arc<Metrics>,
    reviews: Reviews,
    /// A namespace no other test uses.
    ns: String,
    limits: Limits,
}

fn setup(options: Options) -> Setup {
    let ns = unique("shop");
    let (client, reviews) = api_server();
    let mut metrics = Metrics::new("controller", "test");
    let limits = Limits {
        max_endpoints: options.max_endpoints,
        ..LIMITS
    };
    let store = Store::new(
        redis::Client::open(options.valkey.as_str()).expect("a valid URL"),
        StoreConfig {
            ttl: Duration::from_secs(30 * DAY),
            limits,
            max_deployments: options.max_deployments,
        },
        StoreMetrics::register(metrics.registry_mut()),
    );
    let api = Arc::new(SidecarApi {
        seed: Arc::new(SeedSlot::new()),
        reviewer: Arc::new(KubeReviewer::new(Arc::new(OnceCell::new_with(Some(
            client,
        ))))),
        metrics: ApiMetrics::register(metrics.registry_mut()),
        limiter: ReviewLimiter::default(),
        rate: RateLimiter::new(options.rate),
        reviews: ReviewCache::default(),
        budget: BodyBudget::new(options.budget),
        store,
        workloads: Arc::new(WorkloadCache::synced_with_settings(&[(
            &ns,
            "pay-workload",
            "pay",
            &[],
            options.redact,
        )])),
        max_report: options.max_report,
        max_push_endpoints: options.max_push,
    });
    Setup {
        router: router(api.clone()),
        api,
        metrics: Arc::new(metrics),
        reviews,
        ns,
        limits,
    }
}

fn exchange(report: &mut Report, path: &str, status: u16) {
    report.record(
        &LIMITS,
        &Redactor::default(),
        &Exchange {
            direction: Direction::Inbound,
            port: 80,
            method: "GET",
            path,
            status,
            head_ms: 3,
            host: Some("svc"),
            grpc: false,
            observation: Observation::default(),
        },
    );
}

fn report_of(paths: &[&str]) -> Report {
    let mut report = Report::new(Some(pod_of("pay")));
    for path in paths {
        exchange(&mut report, path, 200);
    }
    report
}

fn envelope(session: &str, sequence: u64, report: &Report) -> Value {
    let report: Value = serde_json::from_str(&report.to_json().unwrap()).unwrap();
    json!({"session": session, "sequence": sequence, "report": report})
}

fn payload(session: &str, sequence: u64, report: &Report) -> Vec<u8> {
    serde_json::to_vec(&envelope(session, sequence, report)).unwrap()
}

struct Answer {
    status: u16,
    retry_after: Option<String>,
    bytes: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
    }
}

/// One request; `length` is the `Content-Length` header, as declared.
async fn send(
    router: &Router,
    peer: &str,
    method: Method,
    token: Option<&str>,
    deployment: &str,
    length: Option<usize>,
    body: Body,
) -> Answer {
    let mut request = Request::builder()
        .method(method)
        .uri(format!("/v1/workloads/{deployment}/report"))
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    if let Some(length) = length {
        request = request.header("content-length", length.to_string());
    }
    let mut request = request.body(body).unwrap();
    request.extensions_mut().insert(Peer(peer.parse().unwrap()));
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        retry_after,
        bytes: bytes.to_vec(),
    }
}

/// A push of `body` for `deployment` by its pod, in the namespace of `s`.
async fn push(s: &Setup, deployment: &str, body: Vec<u8>) -> Answer {
    let token = token(&s.ns, deployment, "t");
    send(
        &s.router,
        PEER,
        Method::POST,
        Some(&token),
        deployment,
        Some(body.len()),
        Body::from(body),
    )
    .await
}

async fn conn() -> redis::aio::MultiplexedConnection {
    redis::Client::open(valkey_url())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .expect("Valkey is reachable")
}

async fn stored_report(s: &Setup, deployment: &str) -> Option<String> {
    let key = keys(&s.ns, deployment).unwrap().report;
    redis::cmd("GET")
        .arg(key)
        .query_async(&mut conn().await)
        .await
        .unwrap()
}

async fn stored_meta(s: &Setup, deployment: &str) -> BTreeMap<String, String> {
    let key = keys(&s.ns, deployment).unwrap().meta;
    redis::cmd("HGETALL")
        .arg(key)
        .query_async(&mut conn().await)
        .await
        .unwrap()
}

#[tokio::test]
async fn ctl_07_a_push_is_applied_once_and_the_store_holds_the_bounded_merge() {
    let s = setup(Options::default());
    let report = report_of(&["/a", "/b"]);
    let answer = push(&s, "pay", payload(SESSION, 1, &report)).await;
    assert_eq!(
        answer.status,
        200,
        "{}",
        String::from_utf8_lossy(&answer.bytes)
    );
    assert_eq!(answer.json(), json!({"applied": true}));

    let mut stamped = report.clone();
    stamped.stamp(today());
    let expected = merge_bounded(Report::new(None), stamped, &s.limits)
        .report
        .to_json()
        .unwrap();
    let first = stored_report(&s, "pay").await;
    assert_eq!(first, Some(expected));
    let meta_first = stored_meta(&s, "pay").await;

    // The same body again: nothing changes.
    let again = push(&s, "pay", payload(SESSION, 1, &report)).await;
    assert_eq!(
        (again.status, again.json()),
        (200, json!({"applied": false}))
    );
    assert_eq!(stored_report(&s, "pay").await, first);
    assert_eq!(stored_meta(&s, "pay").await, meta_first);

    // The next sequence applies, a lower one of the same session does not.
    let next = push(&s, "pay", payload(SESSION, 2, &report_of(&["/c"]))).await;
    assert_eq!(next.json(), json!({"applied": true}));
    let (report_2, meta_2) = (stored_report(&s, "pay").await, stored_meta(&s, "pay").await);
    assert_ne!(report_2, first);
    let lower = push(&s, "pay", payload(SESSION, 1, &report_of(&["/d"]))).await;
    assert_eq!(
        (lower.status, lower.json()),
        (200, json!({"applied": false}))
    );
    assert_eq!(stored_report(&s, "pay").await, report_2);
    assert_eq!(stored_meta(&s, "pay").await, meta_2);

    // A new session of the same pod applies from sequence 1.
    let fresh = push(&s, "pay", payload(SESSION_2, 1, &report_of(&["/e"]))).await;
    assert_eq!(fresh.json(), json!({"applied": true}));
}

#[tokio::test]
async fn ctl_07_missing_or_unknown_credentials_get_401_and_a_foreign_pod_403() {
    let s = setup(Options::default());
    let body = payload(SESSION, 1, &report_of(&["/a"]));
    let post = |credential: Option<String>, deployment: &'static str| {
        let (router, body) = (s.router.clone(), body.clone());
        async move {
            send(
                &router,
                PEER,
                Method::POST,
                credential.as_deref(),
                deployment,
                Some(body.len()),
                Body::from(body),
            )
            .await
            .status
        }
    };
    for credential in [None, Some("bad"), Some("wrong-audience")] {
        assert_eq!(
            post(credential.map(String::from), "pay").await,
            401,
            "{credential:?}"
        );
    }
    // The token is bound to a `pay` pod.
    assert_eq!(post(Some(token(&s.ns, "pay", "t")), "web").await, 403);
    assert_eq!(post(Some("other-pod".into()), "pay").await, 403);
    assert_eq!(post(Some("no-claim".into()), "pay").await, 403);
    assert_eq!(stored_report(&s, "pay").await, None);
    assert_eq!(stored_report(&s, "web").await, None);
}

#[tokio::test]
async fn ctl_07_malformed_pushes_get_400_and_other_methods_405() {
    let s = setup(Options::default());
    let good = envelope(SESSION, 1, &report_of(&["/a"]));
    let with = |key: &str, value: Value| {
        let mut body = good.clone();
        body[key] = value;
        serde_json::to_vec(&body).unwrap()
    };
    let mut unknown_field = good.clone();
    unknown_field["report"]["bogus"] = json!(1);
    for (name, bad) in [
        ("not json", b"nope".to_vec()),
        ("empty object", b"{}".to_vec()),
        ("short session", with("session", json!("abc"))),
        (
            "upper-case session",
            with("session", json!(SESSION.to_uppercase())),
        ),
        ("sequence 0", with("sequence", json!(0))),
        (
            "sequence over 2^53",
            with("sequence", json!((1u64 << 53) + 1)),
        ),
        ("negative sequence", with("sequence", json!(-1))),
        ("fractional sequence", with("sequence", json!(1.5))),
        (
            "report violation",
            serde_json::to_vec(&unknown_field).unwrap(),
        ),
        ("report is not a report", with("report", json!([1, 2]))),
    ] {
        assert_eq!(push(&s, "pay", bad).await.status, 400, "{name}");
    }
    let tok = token(&s.ns, "pay", "t");
    let body = serde_json::to_vec(&good).unwrap();
    // No Content-Length.
    let missing = send(
        &s.router,
        PEER,
        Method::POST,
        Some(&tok),
        "pay",
        None,
        Body::from(body.clone()),
    )
    .await;
    assert_eq!(missing.status, 400);
    for method in [Method::GET, Method::PUT, Method::DELETE] {
        let answer = send(
            &s.router,
            PEER,
            method,
            Some(&tok),
            "pay",
            Some(body.len()),
            Body::from(body.clone()),
        )
        .await;
        assert_eq!(answer.status, 405);
    }
    assert_eq!(stored_report(&s, "pay").await, None);
}

/// A body that records whether anybody read it, and never yields.
fn watched(read: Arc<AtomicBool>) -> Body {
    Body::from_stream(futures::stream::poll_fn(
        move |_| -> Poll<Option<Result<Bytes, std::io::Error>>> {
            read.store(true, Ordering::SeqCst);
            Poll::Pending
        },
    ))
}

#[tokio::test]
async fn ctl_07_413_and_401_are_answered_before_the_body_is_read() {
    let s = setup(Options {
        max_report: 4096,
        ..Options::default()
    });
    let tok = token(&s.ns, "pay", "t");
    let read = Arc::new(AtomicBool::new(false));
    let answer = send(
        &s.router,
        PEER,
        Method::POST,
        Some(&tok),
        "pay",
        Some(4097),
        watched(read.clone()),
    )
    .await;
    assert_eq!(answer.status, 413);
    assert!(!read.load(Ordering::SeqCst), "the body was read");

    // A request declaring 1 GiB without credentials, with a bad token or for another
    // deployment: refused from the headers alone.
    for (credential, deployment, status) in [
        (None, "pay", 401),
        (Some("bad"), "pay", 401),
        (Some(tok.as_str()), "web", 403),
    ] {
        let read = Arc::new(AtomicBool::new(false));
        let answer = send(
            &s.router,
            PEER,
            Method::POST,
            credential,
            deployment,
            Some(1 << 30),
            watched(read.clone()),
        )
        .await;
        assert_eq!(answer.status, status, "{credential:?}");
        assert!(!read.load(Ordering::SeqCst), "the body was read");
    }

    // A body of exactly the limit is not refused for its size.
    let mut body = payload(SESSION, 1, &report_of(&["/a"]));
    body.resize(4096, b' ');
    assert_eq!(push(&s, "pay", body).await.status, 200);
}

#[tokio::test]
async fn ctl_07_507_for_a_push_over_the_endpoint_limit_and_for_too_many_deployments() {
    let s = setup(Options {
        max_push: 3,
        max_deployments: 2,
        ..Options::default()
    });
    let four = report_of(&["/a", "/b", "/c", "/d"]);
    assert_eq!(
        push(&s, "pay", payload(SESSION, 1, &four)).await.status,
        507
    );
    assert_eq!(stored_report(&s, "pay").await, None, "nothing stored");
    let three = report_of(&["/a", "/b", "/c"]);
    assert_eq!(
        push(&s, "pay", payload(SESSION, 1, &three)).await.status,
        200
    );

    let one = report_of(&["/a"]);
    assert_eq!(push(&s, "web", payload(SESSION, 1, &one)).await.status, 200);
    assert_eq!(push(&s, "job", payload(SESSION, 1, &one)).await.status, 507);
    assert_eq!(stored_report(&s, "job").await, None);
    // The two deployments that have data still apply.
    for deployment in ["pay", "web"] {
        let again = push(&s, deployment, payload(SESSION, 2, &report_of(&["/z"]))).await;
        assert_eq!(
            (again.status, again.json()),
            (200, json!({"applied": true}))
        );
    }
}

#[tokio::test]
async fn ctl_07_503_with_retry_after_when_valkey_is_down_or_the_review_fails() {
    let s = setup(Options {
        valkey: "redis://127.0.0.1:1".to_string(),
        ..Options::default()
    });
    let body = payload(SESSION, 1, &report_of(&["/a"]));
    let down = push(&s, "pay", body.clone()).await;
    assert_eq!((down.status, down.retry_after.as_deref()), (503, Some("1")));

    let boom = send(
        &s.router,
        PEER,
        Method::POST,
        Some("boom"),
        "pay",
        Some(body.len()),
        Body::from(body),
    )
    .await;
    assert_eq!((boom.status, boom.retry_after.as_deref()), (503, Some("1")));
}

#[tokio::test]
async fn ctl_07_503_when_two_concurrent_bodies_exceed_the_budget() {
    let s = setup(Options {
        max_report: 1000,
        budget: 1500,
        ..Options::default()
    });
    let padded = |session: &str, sequence: u64| {
        let mut body = payload(session, sequence, &report_of(&["/a"]));
        body.resize(1000, b' ');
        body
    };
    let first = padded(SESSION, 1);
    let (tx, rx) = futures::channel::mpsc::unbounded::<Result<Bytes, std::io::Error>>();
    tx.unbounded_send(Ok(Bytes::copy_from_slice(&first[..10])))
        .unwrap();
    let held = tokio::spawn({
        let (router, tok) = (s.router.clone(), token(&s.ns, "pay", "t"));
        async move {
            send(
                &router,
                PEER,
                Method::POST,
                Some(&tok),
                "pay",
                Some(1000),
                Body::from_stream(rx),
            )
            .await
        }
    });
    while s.api.budget.used() == 0 {
        tokio::task::yield_now().await;
    }
    let second = push(&s, "pay", padded(SESSION_2, 1)).await;
    assert_eq!(
        (second.status, second.retry_after.as_deref()),
        (503, Some("1"))
    );

    tx.unbounded_send(Ok(Bytes::copy_from_slice(&first[10..])))
        .unwrap();
    drop(tx);
    assert_eq!(held.await.unwrap().status, 200);
    assert_eq!(s.api.budget.used(), 0, "the reservation is released");
    assert_eq!(push(&s, "pay", padded(SESSION_2, 1)).await.status, 200);
}

#[tokio::test(start_paused = true)]
async fn ctl_08_over_the_rate_is_429_without_a_review_and_the_bucket_refills() {
    let s = setup(Options {
        rate: 5,
        ..Options::default()
    });
    // No Content-Length: past admission a request is a 400.
    let hit = |peer: &'static str, tok: String| {
        let router = s.router.clone();
        async move {
            send(
                &router,
                peer,
                Method::POST,
                Some(&tok),
                "pay",
                None,
                Body::empty(),
            )
            .await
        }
    };
    let answers =
        futures::future::join_all((0..30).map(|i| hit("10.1.1.1", token(&s.ns, "pay", i)))).await;
    let limited: Vec<_> = answers.iter().filter(|a| a.status == 429).collect();
    assert_eq!(limited.len(), 20);
    assert!(
        limited
            .iter()
            .all(|a| a.retry_after.as_deref() == Some("1"))
    );
    assert_eq!(answers.len() - limited.len(), 10);
    // Reviews for the requests that were served, none for the 429s.
    assert_eq!(s.reviews.lock().unwrap().len(), 10);
    // Another source is served meanwhile.
    assert_eq!(hit("10.2.2.2", token(&s.ns, "pay", 100)).await.status, 400);

    tokio::time::advance(Duration::from_secs(1)).await;
    let later =
        futures::future::join_all((0..5).map(|_| hit("10.1.1.1", token(&s.ns, "pay", 0)))).await;
    let statuses: Vec<u16> = later.iter().map(|a| a.status).collect();
    assert_eq!(statuses, [400; 5]);
}

#[tokio::test(start_paused = true)]
async fn ctl_08_a_review_is_cached_for_60s_by_token_and_a_failed_one_is_not() {
    let s = setup(Options::default());
    let hit = |tok: String| {
        let router = s.router.clone();
        async move {
            send(
                &router,
                PEER,
                Method::POST,
                Some(&tok),
                "pay",
                None,
                Body::empty(),
            )
            .await
            .status
        }
    };
    let reviews = || s.reviews.lock().unwrap().len();
    let (a, b) = (token(&s.ns, "pay", "a"), token(&s.ns, "pay", "b"));
    assert_eq!(hit(a.clone()).await, 400);
    assert_eq!(hit(a.clone()).await, 400);
    assert_eq!(reviews(), 1, "same token, one review");
    assert_eq!(hit(b).await, 400);
    assert_eq!(reviews(), 2, "another token, its own");
    // A failed review is not cached: the next request is reviewed again.
    for credential in ["boom", "boom", "bad", "bad"] {
        hit(credential.to_string()).await;
    }
    assert_eq!(reviews(), 6);
    tokio::time::advance(Duration::from_secs(59)).await;
    hit(a.clone()).await;
    assert_eq!(reviews(), 6);
    tokio::time::advance(Duration::from_secs(2)).await;
    hit(a).await;
    assert_eq!(reviews(), 7, "expired after 60 s");
}

const JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.c2lnbmF0dXJlLXZhbHVl";
const CARD: &str = "4111111111111111";
const GITHUB: &str = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
const NAMED: &str = "CANARYnamedvalue12345";

/// Replaces every redaction marker of `value` by `secret`, as a faulty sidecar would send it,
/// with the size of each sample that holds one kept true (the check verifies it).
fn plant(value: &mut Value, secret: &str) {
    match value {
        Value::String(s) if s.contains(MARKER) => *s = s.replace(MARKER, secret),
        Value::Array(items) => items.iter_mut().for_each(|i| plant(i, secret)),
        Value::Object(map) => {
            let planted = map.contains_key("request_shape").then(|| {
                Value::Object(map.clone())
                    .to_string()
                    .matches(MARKER)
                    .count()
            });
            map.values_mut().for_each(|i| plant(i, secret));
            if let (Some(count), Some(bytes)) = (planted, map.get("bytes").and_then(Value::as_u64))
            {
                let grown = count * (secret.len() - MARKER.len());
                map.insert("bytes".to_string(), json!(bytes + grown as u64));
            }
        }
        _ => {}
    }
}

/// One exchange whose request header `name` holds `value`.
fn with_header(name: &str, value: &str) -> Report {
    let headers = [(name, value)];
    let mut report = Report::new(Some(pod_of("pay")));
    report.record(
        &LIMITS,
        &Redactor::default(),
        &Exchange {
            direction: Direction::Inbound,
            port: 80,
            method: "GET",
            path: "/p",
            status: 200,
            head_ms: 3,
            host: Some("svc"),
            grpc: false,
            observation: Observation {
                req_headers: &headers,
                ..Observation::default()
            },
        },
    );
    report
}

/// Every key of the namespace of `s`, with its content.
async fn dump(s: &Setup) -> String {
    let mut conn = conn().await;
    let found: Vec<String> = redis::cmd("KEYS")
        .arg(format!("mimic:v1:*{}*", s.ns))
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(!found.is_empty(), "something was stored");
    let mut text = String::new();
    for key in found {
        let kind: String = redis::cmd("TYPE")
            .arg(&key)
            .query_async(&mut conn)
            .await
            .unwrap();
        text += &key;
        match kind.as_str() {
            "string" => {
                let v: Vec<u8> = redis::cmd("GET")
                    .arg(&key)
                    .query_async(&mut conn)
                    .await
                    .unwrap();
                text += &String::from_utf8_lossy(&v);
            }
            "hash" => {
                let v: BTreeMap<String, String> = redis::cmd("HGETALL")
                    .arg(&key)
                    .query_async(&mut conn)
                    .await
                    .unwrap();
                text += &format!("{v:?}");
            }
            "zset" => {
                let v: Vec<String> = redis::cmd("ZRANGE")
                    .arg(&key)
                    .arg(0)
                    .arg(-1)
                    .query_async(&mut conn)
                    .await
                    .unwrap();
                text += &v.join(",");
            }
            other => panic!("unexpected type {other} for {key}"),
        }
        text.push('\n');
    }
    text
}

#[tokio::test]
async fn ctl_11_nothing_sensitive_reaches_valkey_stdout_or_metrics() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let s = setup(Options {
        redact: &["xtrafield"],
        ..Options::default()
    });
    let mut secrets = vec![];
    // Detected forms in a report the sidecar did not redact.
    for (sequence, secret) in [(1, JWT), (2, CARD), (3, GITHUB)] {
        let mut value = envelope(SESSION, sequence, &with_header("x-note", secret));
        plant(&mut value, secret);
        assert!(value.to_string().contains(secret), "planted");
        let body = serde_json::to_vec(&value).unwrap();
        assert_eq!(push(&s, "pay", body).await.status, 200, "{secret}");
        secrets.push(secret);
    }
    // A name that only the MimicWorkload lists: the sidecar kept the value.
    let named = with_header("x-xtrafield", NAMED);
    assert!(
        named.to_json().unwrap().contains(NAMED),
        "the sidecar did not know the name"
    );
    assert_eq!(
        push(&s, "pay", payload(SESSION, 4, &named)).await.status,
        200
    );
    secrets.push(NAMED);

    let bearer = token(&s.ns, "pay", "BEARERCANARY7f3a");
    let body = payload(SESSION, 5, &report_of(&["/q"]));
    let answer = send(
        &s.router,
        PEER,
        Method::POST,
        Some(&bearer),
        "pay",
        Some(body.len()),
        Body::from(body),
    )
    .await;
    assert_eq!(answer.status, 200);
    secrets.push("BEARERCANARY7f3a");

    let stored = dump(&s).await;
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    let metrics = s.metrics.encode();
    for secret in secrets {
        let hash = xxh3_64(secret.as_bytes());
        for needle in [
            secret.to_string(),
            hash.to_string(),
            format!("{hash:x}"),
            format!("{hash:016x}"),
        ] {
            assert!(!stored.contains(&needle), "{needle} in Valkey:\n{stored}");
            assert!(!logs.contains(&needle), "{needle} in the logs:\n{logs}");
            assert!(
                !metrics.contains(&needle),
                "{needle} in the metrics:\n{metrics}"
            );
        }
    }
}

fn lines(capture: &crate::webhook::tests::Capture) -> Vec<Value> {
    String::from_utf8(capture.0.lock().unwrap().clone())
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

fn series(metrics: &Metrics, selector: &str) -> Option<f64> {
    metrics
        .encode()
        .lines()
        .find(|l| l.starts_with(selector))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
}

#[tokio::test]
async fn ctl_12_one_line_per_push_and_the_expected_metrics() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let s = setup(Options {
        max_endpoints: 2,
        max_push: 2,
        ..Options::default()
    });
    let tok = token(&s.ns, "pay", "t");
    let (a, b, c) = ("/CANARYPATH-a", "/CANARYPATH-b", "/CANARYPATH-c");
    let mut bogus = envelope(SESSION, 2, &report_of(&[a]));
    bogus["report"]["bogus"] = json!(1);
    let bodies = [
        payload(SESSION, 1, &report_of(&[a, b])),
        // The same sequence again: a duplicate.
        payload(SESSION, 1, &report_of(&[a])),
        // A report the check refuses.
        serde_json::to_vec(&bogus).unwrap(),
        // Three endpoints when two are allowed in a push.
        payload(SESSION, 2, &report_of(&[a, b, c])),
        // The store is full of count-1 endpoints: a new one is refused.
        payload(SESSION, 2, &report_of(&[c])),
        // A busier one displaces a stored one.
        payload(SESSION, 3, &report_of(&[c, c, c, c, c])),
    ];
    let sizes: Vec<usize> = bodies.iter().map(Vec::len).collect();
    let expected = [200, 200, 400, 507, 200, 200];
    for (body, status) in bodies.into_iter().zip(expected) {
        assert_eq!(push(&s, "pay", body).await.status, status);
    }
    // Unauthenticated, forbidden, too large.
    let body = payload(SESSION, 9, &report_of(&[a]));
    let refused = [
        (Some("bad"), "pay", body.len(), 401),
        (Some(tok.as_str()), "web", body.len(), 403),
        (Some(tok.as_str()), "pay", 2 << 20, 413),
    ];
    for (credential, deployment, length, status) in refused {
        let answer = send(
            &s.router,
            PEER,
            Method::POST,
            credential,
            deployment,
            Some(length),
            Body::from(body.clone()),
        )
        .await;
        assert_eq!(answer.status, status);
    }

    let all = lines(&capture);
    let api: Vec<&Value> = all
        .iter()
        .filter(|l| l["target"] == "sidecar_api")
        .collect();
    assert_eq!(api.len(), 9, "{api:#?}");
    let text = |v: &Value, k: &str| v[k].as_str().unwrap_or_default().to_string();
    let results: Vec<String> = api.iter().map(|l| text(l, "result")).collect();
    assert_eq!(
        results,
        [
            "ok",
            "duplicate",
            "invalid",
            "push_limit",
            "ok",
            "ok",
            "unauthenticated",
            "forbidden",
            "too_large"
        ]
    );
    for line in &api {
        assert_eq!(text(line, "endpoint"), ROUTE);
    }
    for (line, bytes) in api.iter().zip(&sizes) {
        assert_eq!(text(line, "session"), SESSION);
        assert_eq!(line["bytes"], json!(bytes));
    }
    assert_eq!(api[0]["sequence"], json!(1));
    assert_eq!(text(api[2], "reason"), "unknown_field");
    assert_eq!(text(api[3], "reason"), "push_limit");

    let warned = |message: &str| all.iter().filter(|l| l["message"] == message).count();
    assert_eq!(
        warned("store limit reached"),
        1,
        "once per deployment and reason"
    );
    assert_eq!(warned("store evicted"), 1, "once per deployment and kind");

    let metric = |selector: &str| series(&s.metrics, selector);
    let requests = |result: &str| {
        metric(&format!(
            "mimic_sidecar_api_requests_total{{endpoint=\"{ROUTE}\",result=\"{result}\"}}"
        ))
    };
    for (result, count) in [
        ("ok", 3.0),
        ("duplicate", 1.0),
        ("invalid", 1.0),
        ("push_limit", 1.0),
        ("unauthenticated", 1.0),
        ("forbidden", 1.0),
        ("too_large", 1.0),
    ] {
        assert_eq!(requests(result), Some(count), "{result}");
    }
    for (selector, value) in [
        ("mimic_store_pushes_total{result=\"applied\"}", 3.0),
        ("mimic_store_pushes_total{result=\"duplicate\"}", 1.0),
        ("mimic_store_writes_total{result=\"ok\"}", 3.0),
        (
            "mimic_store_refused_total{kind=\"endpoint\",reason=\"endpoint_limit\"}",
            1.0,
        ),
        ("mimic_store_evicted_total{kind=\"endpoint\"}", 1.0),
        ("mimic_store_report_bytes_count", 3.0),
        ("mimic_store_write_duration_seconds_count", 4.0),
    ] {
        assert_eq!(metric(selector), Some(value), "{selector}");
    }

    // No path, token or report content in a line or a series.
    let raw = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    let exposition = s.metrics.encode();
    for needle in ["CANARYPATH", "ns|", "authorization", "Bearer"] {
        assert!(!raw.contains(needle), "{needle} in the logs:\n{raw}");
        assert!(!exposition.contains(needle), "{needle} in the metrics");
    }
}
