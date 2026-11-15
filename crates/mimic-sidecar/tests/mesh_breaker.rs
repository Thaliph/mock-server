//! CBR-25 to CBR-31: the circuit breaker of the sidecar (spec §7.9 "Circuit breaker", §12.27):
//! `maxRequests` per caller pod and backend Service, the `breaker-open` answer, the access
//! `reason`, and `maxConnections`.

mod common;

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::mesh_pods::{EchoPod, H1Client, Pod, RawPod};
use common::mock_rig::{Options, rig_with, users_set};
use common::{Logs, capture_logs, closed_port, fixture_mesh};
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::Mode;
use mimic_core::mesh::view::{Limits, View};
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
use mimic_sidecar::mesh::{MeshConfig, MeshState};
use mimic_sidecar::mode::Announced;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// A Service of the view: its ports with their pods, its ClusterIP (none: headless), its
/// breaker and its routes.
struct Svc {
    key: &'static str,
    ip: Option<String>,
    ports: Vec<(u16, Vec<SocketAddr>)>,
    /// `maxRequests`, `maxConnections`.
    breaker: (u32, u32),
    /// `baseEjectionSeconds`, `consecutive5xx`, `consecutiveFailures`, `maxEjectionPercent`.
    outlier: (u32, u32, u32, u32),
    routes: String,
}

fn svc(key: &'static str, ip: Option<&str>, port: u16, pods: &[SocketAddr]) -> Svc {
    Svc {
        key,
        ip: ip.map(str::to_string),
        ports: vec![(port, pods.to_vec())],
        breaker: (1024, 1024),
        outlier: (30, 0, 5, 50),
        routes: String::new(),
    }
}

impl Svc {
    fn breaker(mut self, max_requests: u32, max_connections: u32) -> Svc {
        self.breaker = (max_requests, max_connections);
        self
    }

    fn outlier(mut self, base: u32, fives: u32, failures: u32, percent: u32) -> Svc {
        self.outlier = (base, fives, failures, percent);
        self
    }

    fn routes(mut self, routes: &str) -> Svc {
        self.routes = format!(r#","routes":[{routes}]"#);
        self
    }

    fn json(&self) -> String {
        let (requests, connections) = self.breaker;
        let (base, fives, failures, percent) = self.outlier;
        let endpoints: Vec<String> = self
            .ports
            .iter()
            .map(|(port, pods)| {
                let list: Vec<String> = pods
                    .iter()
                    .map(|p| format!(r#"{{"ip":"{}","port":{},"ready":true}}"#, p.ip(), p.port()))
                    .collect();
                format!(r#""{port}":[{}]"#, list.join(","))
            })
            .collect();
        let ports: Vec<String> = self
            .ports
            .iter()
            .map(|(port, _)| format!(r#""{port}":{{"targetPort":8080}}"#))
            .collect();
        let ips = self
            .ip
            .as_ref()
            .map(|i| format!("\"{i}\""))
            .unwrap_or_default();
        format!(
            r#""{}":{{"endpoints":{{{}}},"generations":{{}},"ips":[{ips}],"policy":{{"circuitBreaker":{{"maxConnections":{connections},"maxRequests":{requests}}},"loadBalancer":{{"slowStartSeconds":0}},"outlierDetection":{{"baseEjectionSeconds":{base},"consecutive5xx":{fives},"consecutiveFailures":{failures},"maxEjectionPercent":{percent}}},"retries":3,"retryBudget":{{"minPerSecond":10,"percent":20,"windowSeconds":10}}}},"ports":{{{}}}{}}}"#,
            self.key,
            endpoints.join(","),
            ports.join(","),
            self.routes,
        )
    }
}

fn view(services: &[Svc]) -> View {
    let text = format!(
        r#"{{"cluster":"local","format":1,"generations":{{}},"mesh":true,"mode":"none","services":{{{}}}}}"#,
        services.iter().map(Svc::json).collect::<Vec<_>>().join(",")
    );
    View::from_json(&text, &Limits::default()).unwrap()
}

fn cart(pods: &[SocketAddr]) -> Svc {
    svc("shop/cart", Some("127.0.0.1"), 80, pods)
}

fn dst() -> SocketAddr {
    "127.0.0.1:80".parse().unwrap()
}

fn state_with(services: &[Svc], configure: impl FnOnce(&mut MeshConfig)) -> MeshState {
    let mut config = MeshConfig {
        enabled: true,
        seed: Some(21),
        ..MeshConfig::default()
    };
    configure(&mut config);
    let state = MeshState::new(&config);
    state.hold(view(services), "\"1-a\"");
    state
}

fn metric(scrape: &str, name: &str, labels: &[&str]) -> f64 {
    scrape
        .lines()
        .filter(|l| l.starts_with(name) && labels.iter().all(|f| l.contains(f)))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
}

/// The lines of the log with this message.
async fn lines(logs: &Logs, message: &str) -> Vec<serde_json::Value> {
    logs.settled().await;
    logs.text()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["message"] == message)
        .collect()
}

/// The access lines whose `reason` is this one, after the log caught up.
async fn reasoned(logs: &Logs, reason: &str) -> Vec<serde_json::Value> {
    logs.settled().await;
    logs.access()
        .into_iter()
        .filter(|l| l["reason"] == reason)
        .collect()
}

/// Waits (at most 5 s) until `cond` holds.
async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never: {what}");
}

fn lower(head: &str) -> String {
    head.to_ascii_lowercase()
}

/// The answer of a breaker that is open (spec §7.9 "Circuit breaker"): exactly one
/// `x-mimic-error`, no pod named, no body.
fn assert_open(head: &str, body: &[u8]) {
    let head = lower(head);
    assert!(head.starts_with("http/1.1 503"), "{head}");
    assert_eq!(head.matches("x-mimic-error").count(), 1, "{head}");
    assert!(head.contains("x-mimic-error: breaker-open"), "{head}");
    assert!(head.contains("content-length: 0"), "{head}");
    assert!(!head.contains("x-mimic-endpoint"), "{head}");
    assert!(body.is_empty());
}

/// One request on a connection of its own: the head, the body and how long it took.
fn spawn_get(addr: SocketAddr, path: &'static str) -> JoinHandle<(String, Vec<u8>, Duration)> {
    tokio::spawn(async move {
        let mut client = H1Client::connect(addr).await;
        let began = Instant::now();
        client
            .send(format!("GET {path} HTTP/1.1\r\nHost: cart.shop.svc:80\r\n\r\n").as_bytes())
            .await;
        let (head, body) = client.read().await.expect("an answer");
        (head, body, began.elapsed())
    })
}

async fn code(job: JoinHandle<(String, Vec<u8>, Duration)>) -> u16 {
    let (head, ..) = job.await.unwrap();
    head[9..12].parse().unwrap()
}

type H2Sender = hyper::client::conn::http2::SendRequest<Empty<Bytes>>;

async fn h2_client(addr: SocketAddr) -> H2Sender {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, Empty<Bytes>>(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    sender
}

/// One `h2c` stream: the status, the `x-mimic-error`, whether `x-mimic-endpoint` came, and how
/// long it took.
fn h2_get(sender: &H2Sender) -> JoinHandle<(u16, Option<String>, bool, Duration)> {
    let mut sender = sender.clone();
    tokio::spawn(async move {
        let began = Instant::now();
        let request = hyper::Request::get("http://cart.shop.svc/")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let response = sender.send_request(request).await.expect("an answer");
        let (status, error, endpoint) = (
            response.status().as_u16(),
            response
                .headers()
                .get("x-mimic-error")
                .map(|v| v.to_str().unwrap().to_string()),
            response.headers().contains_key("x-mimic-endpoint"),
        );
        response.into_body().collect().await.unwrap();
        (status, error, endpoint, began.elapsed())
    })
}

fn median(mut values: Vec<u64>) -> u64 {
    values.sort_unstable();
    values[values.len() / 2]
}

#[tokio::test]
async fn cbr_25_over_maxrequests_http1_requests_are_answered_breaker_open_at_once() {
    let logs = capture_logs();
    let pod = Pod::http1("slow", 1500).await;
    let state = state_with(&[cart(&[pod.addr]).breaker(4, 1024)], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    let held: Vec<_> = (0..4).map(|_| spawn_get(f.addr, "/")).collect();
    until("four requests at the pod", || {
        pod.counts.in_flight.load(Ordering::SeqCst) == 4
    })
    .await;
    let mut kept = H1Client::connect(f.addr).await;
    let mut client_ms = Vec::new();
    for n in 0..6 {
        let began = Instant::now();
        let (head, body) = if n == 0 {
            kept.get("/").await
        } else {
            let (head, body, _) = spawn_get(f.addr, "/").await.unwrap();
            (head, String::from_utf8(body).unwrap())
        };
        client_ms.push(began.elapsed().as_millis() as u64);
        assert_open(&head, body.as_bytes());
    }
    // The keep-alive connection that was refused is usable: it is refused again.
    let (head, body) = kept.get("/").await;
    assert_open(&head, body.as_bytes());
    assert_eq!(pod.counts.requests(), 4, "no pod receives the refused ones");
    assert!(median(client_ms) <= 5, "answered at once");
    let durations: Vec<u64> = reasoned(&logs, "breaker-open")
        .await
        .iter()
        .filter_map(|l| l["duration_ms"].as_u64())
        .collect();
    assert_eq!(durations.len(), 7, "{}", logs.text());
    assert!(median(durations) <= 1, "the sidecar's own measure");
    for job in held {
        assert_eq!(code(job).await, 200);
    }
    // Once the four ended the next request is sent.
    let (head, _) = kept.get("/").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(pod.counts.requests(), 5);
}

#[tokio::test]
async fn cbr_25_over_maxrequests_h2c_streams_are_answered_breaker_open_at_once() {
    let logs = capture_logs();
    let pod = Pod::h2c("slow", 1500, None).await;
    let state = state_with(&[cart(&[pod.addr]).breaker(4, 1024)], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    let sender = h2_client(f.addr).await;
    let held: Vec<_> = (0..4).map(|_| h2_get(&sender)).collect();
    until("four requests at the pod", || {
        pod.counts.in_flight.load(Ordering::SeqCst) == 4
    })
    .await;
    let mut client_ms = Vec::new();
    for _ in 0..6 {
        let (status, error, endpoint, took) = h2_get(&sender).await.unwrap();
        assert_eq!(
            (status, error.as_deref(), endpoint),
            (503, Some("breaker-open"), false)
        );
        client_ms.push(took.as_millis() as u64);
    }
    assert_eq!(pod.counts.requests(), 4);
    assert!(median(client_ms) <= 5, "answered at once");
    let durations: Vec<u64> = reasoned(&logs, "breaker-open")
        .await
        .iter()
        .filter_map(|l| l["duration_ms"].as_u64())
        .collect();
    assert_eq!(durations.len(), 6, "{}", logs.text());
    assert!(median(durations) <= 1);
    for job in held {
        assert_eq!(job.await.unwrap().0, 200);
    }
    // The connection is still usable, and a slot is free again.
    assert_eq!(h2_get(&sender).await.unwrap().0, 200);
}

/// A pod that answers with a body of 10 bytes, 5 at once and 5 after `pause`.
async fn streaming_pod(pause: Duration) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let mut pending = Vec::new();
                loop {
                    let Ok(n) = stream.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    pending.extend_from_slice(&buf[..n]);
                    while let Some(p) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                        pending.drain(..p + 4);
                        let first = b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\n12345";
                        if stream.write_all(first).await.is_err() {
                            return;
                        }
                        tokio::time::sleep(pause).await;
                        if stream.write_all(b"67890").await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    addr
}

/// Waits until a request is not answered `breaker-open` any more (the slot is given back when
/// the response body is dropped, a moment after its last byte).
async fn free_again(addr: SocketAddr) {
    for _ in 0..100 {
        let (head, ..) = spawn_get(addr, "/").await.unwrap();
        if !head.contains("breaker-open") {
            assert!(head.starts_with("HTTP/1.1 200"), "{head}");
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the slot was never given back");
}

#[tokio::test]
async fn cbr_26_a_long_response_holds_its_slot_until_its_last_byte() {
    let _logs = capture_logs();
    let pod = streaming_pod(Duration::from_millis(1200)).await;
    let state = state_with(&[cart(&[pod]).breaker(1, 1024)], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    let long = spawn_get(f.addr, "/");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (head, body, _) = spawn_get(f.addr, "/").await.unwrap();
    assert_open(&head, &body);
    assert_eq!(code(long).await, 200);
    free_again(f.addr).await;
}

#[tokio::test]
async fn cbr_26_retries_and_the_fallback_to_the_cluster_ip_hold_one_slot_each() {
    let _logs = capture_logs();
    let ok = Pod::http1("ok", 0).await;
    let state = state_with(
        &[cart(&[closed_port().await, closed_port().await, ok.addr]).breaker(1, 1024)],
        |_| {},
    );
    let f = fixture_mesh(dst(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for n in 0..20 {
        let (head, _) = client.get("/").await;
        assert!(head.starts_with("HTTP/1.1 200"), "request {n}: {head}");
    }
    let cluster = Pod::http1("cluster", 0).await;
    let state = state_with(
        &[svc(
            "shop/cart",
            Some("127.0.0.1"),
            cluster.addr.port(),
            &[closed_port().await, closed_port().await],
        )
        .breaker(1, 1024)],
        |_| {},
    );
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for n in 0..10 {
        let (head, body) = client.get("/").await;
        assert!(head.starts_with("HTTP/1.1 200"), "request {n}: {head}");
        assert_eq!(body, "cluster");
    }
}

#[tokio::test]
async fn cbr_26_a_client_that_leaves_and_a_route_timeout_free_the_slot_at_once() {
    let _logs = capture_logs();
    let pod = Pod::http1("slow", 3000).await;
    let state = state_with(&[cart(&[pod.addr]).breaker(1, 1024)], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    // The app leaves with its body half sent.
    let mut leaving = TcpStream::connect(f.addr).await.unwrap();
    leaving
        .write_all(b"POST / HTTP/1.1\r\nHost: cart.shop.svc:80\r\ncontent-length: 10\r\n\r\nabc")
        .await
        .unwrap();
    until("the pod has the request", || pod.counts.requests() == 1).await;
    pod.counts.delay_ms.store(0, Ordering::SeqCst);
    drop(leaving);
    free_again(f.addr).await;
    // A route timeout.
    let pod = Pod::http1("slow", 3000).await;
    let timed = r#"{"name":"shop/timed","rules":[{"backends":[{"port":80,"service":"shop/cart","weight":1}],"matches":[{"path":{"type":"PathPrefix","value":"/"}}],"timeouts":{"backendRequestMs":100}}]}"#;
    let state = state_with(&[cart(&[pod.addr]).breaker(1, 1024).routes(timed)], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    for _ in 0..3 {
        let (head, ..) = spawn_get(f.addr, "/").await.unwrap();
        assert!(head.starts_with("HTTP/1.1 504"), "{head}");
    }
}

#[tokio::test]
async fn cbr_26_mirror_copies_l4_connections_and_stale_views_hold_no_slot() {
    let _logs = capture_logs();
    let primary = Pod::http1("primary", 0).await;
    let mirror = Pod::http1("mirror", 2000).await;
    let route = r#"{"name":"shop/mirror","rules":[{"backends":[{"port":80,"service":"shop/cart-v1","weight":1}],"filters":{"mirrors":[{"denominator":1,"numerator":1,"port":80,"service":"shop/mirror"}]},"matches":[{"path":{"type":"PathPrefix","value":"/"}}]}]}"#;
    let state = state_with(
        &[
            cart(&[]).routes(route),
            svc("shop/cart-v1", None, 80, &[primary.addr]).breaker(1, 1024),
            svc("shop/mirror", None, 80, &[mirror.addr]).breaker(1, 1024),
        ],
        |_| {},
    );
    let f = fixture_mesh(dst(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for n in 0..50 {
        let (head, _) = client.get("/").await;
        assert!(head.starts_with("HTTP/1.1 200"), "request {n}: {head}");
    }
    until("every copy reached the mirror", || {
        mirror.counts.requests() == 50
    })
    .await;
    // Five concurrent L4 connections are all relayed.
    let echo = EchoPod::start().await;
    let state = state_with(&[cart(&[echo.addr]).breaker(1, 1024)], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    let mut held = Vec::new();
    for _ in 0..5 {
        let mut c = TcpStream::connect(f.addr).await.unwrap();
        c.write_all(b"\x00\x01binary").await.unwrap();
        let mut got = [0u8; 8];
        c.read_exact(&mut got).await.unwrap();
        held.push(c);
    }
    // A stale view sends requests to the original destination: no slot.
    let cluster = Pod::http1("cluster", 300).await;
    let state = state_with(
        &[svc("shop/cart", Some("127.0.0.1"), cluster.addr.port(), &[]).breaker(1, 1024)],
        |c| c.stale_after = Duration::from_millis(1),
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let jobs: Vec<_> = (0..3).map(|_| spawn_get(f.addr, "/")).collect();
    for job in jobs {
        assert_eq!(code(job).await, 200);
    }
}

#[tokio::test]
async fn cbr_26_a_request_the_mock_answers_takes_no_slot_and_is_never_breaker_open() {
    let _logs = capture_logs();
    let pod = Pod::http1("slow", 1500).await;
    let state = state_with(&[cart(&[pod.addr]).breaker(1, 1024)], |_| {});
    let rig = rig_with(
        users_set(5),
        100,
        1024,
        None,
        Options {
            mode: Some(Mode::Discovery),
            dst: Some(dst()),
            mesh: Some(state.clone()),
            ..Options::default()
        },
    )
    .await;
    let request = "GET /users/1 HTTP/1.1\r\nhost: users.test:8080\r\n\r\n";
    let held = tokio::spawn({
        let addr = rig.addr;
        async move {
            let mut client = H1Client::connect(addr).await;
            client.send(request.as_bytes()).await;
            client.read().await.expect("an answer")
        }
    });
    until("the slot is held", || {
        pod.counts.in_flight.load(Ordering::SeqCst) == 1
    })
    .await;
    // The breaker is full: a balanced request is refused.
    let mut client = H1Client::connect(rig.addr).await;
    client.send(request.as_bytes()).await;
    let (head, _) = client.read().await.expect("an answer");
    assert!(head.contains("breaker-open"), "full: {head}");
    // The mock answers the next ones, whatever the slots.
    rig.mode
        .observe(Announced {
            mode: Ok(Mode::Mock),
            factor: None,
        })
        .await;
    for n in 0..5 {
        client.send(request.as_bytes()).await;
        let (head, _) = client.read().await.expect("an answer");
        assert!(
            head.starts_with("HTTP/1.1 200") && lower(&head).contains("x-mimic-match"),
            "mocked request {n}: {head}"
        );
    }
    let scrape = rig.metrics.encode();
    assert_eq!(
        metric(
            &scrape,
            "mimic_mesh_requests_total{",
            &["reason=\"breaker-open\""]
        ),
        1.0,
        "only the balanced request: {scrape}"
    );
    let (head, _) = held.await.unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
}

/// A pod that answers `101` to an `Upgrade`, `200` to a `CONNECT`, a response whose body never
/// ends to `/hold` and `200 ok` to anything else.
async fn tunnel_pod() -> RawPod {
    RawPod::start(|head| {
        let head = String::from_utf8_lossy(head).to_ascii_lowercase();
        if head.starts_with("connect") {
            b"HTTP/1.1 200 Connection established\r\n\r\n".to_vec()
        } else if head.contains("upgrade: websocket") {
            b"HTTP/1.1 101 Switching Protocols\r\nconnection: upgrade\r\nupgrade: websocket\r\n\r\n"
                .to_vec()
        } else if head.starts_with("get /hold") {
            b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\n12345".to_vec()
        } else {
            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok".to_vec()
        }
    })
    .await
}

#[tokio::test]
async fn cbr_26_an_upgrade_and_a_connect_with_a_full_breaker_reach_the_pod_and_hold_no_slot() {
    let _logs = capture_logs();
    let pod = tunnel_pod().await;
    let state = state_with(&[cart(&[pod.addr]).breaker(1, 1024)], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    // A response that never ends holds the only slot.
    let mut holder = H1Client::connect(f.addr).await;
    holder
        .send(b"GET /hold HTTP/1.1\r\nHost: cart.shop.svc:80\r\n\r\n")
        .await;
    let head = holder
        .read_raw(b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\n12345".len())
        .await;
    assert!(head.starts_with(b"HTTP/1.1 200"), "{head:?}");
    let mut tunnels = Vec::new();
    for (opening, answer) in [
        (
            "GET /ws HTTP/1.1\r\nHost: cart.shop.svc:80\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
            "HTTP/1.1 101",
        ),
        (
            "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n",
            "HTTP/1.1 200",
        ),
    ] {
        let mut client = H1Client::connect(f.addr).await;
        // A request first (the first request of a connection with an `Upgrade` keeps the byte
        // relay): the breaker is full.
        let (head, body) = client.get("/").await;
        assert_open(&head, body.as_bytes());
        client.send(opening.as_bytes()).await;
        let (head, _) = client.read().await.expect("an answer");
        assert!(head.starts_with(answer), "{opening}: {head}");
        tunnels.push(client);
    }
    let scrape = f.metrics.encode();
    assert_eq!(
        metric(
            &scrape,
            "mimic_mesh_requests_total{",
            &["reason=\"breaker-open\""]
        ),
        2.0,
        "the two probes only: {scrape}"
    );
    // The tunnels hold no slot: once the long response ends, a request is served.
    drop(holder);
    free_again(f.addr).await;
    drop(tunnels);
}

#[tokio::test]
async fn cbr_27_the_ports_of_a_service_share_one_count_and_two_services_do_not() {
    let _logs = capture_logs();
    let (a, b) = (Pod::http1("a", 800).await, Pod::http1("b", 800).await);
    let mut shared = cart(&[a.addr]).breaker(1, 1024);
    shared.ports.push((81, vec![b.addr]));
    let state = state_with(&[shared], |_| {});
    let on80 = fixture_mesh(dst(), &state, None).await;
    let on81 = fixture_mesh("127.0.0.1:81".parse().unwrap(), &state, None).await;
    let first = spawn_get(on80.addr, "/");
    until("the first request is in", || {
        a.counts.in_flight.load(Ordering::SeqCst) == 1
    })
    .await;
    let (head, body, _) = spawn_get(on81.addr, "/").await.unwrap();
    assert_open(&head, &body);
    assert_eq!(code(first).await, 200);
    // Two Services: one request each is in flight, neither is refused.
    let (c, d) = (Pod::http1("c", 500).await, Pod::http1("d", 500).await);
    let other = svc("shop/other", Some("127.0.0.1"), 81, &[d.addr]).breaker(1, 1024);
    let state = state_with(&[cart(&[c.addr]).breaker(1, 1024), other], |_| {});
    let on80 = fixture_mesh(dst(), &state, None).await;
    let on81 = fixture_mesh("127.0.0.1:81".parse().unwrap(), &state, None).await;
    let (x, y) = (spawn_get(on80.addr, "/"), spawn_get(on81.addr, "/"));
    assert_eq!((code(x).await, code(y).await), (200, 200));
}

#[tokio::test]
async fn cbr_27_a_routed_request_counts_on_its_drawn_backends_service() {
    let _logs = capture_logs();
    let v1 = Pod::http1("v1", 800).await;
    let route = r#"{"name":"shop/split","rules":[{"backends":[{"port":80,"service":"shop/cart-v1","weight":1}],"matches":[{"path":{"type":"PathPrefix","value":"/"}}]}]}"#;
    let state = state_with(
        &[
            cart(&[]).routes(route),
            svc("shop/cart-v1", None, 80, &[v1.addr]).breaker(1, 1024),
        ],
        |_| {},
    );
    let f = fixture_mesh(dst(), &state, None).await;
    let first = spawn_get(f.addr, "/");
    until("the first request is in", || {
        v1.counts.in_flight.load(Ordering::SeqCst) == 1
    })
    .await;
    let (head, body, _) = spawn_get(f.addr, "/").await.unwrap();
    assert_open(&head, &body);
    assert_eq!(code(first).await, 200);
}

#[tokio::test]
async fn cbr_28_an_open_breaker_adds_no_try_and_is_counted_and_logged_once_a_second() {
    let logs = capture_logs();
    let pod = Pod::http1("slow", 2500).await;
    let route = r#"{"name":"shop/r","rules":[{"backends":[{"port":80,"service":"shop/cart","weight":1}],"filters":{"responseHeaders":{"add":[{"name":"x-added","value":"1"}]}},"matches":[{"path":{"type":"PathPrefix","value":"/"}}]}]}"#;
    let state = state_with(&[cart(&[pod.addr]).breaker(1, 1024).routes(route)], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    let held = spawn_get(f.addr, "/");
    until("one request in", || {
        pod.counts.in_flight.load(Ordering::SeqCst) == 1
    })
    .await;
    let attempts =
        |f: &common::Fixture| metric(&f.metrics.encode(), "mimic_mesh_attempts_total{", &[]);
    let before = attempts(&f);
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..30 {
        let (head, body) = client.get("/").await;
        assert_open(&head, body.as_bytes());
        assert!(
            !lower(&head).contains("x-added"),
            "no response modifier: {head}"
        );
    }
    assert_eq!(attempts(&f), before, "no try");
    let scrape = f.metrics.encode();
    assert_eq!(
        metric(
            &scrape,
            "mimic_mesh_requests_total{",
            &[
                "reason=\"breaker-open\"",
                "status_class=\"5xx\"",
                "backend=\"shop/cart:80\""
            ],
        ),
        30.0,
        "{scrape}"
    );
    let warned = lines(&logs, "breaker open").await;
    assert_eq!(warned.len(), 1, "{}", logs.text());
    assert_eq!(warned[0]["level"], "WARN");
    assert_eq!(warned[0]["cause"], "max_requests");
    assert_eq!(warned[0]["in_flight"], 1);
    assert_eq!(warned[0]["max_requests"], 1);
    assert_eq!(warned[0]["service"], "shop/cart");
    assert_eq!(warned[0]["backend"], "shop/cart:80");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let (head, body) = client.get("/").await;
    assert_open(&head, body.as_bytes());
    let warned = lines(&logs, "breaker open").await;
    assert_eq!(warned.len(), 2, "{}", logs.text());
    assert_eq!(warned[1]["suppressed"], 29);
    // The access line: a reason, the backend and the rule, no pod and no tries.
    let open = reasoned(&logs, "breaker-open").await;
    assert_eq!(open.len(), 31);
    for line in &open {
        assert_eq!(line["backend"], "shop/cart:80");
        assert_eq!(line["route"], "shop/r");
        assert_eq!(line["rule"], 0);
        assert!(
            line.get("upstream").is_none() && line.get("attempts").is_none(),
            "a breaker-open line names no pod and no tries"
        );
    }
    assert_eq!(code(held).await, 200);
}

#[tokio::test]
async fn cbr_28_a_breaker_open_is_counted_as_a_synthetic_exchange_of_discovery() {
    let _logs = capture_logs();
    let pod = Pod::http1("slow", 1500).await;
    let state = state_with(&[cart(&[pod.addr]).breaker(1, 1024)], |_| {});
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let f = fixture_mesh(dst(), &state, Some(&discovery)).await;
    let held = spawn_get(f.addr, "/");
    until("one request in", || {
        pod.counts.in_flight.load(Ordering::SeqCst) == 1
    })
    .await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..3 {
        let (head, body) = client.get("/").await;
        assert_open(&head, body.as_bytes());
    }
    let scrape = f.metrics.encode();
    assert!(
        scrape.contains(
            "mimic_discovery_exchanges_total{direction=\"outbound\",result=\"synthetic\"} 3"
        ),
        "{scrape}"
    );
    assert_eq!(code(held).await, 200);
}

#[tokio::test]
async fn cbr_28_a_breaker_open_answered_by_an_app_is_an_app_503_never_retried() {
    let _logs = capture_logs();
    let pod = RawPod::start(|_| {
        b"HTTP/1.1 503 Service Unavailable\r\nx-mimic-error: breaker-open\r\ncontent-length: 0\r\n\r\n"
            .to_vec()
    })
    .await;
    let other = RawPod::start(|_| {
        b"HTTP/1.1 503 Service Unavailable\r\nx-mimic-error: breaker-open\r\ncontent-length: 0\r\n\r\n"
            .to_vec()
    })
    .await;
    let state = state_with(&[cart(&[pod.addr, other.addr])], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..10 {
        let (head, _) = client.get("/").await;
        assert!(
            lower(&head).contains("x-mimic-error: breaker-open"),
            "{head}"
        );
    }
    let heads = |p: &RawPod| {
        String::from_utf8_lossy(&p.received())
            .matches("GET / HTTP/1.1")
            .count()
    };
    assert_eq!(
        heads(&pod) + heads(&other),
        10,
        "one try each, never retried"
    );
}

#[tokio::test]
async fn cbr_29_a_new_maxrequests_applies_from_the_next_request_and_a_returning_service_counts_from_0()
 {
    let _logs = capture_logs();
    let pod = Pod::http1("slow", 900).await;
    let state = state_with(&[cart(&[pod.addr]).breaker(8, 1024)], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    let held: Vec<_> = (0..8).map(|_| spawn_get(f.addr, "/")).collect();
    until("eight in flight", || {
        pod.counts.in_flight.load(Ordering::SeqCst) == 8
    })
    .await;
    state.hold(view(&[cart(&[pod.addr]).breaker(2, 1024)]), "\"2-a\"");
    let (head, body, _) = spawn_get(f.addr, "/").await.unwrap();
    assert_open(&head, &body);
    for job in held {
        assert_eq!(code(job).await, 200, "the requests in flight finish");
    }
    pod.counts.delay_ms.store(600, Ordering::SeqCst);
    let two: Vec<_> = (0..2).map(|_| spawn_get(f.addr, "/")).collect();
    until("two in flight", || {
        pod.counts.in_flight.load(Ordering::SeqCst) == 2
    })
    .await;
    let (head, body, _) = spawn_get(f.addr, "/").await.unwrap();
    assert_open(&head, &body);
    // A higher value applies from the next request.
    state.hold(view(&[cart(&[pod.addr]).breaker(3, 1024)]), "\"3-a\"");
    let third = spawn_get(f.addr, "/");
    for job in two {
        assert_eq!(code(job).await, 200);
    }
    assert_eq!(code(third).await, 200);
    // A Service that leaves and comes back counts from 0, whatever was in flight.
    let one = spawn_get(f.addr, "/");
    until("one in flight", || {
        pod.counts.in_flight.load(Ordering::SeqCst) == 1
    })
    .await;
    state.hold(
        view(&[svc("shop/other", Some("127.0.0.2"), 80, &[])]),
        "\"4-a\"",
    );
    state.hold(view(&[cart(&[pod.addr]).breaker(1, 1024)]), "\"5-a\"");
    assert_eq!(code(spawn_get(f.addr, "/")).await, 200, "counted from 0");
    assert_eq!(code(one).await, 200);
}

#[tokio::test]
async fn cbr_30_the_answers_of_the_sidecar_carry_their_reason_and_a_pod_answer_none() {
    let logs = capture_logs();
    let ok = Pod::http1("ok", 0).await;
    let slow = Pod::http1("slow", 3000).await;
    let refusing = RawPod::start(|_| {
        b"HTTP/1.1 503 Service Unavailable\r\nx-mimic-error: overloaded\r\nx-mimic-endpoint: 127.0.0.1:1\r\ncontent-length: 0\r\n\r\n".to_vec()
    })
    .await;
    let rules = |path: &str, backends: &str, timeouts: &str| {
        format!(
            r#"{{"backends":[{backends}],"matches":[{{"path":{{"type":"Exact","value":"{path}"}}}}]{timeouts}}}"#
        )
    };
    let to = |service: &str, weight: u32| {
        format!(r#"{{"port":80,"service":"{service}","weight":{weight}}}"#)
    };
    let routes = format!(
        r#"{{"name":"shop/r","rules":[{},{},{},{}]}}"#,
        rules("/zero", &to("shop/ok", 0), ""),
        rules("/invalid", r#"{"invalid":true,"weight":1}"#, ""),
        rules(
            "/slow",
            &to("shop/slow", 1),
            r#","timeouts":{"backendRequestMs":100}"#
        ),
        rules("/ok", &to("shop/ok", 1), ""),
    );
    let state = state_with(
        &[
            cart(&[]).routes(&routes),
            svc("shop/ok", None, 80, &[ok.addr]),
            svc("shop/slow", None, 80, &[slow.addr]),
        ],
        |_| {},
    );
    let f = fixture_mesh(dst(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for path in ["/zero", "/invalid", "/slow", "/ok", "/other"] {
        client.get(path).await;
    }
    // A Service with no endpoint, and one whose pod refuses to connect.
    let empty = state_with(&[cart(&[])], |_| {});
    let g = fixture_mesh(dst(), &empty, None).await;
    H1Client::connect(g.addr).await.get("/").await;
    let closed = state_with(&[cart(&[closed_port().await])], |_| {});
    let h = fixture_mesh(dst(), &closed, None).await;
    H1Client::connect(h.addr).await.get("/").await;
    // A pod's own answer and a trusted refusal passed as received: none.
    let busy = state_with(&[cart(&[refusing.addr])], |_| {});
    let i = fixture_mesh(dst(), &busy, None).await;
    H1Client::connect(i.addr).await.get("/").await;
    logs.settled().await;
    let access = logs.access();
    assert_eq!(access.len(), 8, "{}", logs.text());
    let reasons: Vec<Option<&str>> = access.iter().map(|l| l["reason"].as_str()).collect();
    assert_eq!(
        reasons,
        [
            Some("no-backend"),
            Some("invalid-backend"),
            Some("timeout"),
            None,
            Some("no-route"),
            Some("no-endpoint"),
            Some("connect-failed"),
            None,
        ],
        "{}",
        logs.text()
    );
}

#[tokio::test]
async fn cbr_31_maxconnections_bounds_the_http1_connections_of_a_pool() {
    let _logs = capture_logs();
    let pod = Pod::http1("slow", 300).await;
    let state = state_with(&[cart(&[pod.addr]).breaker(1024, 2)], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    let jobs: Vec<_> = (0..6).map(|_| spawn_get(f.addr, "/")).collect();
    for job in jobs {
        assert_eq!(code(job).await, 200);
    }
    assert_eq!(
        pod.counts.accepted(),
        2,
        "two connections served six requests"
    );
}

#[tokio::test]
async fn cbr_31_h2c_keeps_its_sixteen_connections_whatever_maxconnections() {
    let _logs = capture_logs();
    let pod = Pod::h2c("slow", 400, Some(1)).await;
    let state = state_with(&[cart(&[pod.addr]).breaker(1024, 2)], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    let sender = h2_client(f.addr).await;
    // One request first, so that the sidecar knows the pod's stream limit of 1.
    assert_eq!(h2_get(&sender).await.unwrap().0, 200);
    // Spaced, so that each new connection announced its limit before the next stream.
    let mut jobs = Vec::new();
    for _ in 0..6 {
        jobs.push(h2_get(&sender));
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    for job in jobs {
        assert_eq!(job.await.unwrap().0, 200);
    }
    let accepted = pod.counts.accepted();
    assert!((3..=16).contains(&accepted), "{accepted} connections");
}

#[tokio::test]
async fn cbr_31_mirror_pools_follow_the_mirror_service_and_a_pod_follows_the_requests_own() {
    let _logs = capture_logs();
    let primary = Pod::http1("primary", 400).await;
    let mirror = Pod::http1("mirror", 400).await;
    let route = r#"{"name":"shop/mirror","rules":[{"backends":[{"port":80,"service":"shop/cart-v1","weight":1}],"filters":{"mirrors":[{"denominator":1,"numerator":1,"port":80,"service":"shop/mirror"}]},"matches":[{"path":{"type":"PathPrefix","value":"/"}}]}]}"#;
    let state = state_with(
        &[
            cart(&[]).routes(route),
            svc("shop/cart-v1", None, 80, &[primary.addr]).breaker(1024, 8),
            svc("shop/mirror", None, 80, &[mirror.addr]).breaker(1024, 1),
        ],
        |_| {},
    );
    let f = fixture_mesh(dst(), &state, None).await;
    let jobs: Vec<_> = (0..5).map(|_| spawn_get(f.addr, "/")).collect();
    for job in jobs {
        assert_eq!(code(job).await, 200);
    }
    until("every copy was answered", || mirror.counts.requests() == 5).await;
    assert_eq!(
        mirror.counts.accepted(),
        1,
        "the mirror Service's own bound"
    );
    assert!(primary.counts.accepted() > 1, "not bound by the mirror's");
    // One pod in two Services, with 2 and 4: each request follows its own.
    let pod = Pod::http1("shared", 300).await;
    let state = state_with(
        &[
            svc("shop/a", Some("127.0.0.1"), 80, &[pod.addr]).breaker(1024, 2),
            svc("shop/b", Some("127.0.0.1"), 81, &[pod.addr]).breaker(1024, 4),
        ],
        |_| {},
    );
    let on_a = fixture_mesh(dst(), &state, None).await;
    let on_b = fixture_mesh("127.0.0.1:81".parse().unwrap(), &state, None).await;
    let jobs: Vec<_> = (0..8).map(|_| spawn_get(on_b.addr, "/")).collect();
    for job in jobs {
        assert_eq!(code(job).await, 200);
    }
    assert_eq!(
        pod.counts.accepted(),
        4,
        "four for the Service that allows four"
    );
    // The two that `shop/a` allows: it never opens more while four are open.
    let jobs: Vec<_> = (0..8).map(|_| spawn_get(on_a.addr, "/")).collect();
    for job in jobs {
        assert_eq!(code(job).await, 200);
    }
    assert_eq!(pod.counts.accepted(), 4);
}

#[tokio::test]
async fn cbr_31_lowering_maxconnections_closes_no_busy_connection_and_idle_ones_over_it_close() {
    let _logs = capture_logs();
    let pod = Pod::http1("slow", 600).await;
    let state = state_with(&[cart(&[pod.addr]).breaker(1024, 4)], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    let jobs: Vec<_> = (0..4).map(|_| spawn_get(f.addr, "/")).collect();
    until("four in flight", || {
        pod.counts.in_flight.load(Ordering::SeqCst) == 4
    })
    .await;
    state.hold(view(&[cart(&[pod.addr]).breaker(1024, 2)]), "\"2-a\"");
    for job in jobs {
        assert_eq!(code(job).await, 200, "no busy connection was closed");
    }
    until("at most two stay open", || pod.counts.open() <= 2).await;
    assert!(pod.counts.open() >= 1 && pod.counts.accepted() == 4);
}

#[tokio::test]
async fn cbr_18_breaker_open_answers_count_on_no_pod() {
    let logs = capture_logs();
    let pod = Pod::http1("slow", 1500).await;
    let state = state_with(
        &[cart(&[pod.addr]).breaker(1, 1024).outlier(30, 1, 1, 100)],
        |_| {},
    );
    let f = fixture_mesh(dst(), &state, None).await;
    let held = spawn_get(f.addr, "/");
    until("one request in", || {
        pod.counts.in_flight.load(Ordering::SeqCst) == 1
    })
    .await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..10 {
        let (head, body) = client.get("/").await;
        assert_open(&head, body.as_bytes());
    }
    assert_eq!(code(held).await, 200);
    assert!(!f.metrics.encode().contains("mimic_mesh_ejections_total{"));
    assert!(lines(&logs, "endpoint ejected").await.is_empty());
}
