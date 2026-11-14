//! RTY-49 to RTY-55: the retries of a Service from the view's policy and the retry budget in the
//! sidecar (spec §7.9 "Retries": "Tries", "Retry budget"): exhaustion, the rate-limited warning,
//! window and isolation, the fallback and L4 paths, the state across views, the metric's labels.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::mesh_pods::{EchoPod, H1Client, Pod, Svc, view_services};
use common::{Logs, capture_logs, closed_by_peer, closed_port, fixture_mesh};
use http_body_util::{BodyExt, Full};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_sidecar::mesh::{MeshConfig, MeshState};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

const BINARY: &[u8] = b"\x00\x01binary\xff\x00protocol-bytes";

fn state() -> MeshState {
    MeshState::new(&MeshConfig {
        enabled: true,
        seed: Some(9),
        refusal_delay: Duration::from_millis(1),
        ..MeshConfig::default()
    })
}

fn hold(state: &MeshState, etag: &mut u32, services: &[Svc]) {
    *etag += 1;
    state.hold(view_services(services), &format!("\"{etag}-a\""));
}

/// The sum of the lines of `name` that carry every fragment of `labels`.
fn metric(metrics: &str, name: &str, labels: &[&str]) -> f64 {
    metrics
        .lines()
        .filter(|l| l.starts_with(name) && labels.iter().all(|f| l.contains(f)))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
}

fn exhausted(metrics: &str, backend: &str) -> f64 {
    metric(
        metrics,
        "mimic_mesh_retry_budget_exhausted_total{",
        &[&format!("backend=\"{backend}\"")],
    )
}

fn skipped_exhausted(metrics: &str) -> f64 {
    metric(
        metrics,
        "mimic_mesh_retries_skipped_total{",
        &["reason=\"exhausted\""],
    )
}

/// A pod that refuses every request: `503`, `x-mimic-error: overloaded`, `x-mimic-endpoint` its
/// own address (RTY-12).
struct Refuser {
    addr: SocketAddr,
    seen: Arc<AtomicUsize>,
}

impl Refuser {
    async fn start(h2: bool) -> Refuser {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let counted = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let counted = counted.clone();
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let counted = counted.clone();
                    async move {
                        let _ = req.into_body().collect().await;
                        counted.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, std::convert::Infallible>(
                            Response::builder()
                                .status(503)
                                .header("x-mimic-error", "overloaded")
                                .header("x-mimic-endpoint", addr.to_string())
                                .body(Full::new(Bytes::new()))
                                .unwrap(),
                        )
                    }
                });
                tokio::spawn(async move {
                    if h2 {
                        let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    } else {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    }
                });
            }
        });
        Refuser { addr, seen }
    }

    fn count(&self) -> usize {
        self.seen.load(Ordering::SeqCst)
    }
}

async fn refusers(n: usize, h2: bool) -> Vec<Refuser> {
    let mut pods = Vec::new();
    for _ in 0..n {
        pods.push(Refuser::start(h2).await);
    }
    pods
}

fn addrs(pods: &[Refuser]) -> Vec<SocketAddr> {
    pods.iter().map(|p| p.addr).collect()
}

fn seen(pods: &[Refuser]) -> usize {
    pods.iter().map(Refuser::count).sum()
}

/// One request; the status, whichever protocol. `n` makes the path and a header value distinct.
async fn send(
    h1: &mut H1Client,
    addr: SocketAddr,
    h2: bool,
    method: &str,
    n: usize,
    body: &[u8],
) -> u16 {
    let path = format!("/secret-path-{n}");
    let value = format!("secret-value-{n}");
    if h2 {
        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut sender, connection) =
            hyper::client::conn::http2::Builder::new(TokioExecutor::new())
                .handshake::<_, Full<Bytes>>(TokioIo::new(stream))
                .await
                .unwrap();
        tokio::spawn(connection);
        let request = Request::builder()
            .method(method)
            .uri(format!("http://cart.shop.svc{path}"))
            .header("x-trace", value)
            .body(Full::new(Bytes::copy_from_slice(body)))
            .unwrap();
        let response = sender.send_request(request).await.unwrap();
        let status = response.status().as_u16();
        let _ = response.into_body().collect().await;
        return status;
    }
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: cart.shop.svc\r\nx-trace: {value}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(body);
    h1.send(&request).await;
    let (head, _) = h1.read().await.expect("an answer");
    head[9..12].parse().unwrap()
}

fn warnings(logs: &Logs) -> Vec<serde_json::Value> {
    logs.text()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["message"] == "retry budget exhausted")
        .collect()
}

#[tokio::test]
async fn rty_49_retries_come_from_the_policy_of_each_service() {
    let logs = capture_logs();
    let (a_open, b_open) = (Pod::http1("a", 0).await, Pod::http1("b", 0).await);
    let (a_open2, b_open2) = (Pod::http1("a", 0).await, Pod::http1("b", 0).await);
    let a_pods = [a_open.addr, a_open2.addr, closed_port().await];
    let b_pods = [b_open.addr, b_open2.addr, closed_port().await];
    let state = state();
    let mut etag = 0;
    let services = |a_retries| {
        [
            Svc::new("shop/a", "127.0.0.2", 80, &a_pods).retries(a_retries),
            Svc::new("shop/b", "127.0.0.3", 80, &b_pods),
        ]
    };
    hold(&state, &mut etag, &services(0));
    let fa = fixture_mesh("127.0.0.2:80".parse().unwrap(), &state, None).await;
    let fb = fixture_mesh("127.0.0.3:80".parse().unwrap(), &state, None).await;
    // Every request to `b` is answered, however many of them meet its refusing pod.
    let mut b = H1Client::connect(fb.addr).await;
    for n in 0..40 {
        assert_eq!(send(&mut b, fb.addr, false, "GET", n, b"").await, 200);
    }
    // `a` has no retry: the request that meets its refusing pod is answered 503 after 1 try.
    let mut a = H1Client::connect(fa.addr).await;
    let mut refused = None;
    for n in 0..40 {
        if send(&mut a, fa.addr, false, "GET", n, b"").await == 503 {
            refused = Some(n);
            break;
        }
    }
    assert!(refused.is_some(), "a request met the refusing pod");
    let mut a = H1Client::connect(fa.addr).await;
    let scrape = fa.metrics.encode();
    assert_eq!(skipped_exhausted(&scrape), 1.0, "{scrape}");
    assert_eq!(
        metric(&scrape, "mimic_mesh_fallbacks_total", &[]),
        0.0,
        "no fallback"
    );
    let lines = logs.access();
    let last = lines.last().expect("an access line");
    assert_eq!(last["attempts"], 1, "{last}");
    // The next view gives `a` one retry: every request is answered, in at most 2 tries.
    hold(&state, &mut etag, &services(1));
    let before = logs.access().len();
    for n in 0..40 {
        assert_eq!(send(&mut a, fa.addr, false, "GET", n, b"").await, 200);
    }
    logs.settled().await;
    for line in &logs.access()[before..] {
        assert!(
            line["attempts"].as_u64().unwrap() <= 2,
            "at most 2 attempts per request"
        );
    }
}

async fn exhaust(h2: bool) -> (MeshState, Vec<Refuser>, Logs, std::time::Duration) {
    let logs = capture_logs();
    let pods = refusers(3, h2).await;
    let state = state();
    let mut etag = 0;
    hold(
        &state,
        &mut etag,
        &[Svc::new("shop/cart", "127.0.0.2", 80, &addrs(&pods)).budget(20, 1, 10)],
    );
    let f = fixture_mesh("127.0.0.2:80".parse().unwrap(), &state, None).await;
    let began = Instant::now();
    let mut client = H1Client::connect(f.addr).await;
    for n in 0..100 {
        let status = send(&mut client, f.addr, h2, "POST", n, &[7u8; 1024]).await;
        assert_eq!(status, 503, "a refusal as received, request {n}");
    }
    let elapsed = began.elapsed();
    assert!(elapsed < Duration::from_secs(9), "{elapsed:?}");
    let scrape = f.metrics.encode();
    assert_eq!(seen(&pods), 130, "tries seen by the pods");
    assert_eq!(
        metric(
            &scrape,
            "mimic_mesh_retries_total{",
            &["reason=\"overloaded\""]
        ),
        30.0,
        "{scrape}"
    );
    assert_eq!(skipped_exhausted(&scrape), 3.0, "{scrape}");
    assert_eq!(exhausted(&scrape, "shop/cart:80"), 97.0, "{scrape}");
    (state, pods, logs, elapsed)
}

#[tokio::test]
async fn rty_50_an_exhausted_budget_returns_the_refusals_as_received_http1() {
    let (_state, _pods, logs, _) = exhaust(false).await;
    let lines = logs.access_exactly(100).await;
    let tries: Vec<u64> = lines
        .iter()
        .map(|l| l["attempts"].as_u64().unwrap())
        .collect();
    assert_eq!(tries[..5], [4, 4, 4, 2, 2], "{tries:?}");
    assert!(tries[5..].iter().all(|n| (1..=2).contains(n)), "{tries:?}");
}

#[tokio::test]
async fn rty_50_an_exhausted_budget_returns_the_refusals_as_received_h2c() {
    let _ = exhaust(true).await;
}

#[tokio::test]
async fn rty_51_the_warning_is_rate_limited_and_holds_no_request_data() {
    let (state, pods, logs, elapsed) = exhaust(false).await;
    logs.settled().await;
    let lines = warnings(&logs);
    let most = 1 + elapsed.as_secs_f64().ceil() as usize;
    assert!(
        (1..=most).contains(&lines.len()),
        "{} lines over {elapsed:?}",
        lines.len()
    );
    for line in &lines {
        for field in [
            "service",
            "backend",
            "window_requests",
            "window_retries",
            "suppressed",
        ] {
            assert!(!line[field].is_null(), "a warning field is missing");
        }
        assert_eq!(line["backend"], "shop/cart:80");
        assert_eq!(line["service"], "shop/cart");
    }
    // One more refused request after more than a second logs a line; the lines and their
    // `suppressed` then add up to the refusals the metric counts.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let f = fixture_mesh("127.0.0.2:80".parse().unwrap(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    assert_eq!(send(&mut client, f.addr, false, "GET", 500, b"").await, 503);
    logs.settled().await;
    let lines = warnings(&logs);
    let total: u64 = lines
        .iter()
        .map(|l| 1 + l["suppressed"].as_u64().unwrap())
        .sum();
    assert_eq!(total, 98, "98 refusals in all");
    let scrape = f.metrics.encode();
    assert_eq!(exhausted(&scrape, "shop/cart:80"), 98.0, "{scrape}");
    let text = logs.text();
    for needle in ["secret-path", "secret-value", "127.0.0.1"] {
        for line in text
            .lines()
            .filter(|l| l.contains("retry budget exhausted"))
        {
            assert!(!line.contains(needle), "the warning holds request data");
        }
    }
    drop(pods);
}

#[tokio::test]
async fn rty_51_two_services_exhausted_in_the_same_second_each_log_a_line() {
    let logs = capture_logs();
    let (a, b) = (refusers(1, false).await, refusers(1, false).await);
    let state = state();
    let mut etag = 0;
    hold(
        &state,
        &mut etag,
        &[
            Svc::new("shop/a", "127.0.0.2", 80, &addrs(&a)).budget(0, 0, 10),
            Svc::new("shop/b", "127.0.0.3", 80, &addrs(&b)).budget(0, 0, 10),
        ],
    );
    for ip in ["127.0.0.2:80", "127.0.0.3:80"] {
        let f = fixture_mesh(ip.parse().unwrap(), &state, None).await;
        let mut client = H1Client::connect(f.addr).await;
        assert_eq!(send(&mut client, f.addr, false, "GET", 1, b"").await, 503);
    }
    logs.settled().await;
    let mut services: Vec<String> = warnings(&logs)
        .iter()
        .map(|l| l["service"].as_str().unwrap().to_string())
        .collect();
    services.sort();
    assert_eq!(services, ["shop/a", "shop/b"]);
}

#[tokio::test]
async fn rty_52_the_window_slides_and_services_and_ports_are_isolated() {
    let _logs = capture_logs();
    let pods = refusers(3, false).await;
    let state = state();
    let mut etag = 0;
    hold(
        &state,
        &mut etag,
        &[Svc::new("shop/cart", "127.0.0.2", 80, &addrs(&pods)).budget(0, 2, 1)],
    );
    let f = fixture_mesh("127.0.0.2:80".parse().unwrap(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for n in 0..5 {
        assert_eq!(send(&mut client, f.addr, false, "GET", n, b"").await, 503);
    }
    assert_eq!(seen(&pods), 3 + 4, "2 retries for the first, none after");
    let scrape = f.metrics.encode();
    assert_eq!(exhausted(&scrape, "shop/cart:80"), 5.0, "{scrape}");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let before = seen(&pods);
    assert_eq!(send(&mut client, f.addr, false, "GET", 9, b"").await, 503);
    assert_eq!(seen(&pods) - before, 3, "2 retries again");
}

#[tokio::test]
async fn rty_52_exhausting_a_service_leaves_another_whole_and_ports_share_one_budget() {
    let _logs = capture_logs();
    let (a, b) = (refusers(3, false).await, refusers(3, false).await);
    let state = state();
    let mut etag = 0;
    let mut svc_a = Svc::new("shop/a", "127.0.0.2", 80, &addrs(&a)).budget(0, 1, 2);
    svc_a.ports.push((81, addrs(&a)));
    hold(
        &state,
        &mut etag,
        &[svc_a, Svc::new("shop/b", "127.0.0.3", 80, &addrs(&b))],
    );
    let fa80 = fixture_mesh("127.0.0.2:80".parse().unwrap(), &state, None).await;
    let fa81 = fixture_mesh("127.0.0.2:81".parse().unwrap(), &state, None).await;
    let fb = fixture_mesh("127.0.0.3:80".parse().unwrap(), &state, None).await;
    let mut c80 = H1Client::connect(fa80.addr).await;
    let mut c81 = H1Client::connect(fa81.addr).await;
    assert_eq!(send(&mut c80, fa80.addr, false, "GET", 1, b"").await, 503);
    assert_eq!(seen(&a), 3, "2 retries on port 80 used the budget");
    assert_eq!(send(&mut c81, fa81.addr, false, "GET", 2, b"").await, 503);
    assert_eq!(seen(&a), 4, "port 81 shares it: none left");
    let scrape = fa80.metrics.encode();
    assert_eq!(exhausted(&scrape, "shop/a:80"), 1.0, "{scrape}");
    assert_eq!(exhausted(&scrape, "shop/a:81"), 1.0, "{scrape}");
    let mut cb = H1Client::connect(fb.addr).await;
    assert_eq!(send(&mut cb, fb.addr, false, "GET", 3, b"").await, 503);
    assert_eq!(seen(&b), 4, "b got its 3 retries");
    assert_eq!(exhausted(&fb.metrics.encode(), "shop/b:80"), 0.0);
}

#[tokio::test]
async fn rty_53_the_fallback_needs_the_budget() {
    let _logs = capture_logs();
    let cluster = Pod::http1("cluster", 0).await;
    let state = state();
    let mut etag = 0;
    // The ClusterIP of the Service is where the cluster pod listens: the original destination.
    let svc = Svc::new(
        "shop/cart",
        "127.0.0.1",
        cluster.addr.port(),
        &[closed_port().await],
    )
    .budget(0, 0, 10);
    hold(&state, &mut etag, &[svc]);
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let (head, body) = H1Client::connect(f.addr).await.get("/").await;
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    assert!(
        head.to_lowercase()
            .contains("x-mimic-error: connect-failed"),
        "{head}"
    );
    assert!(body.is_empty());
    let scrape = f.metrics.encode();
    let backend = format!("shop/cart:{}", cluster.addr.port());
    assert_eq!(metric(&scrape, "mimic_mesh_fallbacks_total", &[]), 0.0);
    assert_eq!(exhausted(&scrape, &backend), 1.0, "{scrape}");
    assert_eq!(connect_failed(&scrape), 1.0, "{scrape}");
    assert_eq!(cluster.counts.requests(), 0);
}

async fn l4_state(budget: (u32, u32)) -> (MeshState, common::Fixture) {
    let state = state();
    let mut pods = Vec::new();
    for _ in 0..5 {
        pods.push(closed_port().await);
    }
    let mut etag = 0;
    let svc = Svc::new("shop/cart", "127.0.0.2", 80, &pods).budget(budget.0, budget.1, 10);
    hold(&state, &mut etag, &[svc]);
    let f = fixture_mesh("127.0.0.2:80".parse().unwrap(), &state, None).await;
    (state, f)
}

fn connect_failed(scrape: &str) -> f64 {
    metric(
        scrape,
        "mimic_mesh_attempts_total{",
        &["result=\"connect_failed\""],
    )
}

#[tokio::test]
async fn rty_53_an_l4_connection_over_budget_is_closed_after_one_connect() {
    let _logs = capture_logs();
    let (_state, f) = l4_state((0, 0)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    let _ = c.write_all(BINARY).await;
    closed_by_peer(c).await;
    let scrape = f.metrics.encode();
    assert_eq!(connect_failed(&scrape), 1.0, "{scrape}");
    assert_eq!(exhausted(&scrape, "shop/cart:80"), 1.0, "{scrape}");
}

#[tokio::test]
async fn rty_53_an_l4_connection_counts_as_one_request_of_the_budget() {
    let _logs = capture_logs();
    // percent 100, no floor: one request allows one retry, so 2 connects and a refusal.
    let (_state, f) = l4_state((100, 0)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    let _ = c.write_all(BINARY).await;
    closed_by_peer(c).await;
    let scrape = f.metrics.encode();
    assert_eq!(connect_failed(&scrape), 2.0, "{scrape}");
    assert_eq!(exhausted(&scrape, "shop/cart:80"), 1.0, "{scrape}");
}

#[tokio::test]
async fn rty_53_an_l4_connection_with_the_default_budget_reaches_the_original_destination() {
    let _logs = capture_logs();
    let cluster = EchoPod::start().await;
    let state = state();
    let mut pods = Vec::new();
    for _ in 0..5 {
        pods.push(closed_port().await);
    }
    let mut etag = 0;
    let svc = Svc::new("shop/cart", "127.0.0.1", cluster.addr.port(), &pods);
    hold(&state, &mut etag, &[svc]);
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(BINARY).await.unwrap();
    let mut got = vec![0u8; BINARY.len()];
    tokio::io::AsyncReadExt::read_exact(&mut c, &mut got)
        .await
        .unwrap();
    assert_eq!(got, BINARY);
    let scrape = f.metrics.encode();
    assert_eq!(connect_failed(&scrape), 3.0, "{scrape}");
    assert_eq!(
        metric(&scrape, "mimic_mesh_retry_budget_exhausted_total{", &[]),
        0.0
    );
}

#[tokio::test]
async fn rty_54_state_survives_views_and_is_bounded_by_the_services() {
    let _logs = capture_logs();
    let pods = refusers(3, false).await;
    let extra = Refuser::start(false).await;
    let state = state();
    let mut etag = 0;
    let mut addresses = addrs(&pods);
    let svc = |pods: &[SocketAddr], percent, window| {
        Svc::new("shop/cart", "127.0.0.2", 80, pods).budget(percent, 1, window)
    };
    hold(&state, &mut etag, &[svc(&addresses, 0, 10)]);
    let f = fixture_mesh("127.0.0.2:80".parse().unwrap(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    // 10 retries in the window: three requests use 9, the fourth gets one and is refused.
    for n in 0..4 {
        assert_eq!(send(&mut client, f.addr, false, "GET", n, b"").await, 503);
    }
    assert_eq!(seen(&pods), 4 + 10);
    // A pod added: still exhausted.
    addresses.push(extra.addr);
    hold(&state, &mut etag, &[svc(&addresses, 0, 10)]);
    let before = seen(&pods) + extra.count();
    assert_eq!(send(&mut client, f.addr, false, "GET", 5, b"").await, 503);
    assert_eq!(seen(&pods) + extra.count() - before, 1, "still exhausted");
    // A new percent applies to the counts held: 100 % grants the next retry.
    hold(&state, &mut etag, &[svc(&addresses, 100, 10)]);
    let before = seen(&pods) + extra.count();
    assert_eq!(send(&mut client, f.addr, false, "GET", 6, b"").await, 503);
    assert_eq!(seen(&pods) + extra.count() - before, 4, "3 retries granted");
    // A new window starts the budget empty.
    hold(&state, &mut etag, &[svc(&addresses, 0, 5)]);
    let before = seen(&pods) + extra.count();
    assert_eq!(send(&mut client, f.addr, false, "GET", 7, b"").await, 503);
    assert_eq!(seen(&pods) + extra.count() - before, 4, "empty budget");
    assert_eq!(state.retry_state_for_test(), (1, 1));
    // The Service leaves the view and comes back: an empty budget (it had 5 retries, 3 used).
    hold(&state, &mut etag, &[]);
    assert_eq!(state.retry_state_for_test(), (0, 0), "nothing held");
    hold(&state, &mut etag, &[svc(&addresses, 0, 5)]);
    assert_eq!(state.retry_state_for_test(), (1, 1));
    // A connection made before the Service left keeps the backend it was routed to.
    let mut client = H1Client::connect(f.addr).await;
    for n in 8..10 {
        let before = seen(&pods) + extra.count();
        assert_eq!(send(&mut client, f.addr, false, "GET", n, b"").await, 503);
        let tries = seen(&pods) + extra.count() - before;
        assert_eq!(tries, if n == 8 { 4 } else { 3 }, "5 retries in all");
    }
}

#[tokio::test]
async fn rty_55_the_metric_has_the_backend_label_only_and_is_evicted_with_the_backend() {
    let _logs = capture_logs();
    let pods = refusers(2, false).await;
    let state = state();
    let mut etag = 0;
    let svc = || Svc::new("shop/cart", "127.0.0.2", 80, &addrs(&pods)).budget(0, 0, 10);
    hold(&state, &mut etag, &[svc()]);
    let f = fixture_mesh("127.0.0.2:80".parse().unwrap(), &state, None).await;
    for n in 0..1000 {
        let method = ["GET", "POST", "PURGE"][n % 3];
        let mut client = H1Client::connect(f.addr).await;
        assert_eq!(send(&mut client, f.addr, false, method, n, b"").await, 503);
    }
    let scrape = f.metrics.encode();
    let series: Vec<&str> = scrape
        .lines()
        .filter(|l| l.starts_with("mimic_mesh_retry_budget_exhausted_total"))
        .collect();
    assert_eq!(series.len(), 1, "{series:?}");
    assert!(
        series[0]
            .starts_with("mimic_mesh_retry_budget_exhausted_total{backend=\"shop/cart:80\"} 1000"),
        "{}",
        series[0]
    );
    // The backend leaves; its series is removed 5 minutes later, kept when it comes back.
    hold(&state, &mut etag, &[]);
    state.evict(Instant::now() + Duration::from_secs(200));
    assert_eq!(exhausted(&f.metrics.encode(), "shop/cart:80"), 1000.0);
    hold(&state, &mut etag, &[svc()]);
    state.evict(Instant::now() + Duration::from_secs(301));
    assert!(
        f.metrics
            .encode()
            .contains("mimic_mesh_retry_budget_exhausted_total{backend=\"shop/cart:80\"}")
    );
    hold(&state, &mut etag, &[]);
    state.evict(Instant::now() + Duration::from_secs(301));
    assert!(
        !f.metrics
            .encode()
            .contains("mimic_mesh_retry_budget_exhausted_total{"),
        "removed"
    );
}
