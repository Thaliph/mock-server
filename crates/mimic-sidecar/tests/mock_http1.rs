//! MCK-11 (no real egress, HTTP/1), MCK-15 (latency, HTTP/1 part) and MCK-17 (miss logs and
//! canaries), plus the pending cap and the cancellation of spec §8 "Bounds" (MCK-18, the
//! HTTP/1 part that does not need the full load). Every wait has a deadline.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::{Logs, capture_logs, closed_port};
use mimic_core::Mode;
use mimic_core::discovery::egress::EgressSet;
use mimic_core::discovery::observed::Observation;
use mimic_core::discovery::redact::Redactor;
use mimic_core::discovery::report::{Direction as CoreDirection, Exchange, Limits, Report, Via};
use mimic_core::discovery::scope::ScopeKey;
use mimic_runtime::config::Env;
use mimic_runtime::metrics::Metrics;
use mimic_sidecar::egress::Snapshot;
use mimic_sidecar::listener::{Direction, OriginalDst, Shared, TunnelMetrics, spawn_listener};
use mimic_sidecar::mock::Mock;
use mimic_sidecar::mode::{Announced, ModeConfig, ModeState};
use mimic_sidecar::red::ProxyMetrics;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TIMEOUT: Duration = Duration::from_secs(10);
const AUTHORITY: &str = "users.test:8080";

// --- the recorded set ------------------------------------------------------------------

fn limits() -> Limits {
    Limits {
        max_literals: 8,
        max_endpoints: 256,
        max_authorities: 64,
        sample_budget: 16 << 20,
    }
}

struct Ex<'a> {
    method: &'a str,
    path: &'a str,
    status: u16,
    head_ms: u64,
    req_body: Option<&'a str>,
    resp_body: &'a str,
}

fn record(report: &mut Report, ex: &Ex<'_>) {
    let req = [("content-type", "application/json")];
    let resp = [("content-type", "application/json")];
    report.record_at(
        ScopeKey::Authority(AUTHORITY.into()),
        &Via {
            tls: false,
            sni: None,
            dst: None,
        },
        &limits(),
        &Redactor::default(),
        &Exchange {
            direction: CoreDirection::Outbound,
            port: 8080,
            method: ex.method,
            path: ex.path,
            status: ex.status,
            head_ms: ex.head_ms,
            host: None,
            grpc: false,
            observation: Observation {
                query: "",
                req_headers: if ex.req_body.is_some() { &req } else { &[] },
                req_body: ex.req_body.map(str::as_bytes),
                resp_headers: &resp,
                resp_body: Some(ex.resp_body.as_bytes()),
                resp_trailers: &[],
            },
        },
    );
}

/// `GET /users/{users_id}` (12 exchanges, all `head_ms` ms) and `POST /orders`.
fn users_set(head_ms: u64) -> EgressSet {
    let mut report = Report::new(None);
    for id in 1..=12 {
        let path = format!("/users/{id}");
        let body = format!("{{\"id\":{id},\"name\":\"user\"}}");
        record(
            &mut report,
            &Ex {
                method: "GET",
                path: &path,
                status: 200,
                head_ms,
                req_body: None,
                resp_body: &body,
            },
        );
    }
    record(
        &mut report,
        &Ex {
            method: "POST",
            path: "/orders",
            status: 201,
            head_ms,
            req_body: Some("{\"item\":\"a\"}"),
            resp_body: "{\"order\":1}",
        },
    );
    EgressSet::project(&report)
}

// --- the rig --------------------------------------------------------------------------

struct Rig {
    addr: SocketAddr,
    metrics: Arc<Metrics>,
    logs: Logs,
    mode: ModeState,
    snapshot: Snapshot,
    mock: Mock,
    accepted: Arc<AtomicUsize>,
    shared: Arc<Shared>,
}

/// A destination that counts the connections it accepts.
async fn destination() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = listener.accept().await else {
                return;
            };
            count.fetch_add(1, Ordering::SeqCst);
            drop(s);
        }
    });
    (addr, accepted)
}

async fn rig(set: EgressSet, factor: u32, max_pending: usize) -> Rig {
    let (dst, accepted) = destination().await;
    rig_to(set, factor, max_pending, dst, accepted).await
}

async fn rig_to(
    set: EgressSet,
    factor: u32,
    max_pending: usize,
    dst: SocketAddr,
    accepted: Arc<AtomicUsize>,
) -> Rig {
    let logs = capture_logs();
    let mut metrics = Metrics::new("sidecar", "test");
    let config = ModeConfig {
        mode: Mode::Mock,
        factor,
        ..ModeConfig::default()
    };
    let mode = ModeState::start(&config, &mut metrics).await;
    let snapshot = Snapshot::new(mode.clone());
    snapshot.load(set, "\"1\"");
    let mock = Mock::new(snapshot.clone(), max_pending, Redactor::default());
    mock.register(&mut metrics);
    let tunnel = TunnelMetrics::register(&mut metrics);
    let proxy = ProxyMetrics::register(&mut metrics, 50);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let original: OriginalDst = Arc::new(move |_| Ok(dst));
    let shared = Arc::new(
        Shared::new(vec![addr], tunnel, original)
            .with_proxy_metrics(proxy)
            .with_mock(mock.clone()),
    );
    spawn_listener(listener, Direction::Outbound, shared.clone());
    Rig {
        addr,
        metrics: Arc::new(metrics),
        logs,
        mode,
        snapshot,
        mock,
        accepted,
        shared,
    }
}

// --- a small HTTP/1 client --------------------------------------------------------------

struct Reply {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().skip(1).find_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    }
}

async fn read_reply(s: &mut TcpStream, pending: &mut Vec<u8>) -> Option<Reply> {
    loop {
        if let Some(p) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&pending[..p]).into_owned();
            let status = head
                .split(' ')
                .nth(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let length = head
                .lines()
                .find_map(|l| {
                    let (n, v) = l.split_once(':')?;
                    n.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            // A `100 Continue` has no body.
            let length = if status == 100 { 0 } else { length };
            if pending.len() >= p + 4 + length {
                let body = pending[p + 4..p + 4 + length].to_vec();
                pending.drain(..p + 4 + length);
                return Some(Reply { status, head, body });
            }
        }
        let mut buf = vec![0u8; 64 * 1024];
        match tokio::time::timeout(TIMEOUT, s.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => pending.extend_from_slice(&buf[..n]),
            Ok(_) => return None,
            Err(_) => panic!("no answer in {TIMEOUT:?}"),
        }
    }
}

fn get(path: &str, host: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n")
}

/// One request on a fresh connection, answered.
async fn once(addr: SocketAddr, raw: &str) -> Reply {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(raw.as_bytes()).await.unwrap();
    read_reply(&mut s, &mut Vec::new()).await.expect("a reply")
}

fn counter(rig: &Rig, series: &str) -> u64 {
    rig.metrics
        .encode()
        .lines()
        .find(|l| l.starts_with(series))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

async fn gauge_is(rig: &Rig, series: &str, want: u64, within: Duration) {
    let end = Instant::now() + within;
    while counter(rig, series) != want {
        assert!(
            Instant::now() < end,
            "{series} is {}, wanted {want}",
            counter(rig, series)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn lines(logs: &Logs, message: &str) -> Vec<serde_json::Value> {
    logs.text()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["message"] == message)
        .collect()
}

// --- MCK-11 -----------------------------------------------------------------------------

#[tokio::test]
async fn mck_11_a_hit_is_answered_from_the_snapshot_and_a_miss_is_599() {
    let rig = rig(users_set(5), 100, 1024).await;

    let hit = once(rig.addr, &get("/users/3", AUTHORITY)).await;
    assert_eq!(hit.status, 200);
    let tier = hit.header("x-mimic-match").expect("x-mimic-match");
    assert!(["exact", "endpoint", "fallback"].contains(&tier.as_str()));
    let body: serde_json::Value = serde_json::from_slice(&hit.body).unwrap();
    assert_eq!(body["name"], "user");
    assert_eq!(
        hit.header("content-length"),
        Some(hit.body.len().to_string())
    );

    for raw in [
        get("/nothing/here", AUTHORITY),
        get("/users/3", "other.test:8080"),
    ] {
        let miss = once(rig.addr, &raw).await;
        assert_eq!(miss.status, 599);
        assert_eq!(miss.header("x-mimic-miss").as_deref(), Some("1"));
        assert_eq!(miss.header("content-length").as_deref(), Some("0"));
        assert!(miss.body.is_empty());
    }
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn mck_11_connect_and_websocket_upgrades_are_misses() {
    let rig = rig(users_set(5), 100, 1024).await;
    let connect = once(
        rig.addr,
        &format!("CONNECT {AUTHORITY} HTTP/1.1\r\nHost: {AUTHORITY}\r\n\r\n"),
    )
    .await;
    assert_eq!(connect.status, 599);
    assert_eq!(connect.header("x-mimic-miss").as_deref(), Some("1"));
    let upgrade = once(
        rig.addr,
        &format!(
            "GET /users/3 HTTP/1.1\r\nHost: {AUTHORITY}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
        ),
    )
    .await;
    assert_eq!(upgrade.status, 599);
    assert_eq!(upgrade.header("x-mimic-miss").as_deref(), Some("1"));
    gauge_is(
        &rig,
        "mimic_mock_miss_total{reason=\"connect\"}",
        1,
        TIMEOUT,
    )
    .await;
    gauge_is(
        &rig,
        "mimic_mock_miss_total{reason=\"upgrade\"}",
        1,
        TIMEOUT,
    )
    .await;
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn mck_11_nothing_listens_at_the_destination_and_the_answer_is_the_same() {
    let dst = closed_port().await;
    let rig = rig_to(users_set(5), 100, 1024, dst, Arc::default()).await;
    let hit = once(rig.addr, &get("/users/3", AUTHORITY)).await;
    assert_eq!(hit.status, 200);
    let miss = once(rig.addr, &get("/nothing", AUTHORITY)).await;
    assert_eq!(miss.status, 599);
}

#[tokio::test]
async fn mck_11_keep_alive_and_five_pipelined_requests_are_answered_in_order() {
    let rig = rig(users_set(5), 100, 1024).await;
    let mut s = TcpStream::connect(rig.addr).await.unwrap();
    let mut pending = Vec::new();
    // Keep-alive: two requests, one after the other.
    for _ in 0..2 {
        s.write_all(get("/users/3", AUTHORITY).as_bytes())
            .await
            .unwrap();
        assert_eq!(read_reply(&mut s, &mut pending).await.unwrap().status, 200);
    }
    // Pipelined: hit, miss, hit, miss, hit in one write.
    let mut batch = String::new();
    for path in ["/users/1", "/nope", "/users/2", "/nope", "/users/4"] {
        batch.push_str(&get(path, AUTHORITY));
    }
    s.write_all(batch.as_bytes()).await.unwrap();
    let mut statuses = Vec::new();
    for _ in 0..5 {
        statuses.push(read_reply(&mut s, &mut pending).await.unwrap().status);
    }
    assert_eq!(statuses, [200, 599, 200, 599, 200]);
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn mck_11_expect_100_continue_gets_100_then_the_answer() {
    let rig = rig(users_set(5), 100, 1024).await;
    let mut s = TcpStream::connect(rig.addr).await.unwrap();
    let body = "{\"item\":\"a\"}";
    let head = format!(
        "POST /orders HTTP/1.1\r\nHost: {AUTHORITY}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nExpect: 100-continue\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await.unwrap();
    let mut pending = Vec::new();
    let interim = read_reply(&mut s, &mut pending).await.unwrap();
    assert_eq!(interim.status, 100);
    s.write_all(body.as_bytes()).await.unwrap();
    let answer = read_reply(&mut s, &mut pending).await.unwrap();
    assert_eq!(answer.status, 201);
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn mck_11_access_lines_and_metrics_count_each_request_and_nothing_is_recorded() {
    let rig = rig(users_set(5), 100, 1024).await;
    let hit = once(rig.addr, &get("/users/3?x=1", AUTHORITY)).await;
    let tier = hit.header("x-mimic-match").unwrap();
    once(rig.addr, &get("/nothing", AUTHORITY)).await;
    once(rig.addr, &get("/users/3", "other.test:8080")).await;

    let access = rig.logs.access_exactly(3).await;
    let by_path = |p: &str, a: &str| {
        access
            .iter()
            .find(|v| v["path"] == p && v["authority"] == a)
            .unwrap_or_else(|| panic!("no access line for {p}"))
    };
    let hit_line = by_path("/users/3", AUTHORITY);
    assert_eq!(hit_line["mock"], tier.as_str());
    assert!(hit_line.get("mock_miss").is_none_or(|v| v.is_null()));
    assert_eq!(hit_line["direction"], "outbound");
    assert_eq!(hit_line["status"], 200);
    let endpoint = by_path("/nothing", AUTHORITY);
    assert_eq!(endpoint["mock"], "miss");
    assert_eq!(endpoint["mock_miss"], "endpoint");
    assert_eq!(endpoint["status"], 599);
    let authority = by_path("/users/3", "other.test:8080");
    assert_eq!(authority["mock_miss"], "authority");

    assert_eq!(
        counter(
            &rig,
            &format!("mimic_mock_requests_total{{result=\"{tier}\"}}")
        ),
        1
    );
    assert_eq!(
        counter(&rig, "mimic_mock_requests_total{result=\"miss\"}"),
        2
    );
    assert_eq!(
        counter(&rig, "mimic_mock_miss_total{reason=\"endpoint\"}"),
        1
    );
    assert_eq!(
        counter(&rig, "mimic_mock_miss_total{reason=\"authority\"}"),
        1
    );
    let text = rig.metrics.encode();
    let outbound: u64 = text
        .lines()
        .filter(|l| {
            l.starts_with("mimic_http_requests_total{") && l.contains("direction=\"outbound\"")
        })
        .filter_map(|l| l.rsplit(' ').next()?.parse::<u64>().ok())
        .sum();
    assert_eq!(outbound, 3);
    assert!(
        !text.contains("mimic_discovery_exchanges_total{direction=\"outbound\""),
        "{text}"
    );
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn mck_11_discovery_mode_still_forwards() {
    let rig = rig(users_set(5), 100, 1024).await;
    rig.mode
        .observe(Announced {
            mode: Ok(Mode::Discovery),
            factor: None,
        })
        .await;
    // The destination accepts and closes: the sidecar connected, the mock did not answer.
    let mut s = TcpStream::connect(rig.addr).await.unwrap();
    s.write_all(get("/users/3", AUTHORITY).as_bytes())
        .await
        .unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(TIMEOUT, s.read_to_end(&mut out)).await;
    assert!(!String::from_utf8_lossy(&out).contains("x-mimic-match"));
    assert!(rig.accepted.load(Ordering::SeqCst) >= 1);
}

// --- MCK-15 -----------------------------------------------------------------------------

/// Time from the write of the request to the response head.
async fn head_time(addr: SocketAddr, raw: String) -> (Duration, u16) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let begun = Instant::now();
    s.write_all(raw.as_bytes()).await.unwrap();
    let reply = read_reply(&mut s, &mut Vec::new()).await.unwrap();
    (begun.elapsed(), reply.status)
}

#[tokio::test]
async fn mck_15_twenty_mocked_requests_wait_the_drawn_latency() {
    // 200 ms is in bucket 31 (182 to 215 ms).
    let rig = rig(users_set(200), 100, 1024).await;
    let tasks: Vec<_> = (0..20)
        .map(|_| tokio::spawn(head_time(rig.addr, get("/users/3", AUTHORITY))))
        .collect();
    for task in tasks {
        let (elapsed, status) = task.await.unwrap();
        assert_eq!(status, 200);
        assert!(elapsed >= Duration::from_millis(182), "{elapsed:?}");
        assert!(elapsed <= Duration::from_millis(265), "{elapsed:?}");
    }
    let access = rig.logs.access_exactly(20).await;
    for line in access {
        let head = line["head_ms"].as_u64().unwrap();
        assert!((182..=265).contains(&head), "head_ms {head}");
    }
}

#[tokio::test]
async fn mck_15_factor_zero_answers_at_once_and_factor_two_doubles_the_wait() {
    let rig0 = rig(users_set(200), 0, 1024).await;
    let (elapsed, status) = head_time(rig0.addr, get("/users/3", AUTHORITY)).await;
    assert_eq!(status, 200);
    assert!(elapsed < Duration::from_millis(50), "{elapsed:?}");

    let rig2 = rig(users_set(200), 200, 1024).await;
    let (elapsed, status) = head_time(rig2.addr, get("/users/3", AUTHORITY)).await;
    assert_eq!(status, 200);
    assert!(elapsed >= Duration::from_millis(364), "{elapsed:?}");
    assert!(elapsed <= Duration::from_millis(480), "{elapsed:?}");
}

#[tokio::test]
async fn mck_15_misses_are_answered_without_delay() {
    let rig = rig(users_set(200), 100, 1024).await;
    for raw in [
        get("/nothing", AUTHORITY),
        get("/users/3", "other.test:8080"),
    ] {
        let (elapsed, status) = head_time(rig.addr, raw).await;
        assert_eq!(status, 599);
        assert!(elapsed < Duration::from_millis(50), "{elapsed:?}");
    }
}

// --- MCK-17 -----------------------------------------------------------------------------

/// `count` misses, one request at a time per connection, pipelined by 100, on the paths
/// `path(i)`.
async fn misses(rig: &Rig, count: usize, path: impl Fn(usize) -> String) {
    let mut sent = 0;
    while sent < count {
        let n = (count - sent).min(100);
        let mut s = TcpStream::connect(rig.addr).await.unwrap();
        let mut batch = String::new();
        for i in sent..sent + n {
            batch.push_str(&get(&path(i), AUTHORITY));
        }
        s.write_all(batch.as_bytes()).await.unwrap();
        let mut pending = Vec::new();
        for _ in 0..n {
            let reply = read_reply(&mut s, &mut pending).await.unwrap();
            assert_eq!(reply.status, 599);
        }
        sent += n;
    }
}

async fn flip(rig: &Rig) {
    for mode in [Mode::Discovery, Mode::Mock] {
        rig.mode
            .observe(Announced {
                mode: Ok(mode),
                factor: None,
            })
            .await;
    }
}

#[tokio::test]
async fn mck_17_a_signature_is_logged_once_per_mode_period_and_every_miss_is_counted() {
    let rig = rig(users_set(5), 100, 1024).await;
    misses(&rig, 1000, |i| format!("/x{}", i % 3)).await;
    rig.logs.settled().await;
    let logged = lines(&rig.logs, "mock miss");
    assert_eq!(logged.len(), 3);
    for line in &logged {
        assert_eq!(line["level"], "WARN");
        assert_eq!(line["reason"], "endpoint");
        assert_eq!(line["authority"], AUTHORITY);
        assert_eq!(line["method"], "GET");
    }

    // A mode change and back: a signature already logged is logged again.
    flip(&rig).await;
    misses(&rig, 3, |i| format!("/x{i}")).await;
    rig.logs.settled().await;
    assert_eq!(lines(&rig.logs, "mock miss").len(), 6);

    // 300 distinct signatures: 256 lines and one limit line.
    flip(&rig).await;
    let before = lines(&rig.logs, "mock miss").len();
    misses(&rig, 300, |i| format!("/y{i}")).await;
    rig.logs.settled().await;
    assert_eq!(lines(&rig.logs, "mock miss").len() - before, 256);
    assert_eq!(lines(&rig.logs, "mock miss log limit reached").len(), 1);
    assert_eq!(
        counter(&rig, "mimic_mock_miss_total{reason=\"endpoint\"}"),
        1303
    );
}

fn canary() -> String {
    canary_of(32)
}

/// A random alphanumeric string of `len` characters.
fn canary_of(len: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    (0..len)
        .map(|_| char::from(ALPHABET[rand::random_range(0..ALPHABET.len())]))
        .collect()
}

#[tokio::test]
async fn mck_17_no_canary_reaches_a_log_a_metric_the_summary_or_an_answer() {
    let rig = rig(users_set(5), 100, 1024).await;
    // The Google key detector of spec §6 wants 35 characters after `AIza`.
    let (query, header, body, segment) = (canary(), canary(), canary(), canary_of(35));
    let payload = format!("{{\"secret\":\"{body}\"}}");
    let requests = [
        // A miss with canaries everywhere.
        format!(
            "POST /nothing/AIza{segment} HTTP/1.1\r\nHost: {AUTHORITY}\r\nX-Token: {header}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        ),
        format!(
            "GET /nothing?token={query} HTTP/1.1\r\nHost: {AUTHORITY}\r\nAuthorization: Bearer {header}\r\n\r\n"
        ),
        // A hit with canaries in the query and a header.
        format!(
            "GET /users/3?token={query} HTTP/1.1\r\nHost: {AUTHORITY}\r\nX-Token: {header}\r\n\r\n"
        ),
        format!("GET /users/AIza{segment} HTTP/1.1\r\nHost: {AUTHORITY}\r\n\r\n"),
    ];
    let mut answers = String::new();
    for raw in &requests {
        let mut s = TcpStream::connect(rig.addr).await.unwrap();
        s.write_all(raw.as_bytes()).await.unwrap();
        let reply = read_reply(&mut s, &mut Vec::new()).await.unwrap();
        answers.push_str(&reply.head);
        answers.push_str(&String::from_utf8_lossy(&reply.body));
    }
    rig.logs.access_exactly(requests.len()).await;
    let everything = [
        answers,
        rig.logs.text(),
        rig.metrics.encode(),
        rig.snapshot.summary(),
    ]
    .join("\n");
    for secret in [&query, &header, &body, &segment] {
        assert!(
            !everything.contains(secret.as_str()),
            "a canary leaked at {:?}",
            everything
                .find(secret.as_str())
                .map(|i| &everything[i.saturating_sub(120)..i])
        );
    }
}

// --- the pending cap and the cancellation (spec §8 "Bounds") ---------------------------

#[tokio::test]
async fn mck_18_requests_over_the_cap_get_the_overload_answer_and_aborts_release_the_slots() {
    // Factor 100 and 200 ms: every hit waits long enough to stay pending.
    let rig = rig(users_set(200), 10_000, 4).await;
    let mut held = Vec::new();
    for _ in 0..4 {
        let mut s = TcpStream::connect(rig.addr).await.unwrap();
        s.write_all(get("/users/3", AUTHORITY).as_bytes())
            .await
            .unwrap();
        held.push(s);
    }
    gauge_is(&rig, "mimic_mock_pending", 4, TIMEOUT).await;

    let mut over = TcpStream::connect(rig.addr).await.unwrap();
    over.write_all(get("/users/3", AUTHORITY).as_bytes())
        .await
        .unwrap();
    let mut pending = Vec::new();
    let reply = read_reply(&mut over, &mut pending).await.unwrap();
    assert_eq!(reply.status, 599);
    assert_eq!(reply.header("x-mimic-miss").as_deref(), Some("1"));
    // The connection is closed after the answer.
    let mut rest = Vec::new();
    let n = tokio::time::timeout(TIMEOUT, over.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0);
    assert_eq!(
        counter(&rig, "mimic_mock_miss_total{reason=\"overload\"}"),
        1
    );
    assert_eq!(counter(&rig, "mimic_mock_pending"), 4);
    rig.logs.settled().await;
    let warns = lines(&rig.logs, "mock overload");
    assert_eq!(warns.len(), 1);
    assert_eq!(warns[0]["level"], "WARN");
    assert!(lines(&rig.logs, "mock miss").is_empty());

    // Every client aborts: the timers are cancelled at once.
    drop(held);
    gauge_is(&rig, "mimic_mock_pending", 0, Duration::from_secs(1)).await;
    let access = rig.logs.access_exactly(5).await;
    let aborted = access
        .iter()
        .filter(|v| v["result"] == "client_abort")
        .count();
    assert_eq!(aborted, 4);
    rig.logs.settled().await;
    let ended = lines(&rig.logs, "mock overload ended");
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["level"], "INFO");
    let _ = &rig.shared;
}

// --- configuration ----------------------------------------------------------------------

fn config(value: Option<&str>) -> Result<ModeConfig, mimic_runtime::config::ConfigError> {
    let pairs = value
        .map(|v| ("MIMIC_MOCK_MAX_PENDING".to_string(), v.to_string()))
        .into_iter();
    ModeConfig::from_env(&Env::from_pairs(pairs), std::path::Path::new("/trust"))
}

#[test]
fn mck_18_max_pending_defaults_to_1024_and_takes_1_to_65536() {
    assert_eq!(config(None).unwrap().max_pending, 1024);
    assert_eq!(config(Some("1")).unwrap().max_pending, 1);
    assert_eq!(config(Some("65536")).unwrap().max_pending, 65536);
    for bad in ["0", "65537", "-1", "many"] {
        assert!(config(Some(bad)).is_err(), "{bad:?} must be refused");
    }
}

// --- unparseable requests, silence and stalls ---------------------------------------------

#[tokio::test]
async fn mck_11_a_request_hyper_cannot_parse_closes_the_connection_with_an_unframed_line() {
    let rig = rig(users_set(5), 100, 1024).await;
    let mut s = TcpStream::connect(rig.addr).await.unwrap();
    // A space before the colon is not a header.
    s.write_all(
        format!("GET /users/3 HTTP/1.1\r\nHost: {AUTHORITY}\r\nBad Header : x\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(TIMEOUT, s.read_to_end(&mut out))
        .await
        .expect("closed in time")
        .ok();
    // No response of hyper's: the connection is just closed.
    assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
    let access = rig.logs.access_exactly(1).await;
    let line = &access[0];
    assert_eq!(line["result"], "unframed");
    assert_eq!(line["direction"], "outbound");
    for absent in ["status", "method", "path", "duration_ms", "head_ms"] {
        assert!(line.get(absent).is_none_or(|v| v.is_null()), "{absent}");
    }
    let text = rig.metrics.encode();
    let none: u64 = text
        .lines()
        .filter(|l| {
            l.starts_with("mimic_http_requests_total{") && l.contains("status_class=\"none\"")
        })
        .filter_map(|l| l.rsplit(' ').next()?.parse::<u64>().ok())
        .sum();
    assert_eq!(none, 1, "{text}");
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn mck_11_a_client_that_closes_before_any_byte_does_not_dial_the_destination() {
    let rig = rig(users_set(5), 100, 1024).await;
    for _ in 0..3 {
        drop(TcpStream::connect(rig.addr).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn mck_11_a_stalled_request_body_releases_its_pending_slot() {
    let rig = rig(users_set(5), 100, 4).await;
    rig.mock.set_stall(Duration::from_millis(300));
    let mut s = TcpStream::connect(rig.addr).await.unwrap();
    s.write_all(
        format!(
            "POST /orders HTTP/1.1\r\nHost: {AUTHORITY}\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{{\"item\""
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    gauge_is(&rig, "mimic_mock_pending", 1, TIMEOUT).await;
    gauge_is(&rig, "mimic_mock_pending", 0, Duration::from_secs(3)).await;
    let mut out = Vec::new();
    let closed = tokio::time::timeout(TIMEOUT, s.read_to_end(&mut out)).await;
    assert!(closed.is_ok(), "the connection stays open");
}

#[tokio::test]
async fn mck_11_a_head_that_never_completes_closes_the_connection() {
    let rig = rig(users_set(5), 100, 4).await;
    rig.mock.set_stall(Duration::from_millis(300));
    let mut s = TcpStream::connect(rig.addr).await.unwrap();
    s.write_all(b"GET /users/3 HTTP/1.1\r\nHost: users.te")
        .await
        .unwrap();
    let mut out = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
    assert!(closed.is_ok(), "the connection stays open");
}
