//! RTE-63, RTE-67, RTE-68, RTE-70: the route timeouts of the sidecar on a single try before the
//! response head, the connection of a cut try that is never reused, where no timeout applies and
//! the view a request is routed with (spec §7.9 "Timeouts", §12.25).

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::mesh_pods::{H1Client, Pod, cluster_ip};
use common::{capture_logs, fixture_mesh};
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::mesh::view::{Limits, View};
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
use mimic_sidecar::mesh::{MeshConfig, MeshState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// An HTTP/1 server that reads and never answers: it keeps what it received and counts the
/// connections it accepted and those still open.
struct SilentPod {
    addr: SocketAddr,
    received: Arc<std::sync::Mutex<Vec<u8>>>,
    open: Arc<AtomicUsize>,
}

impl SilentPod {
    async fn start() -> SilentPod {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let open = Arc::new(AtomicUsize::new(0));
        let (log, o) = (received.clone(), open.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                o.fetch_add(1, Ordering::SeqCst);
                let (log, o) = (log.clone(), o.clone());
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = stream.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        log.lock().unwrap().extend_from_slice(&buf[..n]);
                    }
                    o.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        SilentPod {
            addr,
            received,
            open,
        }
    }

    /// The request heads it received.
    fn requests(&self) -> usize {
        let received = self.received.lock().unwrap();
        received
            .windows(10)
            .filter(|w| w == b"HTTP/1.1\r\n")
            .count()
    }

    /// The bytes of the bodies it received (what follows the heads).
    fn body_bytes(&self) -> usize {
        let received = self.received.lock().unwrap();
        received
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map_or(0, |p| received.len() - (p + 4))
    }
}

/// A rule of the route `shop/timed`: the path prefix it matches, the Service it sends to and
/// its `timeouts` member (a JSON object) when it has one.
fn rule(path: &str, service: &str, timeouts: Option<&str>) -> String {
    rule_on(path, (service, 80), timeouts)
}

/// Like [`rule`], to port `port` of the Service.
fn rule_on(path: &str, (service, port): (&str, u16), timeouts: Option<&str>) -> String {
    let timeouts = timeouts.map_or_else(String::new, |t| format!(r#","timeouts":{t}"#));
    format!(
        r#"{{"backends":[{{"port":{port},"service":"{service}","weight":1}}],"matches":[{{"path":{{"type":"PathPrefix","value":"{path}"}}}}]{timeouts}}}"#
    )
}

fn view(rules: &[String], own: SocketAddr, other: &[SocketAddr]) -> View {
    view_on(80, rules, own, other)
}

/// The Service `shop/cart` is on `port`; `shop/other` is headless on 80.
fn view_on(port: u16, rules: &[String], own: SocketAddr, other: &[SocketAddr]) -> View {
    let pod = |p: &SocketAddr| format!(r#"{{"ip":"{}","port":{},"ready":true}}"#, p.ip(), p.port());
    let policy = r#""policy":{"loadBalancer":{"slowStartSeconds":0},"retries":3,"retryBudget":{"minPerSecond":10,"percent":20,"windowSeconds":10}}"#;
    let text = format!(
        r#"{{"cluster":"local","format":1,"generations":{{}},"mesh":true,"mode":"none","services":{{"shop/cart":{{"endpoints":{{"{port}":[{own}]}},"generations":{{}},"ips":["127.0.0.1"],{policy},"ports":{{"{port}":{{"targetPort":8080}}}},"routes":[{{"name":"shop/timed","rules":[{rules}]}}]}},"shop/other":{{"endpoints":{{"80":[{other}]}},"generations":{{}},"ips":[],{policy},"ports":{{"80":{{"targetPort":8080}}}}}}}}}}"#,
        own = pod(&own),
        other = other.iter().map(pod).collect::<Vec<_>>().join(","),
        rules = rules.join(","),
    );
    View::from_json(&text, &Limits::default()).unwrap()
}

fn state_with(view: View) -> MeshState {
    state_configured(view, |_| {})
}

fn state_configured(view: View, configure: impl FnOnce(&mut MeshConfig)) -> MeshState {
    let mut config = MeshConfig {
        enabled: true,
        seed: Some(7),
        ..MeshConfig::default()
    };
    configure(&mut config);
    let state = MeshState::new(&config);
    state.hold(view, "\"1-a\"");
    state
}

fn metric(metrics: &str, name: &str, labels: &[&str]) -> f64 {
    metrics
        .lines()
        .filter(|l| l.starts_with(name) && labels.iter().all(|f| l.contains(f)))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
}

fn lower(head: &str) -> String {
    head.to_lowercase()
}

/// One request over `h2c` to `addr`: the status, the fields and the time it took.
async fn h2c_call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: &'static [u8],
) -> (StatusCode, hyper::HeaderMap, Duration) {
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, Full<Bytes>>(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let request = Request::builder()
        .method(method)
        .uri(format!("http://cart.shop.svc{path}"))
        .body(Full::new(Bytes::from_static(body)))
        .unwrap();
    let began = Instant::now();
    let response = sender.send_request(request).await.expect("an answer");
    let elapsed = began.elapsed();
    let (parts, body) = response.into_parts();
    assert!(body.collect().await.unwrap().to_bytes().is_empty());
    (parts.status, parts.headers, elapsed)
}

const TIMED: &str = r#"{"requestMs":200}"#;

#[tokio::test]
async fn rte_63_http1_get_and_post_to_a_silent_pod_answer_504_and_the_connection_is_kept() {
    let logs = capture_logs();
    let (own, silent) = (Pod::http1("own", 0).await, SilentPod::start().await);
    let state = state_with(view(
        &[
            rule("/slow", "shop/other", Some(TIMED)),
            rule("/", "shop/cart", None),
        ],
        own.addr,
        &[silent.addr],
    ));
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let f = fixture_mesh(cluster_ip(), &state, Some(&discovery)).await;
    let mut client = H1Client::connect(f.addr).await;
    for (i, request) in [
        "GET /slow HTTP/1.1\r\nHost: cart.shop.svc:80\r\n\r\n".to_string(),
        format!(
            "POST /slow HTTP/1.1\r\nHost: cart.shop.svc:80\r\ncontent-length: 1024\r\n\r\n{}",
            "x".repeat(1024)
        ),
    ]
    .iter()
    .enumerate()
    {
        let began = Instant::now();
        client.send(request.as_bytes()).await;
        let (head, body) = client.read().await.expect("the 504");
        let took = began.elapsed();
        assert!(head.starts_with("HTTP/1.1 504"), "{head}");
        assert!(
            took >= Duration::from_millis(200) && took < Duration::from_millis(250),
            "answered after {took:?}"
        );
        let head = lower(&head);
        assert_eq!(head.matches("x-mimic-error").count(), 1, "{head}");
        assert!(head.contains("x-mimic-error: timeout"), "{head}");
        assert!(
            head.contains("content-length: 0") && body.is_empty(),
            "{head}"
        );
        assert!(!head.contains("connection:"), "kept: {head}");
        // The connection is kept: the next request is relayed.
        let (head, body) = client.get("/fast").await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(body, "own", "request {i}");
    }
    let scrape = f.metrics.encode();
    let count = |name: &str, labels: &[&str]| metric(&scrape, name, labels);
    assert_eq!(
        count("mimic_mesh_attempts_total{", &["result=\"timeout\""]),
        2.0
    );
    assert_eq!(
        count(
            "mimic_mesh_requests_total{",
            &["status_class=\"5xx\"", "reason=\"timeout\""]
        ),
        2.0
    );
    assert_eq!(
        count(
            "mimic_mesh_endpoint_requests_total{",
            &["backend=\"shop/other:80\"", "status_class=\"none\""]
        ),
        2.0
    );
    assert_eq!(count("mimic_mesh_retries_total{", &[]), 0.0);
    assert_eq!(count("mimic_mesh_retries_skipped_total{", &[]), 0.0);
    // The two `504` are the sidecar's own answers: counted synthetic, nothing recorded.
    common::counted(
        &f.metrics,
        "mimic_discovery_exchanges_total{direction=\"outbound\",result=\"synthetic\"} 2",
    )
    .await;
    assert!(!discovery.report().to_json().unwrap().contains("\"504\""));
    let lines = logs.access_exactly(4).await;
    for entry in lines.iter().filter(|l| l["status"] == 504) {
        assert_eq!(entry["result"], "timeout", "{entry}");
        assert_eq!(entry["timeout"], "request", "{entry}");
        assert_eq!(entry["attempts"], 1, "{entry}");
        assert_eq!(entry["route"], "shop/timed", "{entry}");
        assert_eq!(entry["rule"], 0, "{entry}");
        assert_eq!(entry["backend"], "shop/other:80", "{entry}");
        assert_eq!(entry["upstream"], silent.addr.to_string(), "{entry}");
    }
    assert!(
        lines
            .iter()
            .filter(|l| l["status"] == 200)
            .all(|l| { l["result"] == "ok" && l.get("timeout").is_none_or(|t| t.is_null()) })
    );
}

#[tokio::test]
async fn rte_63_a_post_cut_by_the_app_is_answered_504_with_connection_close() {
    let _logs = capture_logs();
    let (own, silent) = (Pod::http1("own", 0).await, SilentPod::start().await);
    let state = state_with(view(
        &[rule("/slow", "shop/other", Some(TIMED))],
        own.addr,
        &[silent.addr],
    ));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    // 1 KiB of a declared 10 KiB, and the app stops sending.
    client
        .send(
            format!(
                "POST /slow HTTP/1.1\r\nHost: cart.shop.svc:80\r\ncontent-length: 10240\r\n\r\n{}",
                "x".repeat(1024)
            )
            .as_bytes(),
        )
        .await;
    let (head, _) = client.read().await.expect("the 504");
    assert!(head.starts_with("HTTP/1.1 504"), "{head}");
    assert!(lower(&head).contains("connection: close"), "{head}");
    assert!(client.read().await.is_none(), "the connection is closed");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        silent.body_bytes() < 10240,
        "the pod got a complete body: {}",
        silent.body_bytes()
    );
    assert_eq!(silent.open.load(Ordering::SeqCst), 0, "the pod's side ends");
}

#[tokio::test]
async fn rte_63_h2c_get_and_post_to_a_silent_pod_answer_504() {
    let _logs = capture_logs();
    let (own, silent) = (
        Pod::h2c("own", 0, None).await,
        Pod::h2c("silent", 5000, None).await,
    );
    let state = state_with(view(
        &[rule("/slow", "shop/other", Some(TIMED))],
        own.addr,
        &[silent.addr],
    ));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    for (method, body) in [("GET", &[][..]), ("POST", &[b'x'; 1024][..])] {
        let body: &'static [u8] = Box::leak(body.to_vec().into_boxed_slice());
        let (status, headers, took) = h2c_call(f.addr, method, "/slow", body).await;
        assert_eq!(status, 504, "{method}");
        assert!(
            took >= Duration::from_millis(200) && took < Duration::from_millis(250),
            "{method} answered after {took:?}"
        );
        assert_eq!(headers.get_all("x-mimic-error").iter().count(), 1);
        assert_eq!(headers["x-mimic-error"], "timeout");
        assert_eq!(headers["content-length"], "0");
    }
    let scrape = f.metrics.encode();
    assert_eq!(
        metric(
            &scrape,
            "mimic_mesh_attempts_total{",
            &["result=\"timeout\""]
        ),
        2.0
    );
}

/// A pod whose requests to `/slow` take 200 ms and whose answer names the request (`x-id`).
/// It marks the connection a slow request came on and sees whether another request uses it.
struct EchoPod {
    addr: SocketAddr,
    /// Connections that carried a slow request and are still open.
    tainted_open: Arc<AtomicUsize>,
    /// Requests that arrived on a connection that had carried a slow request.
    reused: Arc<AtomicUsize>,
    accepted: Arc<AtomicUsize>,
}

impl EchoPod {
    async fn start() -> EchoPod {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let pod = EchoPod {
            addr,
            tainted_open: Arc::default(),
            reused: Arc::default(),
            accepted: Arc::default(),
        };
        let (tainted_open, reused, accepted) = (
            pod.tainted_open.clone(),
            pod.reused.clone(),
            pod.accepted.clone(),
        );
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                accepted.fetch_add(1, Ordering::SeqCst);
                let (tainted_open, reused) = (tainted_open.clone(), reused.clone());
                tokio::spawn(async move {
                    let mut tainted = false;
                    let mut pending = Vec::new();
                    let mut buf = [0u8; 4096];
                    'connection: while let Ok(n) = stream.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        pending.extend_from_slice(&buf[..n]);
                        while let Some(p) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&pending[..p]).to_lowercase();
                            pending.drain(..p + 4);
                            if tainted {
                                reused.fetch_add(1, Ordering::SeqCst);
                            }
                            let id = head
                                .lines()
                                .find_map(|l| l.strip_prefix("x-id: "))
                                .unwrap_or("?")
                                .to_string();
                            if head.starts_with("get /slow") {
                                tainted = true;
                                tainted_open.fetch_add(1, Ordering::SeqCst);
                                // The pod notices the connection closing while it waits.
                                let mut one = [0u8; 1];
                                let closed = async {
                                    while matches!(stream.peek(&mut one).await, Ok(n) if n > 0) {
                                        tokio::time::sleep(Duration::from_millis(5)).await;
                                    }
                                };
                                tokio::select! {
                                    () = tokio::time::sleep(Duration::from_millis(200)) => {}
                                    () = closed => break 'connection,
                                }
                            }
                            let answer = format!(
                                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{id}",
                                id.len()
                            );
                            if stream.write_all(answer.as_bytes()).await.is_err() {
                                break;
                            }
                        }
                    }
                    if tainted {
                        tainted_open.fetch_sub(1, Ordering::SeqCst);
                    }
                });
            }
        });
        pod
    }
}

#[tokio::test]
async fn rte_67_http1_a_late_answer_never_answers_another_request_and_the_connection_closes() {
    let _logs = capture_logs();
    let pod = EchoPod::start().await;
    let own = Pod::http1("unused", 0).await;
    let state = state_with(view(
        &[
            rule("/slow", "shop/other", Some(r#"{"backendRequestMs":100}"#)),
            rule("/fast", "shop/other", None),
        ],
        own.addr,
        &[pod.addr],
    ));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    // One keep-alive connection of the app; every second request goes to the fast rule.
    let mut client = H1Client::connect(f.addr).await;
    for i in 0..100 {
        let path = if i % 2 == 0 { "slow" } else { "fast" };
        client
            .send(
                format!("GET /{path} HTTP/1.1\r\nHost: cart.shop.svc:80\r\nx-id: r{i}\r\n\r\n")
                    .as_bytes(),
            )
            .await;
        let (head, body) = client.read().await.expect("an answer");
        if i % 2 == 0 {
            assert!(head.starts_with("HTTP/1.1 504"), "{i}: {head}");
            // The pod sees the connection closed within 50 ms of the `504`.
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(pod.tainted_open.load(Ordering::SeqCst), 0, "request {i}");
        } else {
            assert!(head.starts_with("HTTP/1.1 200"), "{i}: {head}");
            assert_eq!(body, format!("r{i}").as_bytes(), "another request's answer");
        }
    }
    assert_eq!(
        pod.reused.load(Ordering::SeqCst),
        0,
        "a connection that carried a timed-out request was used again"
    );
    assert!(pod.accepted.load(Ordering::SeqCst) >= 50);
}

/// An `h2c` server whose `/slow` streams answer after 200 ms. It counts its connections and
/// keeps the reset reasons it saw.
struct H2Pod {
    addr: SocketAddr,
    accepted: Arc<AtomicUsize>,
    resets: Arc<std::sync::Mutex<Vec<h2::Reason>>>,
}

impl H2Pod {
    async fn start() -> H2Pod {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let pod = H2Pod {
            addr,
            accepted: Arc::default(),
            resets: Arc::default(),
        };
        let (accepted, resets) = (pod.accepted.clone(), pod.resets.clone());
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                accepted.fetch_add(1, Ordering::SeqCst);
                let resets = resets.clone();
                tokio::spawn(async move {
                    let Ok(mut conn) = h2::server::handshake(stream).await else {
                        return;
                    };
                    while let Some(Ok((request, mut respond))) = conn.accept().await {
                        let resets = resets.clone();
                        tokio::spawn(async move {
                            let slow = request.uri().path() == "/slow";
                            let id = request
                                .headers()
                                .get("x-id")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("?")
                                .to_string();
                            if slow {
                                tokio::select! {
                                    reason = std::future::poll_fn(|cx| respond.poll_reset(cx)) => {
                                        if let Ok(reason) = reason {
                                            resets.lock().unwrap().push(reason);
                                        }
                                        return;
                                    }
                                    () = tokio::time::sleep(Duration::from_millis(200)) => {}
                                }
                            }
                            let response = hyper::Response::new(());
                            if let Ok(mut send) = respond.send_response(response, false) {
                                let _ = send.send_data(Bytes::from(id), true);
                            }
                        });
                    }
                });
            }
        });
        pod
    }
}

#[tokio::test]
async fn rte_67_h2c_a_timed_out_stream_is_reset_cancel_and_the_pod_connection_is_reused() {
    let _logs = capture_logs();
    let pod = H2Pod::start().await;
    let own = Pod::http1("unused", 0).await;
    let state = state_with(view(
        &[
            rule("/slow", "shop/other", Some(r#"{"backendRequestMs":100}"#)),
            rule("/fast", "shop/other", None),
        ],
        own.addr,
        &[pod.addr],
    ));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let stream = tokio::net::TcpStream::connect(f.addr).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, Full<Bytes>>(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let mut call = async |path: &str, id: usize| {
        let request = Request::get(format!("http://cart.shop.svc{path}"))
            .header("x-id", format!("r{id}"))
            .body(Full::new(Bytes::new()))
            .unwrap();
        let response = sender.send_request(request).await.expect("an answer");
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    };
    assert_eq!(call("/slow", 0).await.0, 504);
    for i in 1..=10 {
        let (status, body) = call("/fast", i).await;
        assert_eq!(status, 200);
        assert_eq!(body, format!("r{i}"), "each gets its own answer");
    }
    assert_eq!(pod.accepted.load(Ordering::SeqCst), 1, "one pod connection");
    for _ in 0..50 {
        if !pod.resets.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(*pod.resets.lock().unwrap(), vec![h2::Reason::CANCEL]);
}

#[tokio::test]
async fn rte_68_a_zero_a_missing_or_another_rules_timeout_never_cuts_a_slow_pod() {
    let _logs = capture_logs();
    let (own, slow) = (Pod::http1("own", 0).await, Pod::http1("slow", 300).await);
    let state = state_with(view(
        &[
            rule(
                "/zero",
                "shop/other",
                Some(r#"{"requestMs":0,"backendRequestMs":0}"#),
            ),
            rule("/plain", "shop/other", None),
            rule("/limited", "shop/other", Some(r#"{"requestMs":100}"#)),
        ],
        own.addr,
        &[slow.addr],
    ));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for path in ["/zero", "/plain"] {
        let began = Instant::now();
        let (head, body) = client.get(path).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{path}: {head}");
        assert_eq!(body, "slow");
        assert!(began.elapsed() >= Duration::from_millis(300));
    }
    // The other rule's timeout is its own.
    let began = Instant::now();
    let (head, _) = client.get("/limited").await;
    assert!(head.starts_with("HTTP/1.1 504"), "{head}");
    assert!(began.elapsed() < Duration::from_millis(250));
    let (head, _) = client.get("/plain").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
}

#[tokio::test]
async fn rte_68_a_stale_view_mesh_false_and_request_fields_do_not_cut_a_slow_answer() {
    let _logs = capture_logs();
    let (own, slow) = (Pod::http1("own", 0).await, Pod::http1("slow", 300).await);
    let timeouts = Some(r#"{"requestMs":100}"#);
    // The original destination is a slow server of its own, on the Service's port.
    let cluster = Pod::http1("cluster", 300).await;
    let routed = view_on(
        cluster.addr.port(),
        &[
            rule("/limited", "shop/other", timeouts),
            rule("/zero", "shop/other", Some(r#"{"requestMs":0}"#)),
        ],
        own.addr,
        &[slow.addr],
    );
    let state = MeshState::new(&MeshConfig {
        enabled: true,
        seed: Some(1),
        stale_after: Duration::from_millis(1000),
        ..MeshConfig::default()
    });
    state.hold(routed, "\"1-a\"");
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    // Request fields change nothing: the `0s` rule answers, the 100 ms one is cut at 100 ms.
    let fields = "grpc-timeout: 1m\r\nx-envoy-upstream-rq-timeout-ms: 1\r\nx-mimic-timeout: 1\r\n";
    client
        .send(format!("GET /zero HTTP/1.1\r\nHost: cart.shop.svc\r\n{fields}\r\n").as_bytes())
        .await;
    let (head, body) = client.read().await.unwrap();
    assert!(
        head.starts_with("HTTP/1.1 200") && body == b"slow",
        "{head}"
    );
    let began = Instant::now();
    client
        .send(format!("GET /limited HTTP/1.1\r\nHost: cart.shop.svc\r\n{fields}\r\n").as_bytes())
        .await;
    let (head, _) = client.read().await.unwrap();
    let took = began.elapsed();
    assert!(head.starts_with("HTTP/1.1 504"), "{head}");
    assert!(
        took >= Duration::from_millis(100) && took < Duration::from_millis(250),
        "{took:?}"
    );
    // A stale view routes nothing: the original destination answers after 300 ms.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    state.tick();
    let began = Instant::now();
    let (head, body) = client.get("/limited").await;
    assert!(
        head.starts_with("HTTP/1.1 200") && body == "cluster",
        "{head}"
    );
    assert!(began.elapsed() >= Duration::from_millis(300));
    // So does `"mesh": false`.
    state.hold(common::mesh_pods::view_text(&[], 0, false), "\"2-a\"");
    let (head, body) = client.get("/limited").await;
    assert!(
        head.starts_with("HTTP/1.1 200") && body == "cluster",
        "{head}"
    );
}

#[tokio::test]
async fn rte_68_a_port_without_routes_has_no_timeout() {
    let _logs = capture_logs();
    let slow = Pod::http1("slow", 300).await;
    let state = state_with(common::mesh_pods::view(&[slow.addr], 0));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let began = Instant::now();
    let (head, body) = client.get("/").await;
    assert!(head.starts_with("HTTP/1.1 200") && body == "slow", "{head}");
    assert!(began.elapsed() >= Duration::from_millis(300));
}

#[tokio::test]
async fn rte_70_a_new_view_applies_from_the_next_request() {
    let _logs = capture_logs();
    let (own, slow) = (Pod::http1("own", 0).await, Pod::http1("slow", 500).await);
    let with =
        |timeouts: Option<&str>| view(&[rule("/", "shop/other", timeouts)], own.addr, &[slow.addr]);
    let state = state_with(with(Some(r#"{"requestMs":5000}"#)));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    // A request is pending when the view with `request: 100ms` arrives: it is answered `200`.
    client
        .send(b"GET /a HTTP/1.1\r\nHost: cart.shop.svc:80\r\n\r\n")
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    state.hold(with(Some(r#"{"requestMs":100}"#)), "\"2-a\"");
    let (head, body) = client.read().await.unwrap();
    assert!(
        head.starts_with("HTTP/1.1 200") && body == b"slow",
        "{head}"
    );
    // The next request on the same connection is cut after 100 ms.
    let began = Instant::now();
    let (head, _) = client.get("/b").await;
    let took = began.elapsed();
    assert!(head.starts_with("HTTP/1.1 504"), "{head}");
    assert!(
        took >= Duration::from_millis(100) && took < Duration::from_millis(300),
        "{took:?}"
    );
    // A view without the timeouts makes the following one `200` again.
    state.hold(with(None), "\"3-a\"");
    let (head, body) = client.get("/c").await;
    assert!(head.starts_with("HTTP/1.1 200") && body == "slow", "{head}");
}

// RTE-63 over intercepted TLS (the CA set-up of MSH-72).

mod tls_rig {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use super::*;
    use mimic_core::proxy::tls::Exclusions;
    use mimic_runtime::metrics::Metrics;
    use mimic_sidecar::listener::{Direction, OriginalDst, Shared, TunnelMetrics, spawn_listener};
    use mimic_sidecar::red::ProxyMetrics;
    use mimic_sidecar::tls::ca::{CaMetrics, CaState};
    use mimic_sidecar::tls::intercept::{Interception, TlsMetrics};
    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
        KeyUsagePurpose, SanType,
    };
    use tokio_rustls::rustls::pki_types::{
        CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName,
    };
    use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    type Authority = Issuer<'static, KeyPair>;

    fn provider() -> Arc<tokio_rustls::rustls::crypto::CryptoProvider> {
        Arc::new(tokio_rustls::rustls::crypto::ring::default_provider())
    }

    fn authority(name: &str) -> (Authority, CertificateDer<'static>) {
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, name.to_string());
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        (Issuer::new(params, key), cert.der().clone())
    }

    fn pem(der: &CertificateDer<'_>) -> String {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(der.as_ref());
        let lines: Vec<&str> = b64
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            lines.join("\n")
        )
    }

    fn ca_state() -> (Arc<CaState>, CertificateDer<'static>) {
        let (root, root_der) = authority("mimic root");
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "mimic intermediate".to_string());
        let cert = params.signed_by(&key, &root).unwrap();
        let state = Arc::new(CaState::new(CaMetrics::default()).unwrap());
        state
            .install_intermediate(root_der.as_ref(), key, &cert.pem())
            .unwrap();
        (state, root_der)
    }

    /// A pod serving TLS that reads and never answers; what it read is kept.
    pub struct SilentTls {
        pub addr: SocketAddr,
        pub received: Arc<Mutex<Vec<u8>>>,
    }

    async fn silent_tls(ca: &Authority) -> SilentTls {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.subject_alt_names = ["cart.shop.svc", "other.shop.svc"]
            .iter()
            .map(|n| SanType::DnsName((*n).try_into().unwrap()))
            .collect();
        let cert = params.signed_by(&key, ca).unwrap();
        let config = ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
            )
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pod = SilentTls {
            addr: listener.local_addr().unwrap(),
            received: Arc::default(),
        };
        let received = pod.received.clone();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let (acceptor, received) = (acceptor.clone(), received.clone());
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = tls.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        received.lock().unwrap().extend_from_slice(&buf[..n]);
                    }
                });
            }
        });
        pod
    }

    pub struct Rig {
        pub addr: SocketAddr,
        pub root: CertificateDer<'static>,
        pub metrics: Arc<Metrics>,
        pub pod: SilentTls,
        _trust: tempfile::TempDir,
    }

    /// The outbound listener with §5 interception; its routes send `/slow` to a silent TLS pod.
    pub async fn rig() -> Rig {
        let (pod_ca, pod_root) = authority("pods");
        let pod = silent_tls(&pod_ca).await;
        let trust = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(trust.path().join("trust/0")).unwrap();
        std::fs::write(trust.path().join("trust/0/image-ca.pem"), pem(&pod_root)).unwrap();
        let (ca, root) = ca_state();
        let listener = mimic_sidecar::listener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        let mut metrics = Metrics::new("sidecar", "test");
        let tunnel = TunnelMetrics::register(&mut metrics);
        let proxy = ProxyMetrics::register(&mut metrics, 50);
        let tls = TlsMetrics::register(&mut metrics);
        let interception = Interception::new(
            Exclusions::parse("").unwrap(),
            Some(ca),
            Vec::<PathBuf>::new(),
            trust.path().to_path_buf(),
            tls,
        );
        let state = state_with(view(
            &[rule("/slow", "shop/other", Some(TIMED))],
            pod.addr,
            &[pod.addr],
        ));
        state.register(&mut metrics);
        let original: OriginalDst = Arc::new(move |_| Ok(cluster_ip()));
        let shared = Arc::new(
            Shared::new(vec![addr], tunnel, original)
                .with_proxy_metrics(proxy)
                .with_tls(interception)
                .with_mesh(state),
        );
        spawn_listener(listener, Direction::Outbound, shared);
        Rig {
            addr,
            root,
            metrics: Arc::new(metrics),
            pod,
            _trust: trust,
        }
    }

    pub async fn client(rig: &Rig) -> tokio_rustls::client::TlsStream<TcpStream> {
        let mut roots = RootCertStore::empty();
        roots.add(rig.root.clone()).unwrap();
        let config = ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let tcp = TcpStream::connect(rig.addr).await.unwrap();
        TlsConnector::from(Arc::new(config))
            .connect(ServerName::try_from("cart.shop.svc").unwrap(), tcp)
            .await
            .expect("handshake")
    }

    pub use tokio::net::TcpStream;
}

#[tokio::test]
async fn rte_63_intercepted_tls_get_and_post_to_a_silent_pod_answer_504() {
    use tls_rig::{client, rig};
    let logs = capture_logs();
    let rig = rig().await;
    let mut tls = client(&rig).await;
    for request in [
        "GET /slow HTTP/1.1\r\nHost: cart.shop.svc\r\n\r\n".to_string(),
        format!(
            "POST /slow HTTP/1.1\r\nHost: cart.shop.svc\r\ncontent-length: 1024\r\n\r\n{}",
            "x".repeat(1024)
        ),
    ] {
        let began = Instant::now();
        tls.write_all(request.as_bytes()).await.unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 1024];
        while !got.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = tokio::time::timeout(Duration::from_secs(5), tls.read(&mut buf))
                .await
                .expect("an answer in time")
                .expect("a read");
            assert!(n > 0, "closed before the answer");
            got.extend_from_slice(&buf[..n]);
        }
        let took = began.elapsed();
        let head = lower(&String::from_utf8_lossy(&got));
        assert!(head.starts_with("http/1.1 504"), "{head}");
        assert!(
            took >= Duration::from_millis(200) && took < Duration::from_millis(250),
            "answered after {took:?}"
        );
        assert_eq!(head.matches("x-mimic-error").count(), 1, "{head}");
        assert!(head.contains("x-mimic-error: timeout"), "{head}");
        assert!(head.contains("content-length: 0"), "{head}");
        assert!(!head.contains("connection:"), "kept: {head}");
    }
    assert!(
        rig.pod.received.lock().unwrap().len() > 1024,
        "reached the pod"
    );
    let scrape = rig.metrics.encode();
    assert_eq!(
        metric(
            &scrape,
            "mimic_mesh_attempts_total{",
            &["result=\"timeout\""]
        ),
        2.0
    );
    let lines = logs.access_exactly(2).await;
    for entry in &lines {
        assert_eq!(entry["status"], 504, "{entry}");
        assert_eq!(entry["result"], "timeout", "{entry}");
        assert_eq!(entry["timeout"], "request", "{entry}");
        assert_eq!(entry["tls"], true, "{entry}");
    }
}

// RTE-68: mock mode, an HTTP/1 `Upgrade` and an L4 connection are not cut.

#[tokio::test]
async fn rte_68_mock_mode_is_not_cut_by_a_route_timeout() {
    use common::mock_rig::{Options, rig_with, users_set};
    use mimic_core::Mode;
    use mimic_sidecar::mode::Announced;
    let (own, pod, cluster) = (
        Pod::http1("own", 0).await,
        Pod::http1("pod", 300).await,
        Pod::http1("cluster", 0).await,
    );
    let state = state_with(view_on(
        cluster.addr.port(),
        &[rule("/", "shop/other", Some(r#"{"requestMs":100}"#))],
        own.addr,
        &[pod.addr],
    ));
    // `rig_with` captures the logs.
    let rig = rig_with(
        users_set(300),
        100,
        1024,
        None,
        Options {
            mode: Some(Mode::Discovery),
            dst: Some(cluster.addr),
            mesh: Some(state),
            ..Options::default()
        },
    )
    .await;
    let request = "GET /users/1 HTTP/1.1\r\nhost: users.test:8080\r\n\r\n";
    let mut client = H1Client::connect(rig.addr).await;
    // The route is live: balanced, the slow pod is cut at 100 ms.
    let began = Instant::now();
    client.send(request.as_bytes()).await;
    let (head, _) = client.read().await.unwrap();
    assert!(head.starts_with("HTTP/1.1 504"), "{head}");
    assert!(began.elapsed() < Duration::from_millis(250));
    // Mocked, the answer takes 300 ms and is not cut.
    rig.mode
        .observe(Announced {
            mode: Ok(Mode::Mock),
            factor: None,
        })
        .await;
    let began = Instant::now();
    client.send(request.as_bytes()).await;
    let (head, _) = client.read().await.unwrap();
    assert!(
        head.starts_with("HTTP/1.1 200") && lower(&head).contains("x-mimic-match"),
        "{head}"
    );
    assert!(
        began.elapsed() >= Duration::from_millis(280),
        "{:?}",
        began.elapsed()
    );
}

#[tokio::test]
async fn rte_68_an_http1_upgrade_is_not_cut_by_a_route_timeout() {
    let _logs = capture_logs();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let pod = listener.local_addr().unwrap();
    // The pod switches protocols after 300 ms, then echoes.
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let mut seen = Vec::new();
                loop {
                    while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                        match s.read(&mut buf).await {
                            Ok(n) if n > 0 => seen.extend_from_slice(&buf[..n]),
                            _ => return,
                        }
                    }
                    let end = seen.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                    let head = lower(&String::from_utf8_lossy(&seen[..end]));
                    seen.drain(..end);
                    if head.contains("upgrade: websocket") {
                        break;
                    }
                    let ok = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
                    if s.write_all(ok).await.is_err() {
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
                let reply = b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n";
                if s.write_all(reply).await.is_err() {
                    return;
                }
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    let state = state_with(view(
        &[rule("/", "shop/cart", Some(r#"{"requestMs":100}"#))],
        pod,
        &[],
    ));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    // Only a request after a routed one leaves `hyper` for a tunnel.
    assert_eq!(client.get("/one").await.1, "ok");
    let began = Instant::now();
    client
        .send(b"GET /ws HTTP/1.1\r\nHost: cart.shop.svc\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n")
        .await;
    let (head, _) = client.read().await.expect("the 101");
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert!(began.elapsed() >= Duration::from_millis(280));
    tokio::time::sleep(Duration::from_millis(200)).await;
    client.send(b"after the timeout").await;
    assert_eq!(client.read_raw(17).await, b"after the timeout");
}

#[tokio::test]
async fn rte_68_an_l4_connection_is_not_cut_by_a_route_timeout() {
    let _logs = capture_logs();
    let echo = common::mesh_pods::EchoPod::start().await;
    let state = state_with(view(
        &[rule("/", "shop/cart", Some(r#"{"requestMs":100}"#))],
        echo.addr,
        &[],
    ));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut c = tokio::net::TcpStream::connect(f.addr).await.unwrap();
    let binary: &[u8] = b"\x00\x01binary\xff\x00protocol-bytes";
    c.write_all(binary).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    c.write_all(binary).await.unwrap();
    let mut got = vec![0u8; binary.len() * 2];
    tokio::time::timeout(Duration::from_secs(5), c.read_exact(&mut got))
        .await
        .expect("an echo in time")
        .expect("an echo");
    assert_eq!(echo.accepted(), 1, "the tunnel went to the pod");
}

// RTE-64, RTE-65: the timeouts across tries.

/// A pod that answers a trusted refusal `after` it read a request head; `stall` leaves the
/// body of the refusal (a `content-length` of 100) unsent.
struct RefusingPod {
    addr: SocketAddr,
}

impl RefusingPod {
    async fn start(after: Duration, stall: bool) -> RefusingPod {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let mut pending = Vec::new();
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 {
                            return;
                        }
                        pending.extend_from_slice(&buf[..n]);
                        while let Some(p) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                            pending.drain(..p + 4);
                            tokio::time::sleep(after).await;
                            let length = if stall { 100 } else { 0 };
                            let answer = format!(
                                "HTTP/1.1 503 Service Unavailable\r\nx-mimic-error: overloaded\r\nx-mimic-endpoint: {addr}\r\ncontent-length: {length}\r\n\r\n"
                            );
                            if s.write_all(answer.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        RefusingPod { addr }
    }
}

async fn http1_call(f: &common::Fixture, request: &str) -> (String, Duration) {
    let mut client = H1Client::connect(f.addr).await;
    let began = Instant::now();
    client.send(request.as_bytes()).await;
    let (head, _) = client.read().await.expect("an answer");
    (head, began.elapsed())
}

const GET_SLOW: &str = "GET /slow HTTP/1.1\r\nHost: cart.shop.svc:80\r\n\r\n";
const POST_SLOW: &str =
    "POST /slow HTTP/1.1\r\nHost: cart.shop.svc:80\r\ncontent-length: 2\r\n\r\nok";

fn count(f: &common::Fixture, name: &str, labels: &[&str]) -> f64 {
    metric(&f.metrics.encode(), name, labels)
}

#[tokio::test]
async fn rte_64_a_connect_failure_then_a_silent_cluster_ip_is_cut_by_backend_request() {
    let logs = capture_logs();
    let silent = SilentPod::start().await;
    let refused = common::closed_port().await;
    let state = state_with(view_on(
        silent.addr.port(),
        &[rule_on(
            "/slow",
            ("shop/cart", silent.addr.port()),
            Some(r#"{"requestMs":1000,"backendRequestMs":100}"#),
        )],
        refused,
        &[],
    ));
    let f = fixture_mesh(silent.addr, &state, None).await;
    let (head, took) = http1_call(&f, GET_SLOW).await;
    assert!(head.starts_with("HTTP/1.1 504"), "{head}");
    assert!(
        took >= Duration::from_millis(100) && took < Duration::from_millis(150),
        "{took:?}"
    );
    let attempts = |result: &str| {
        count(
            &f,
            "mimic_mesh_attempts_total{",
            &[&format!("result=\"{result}\"")],
        )
    };
    assert_eq!(attempts("connect_failed"), 1.0);
    assert_eq!(attempts("timeout"), 1.0);
    assert_eq!(
        count(
            &f,
            "mimic_mesh_retries_total{",
            &["reason=\"connect_failed\""]
        ),
        1.0
    );
    assert_eq!(count(&f, "mimic_mesh_fallbacks_total", &[]), 1.0);
    let lines = logs.access_exactly(1).await;
    assert_eq!(lines[0]["attempts"], 2, "{}", lines[0]);
    assert_eq!(lines[0]["upstream"], silent.addr.to_string());
    assert_eq!(lines[0]["timeout"], "backendRequest");
}

#[tokio::test]
async fn rte_64_two_silent_pods_are_cut_after_one_try_and_never_retried() {
    let _logs = capture_logs();
    let (a, b) = (SilentPod::start().await, SilentPod::start().await);
    let own = Pod::http1("own", 0).await;
    let state = state_with(view(
        &[rule(
            "/slow",
            "shop/other",
            Some(r#"{"requestMs":1000,"backendRequestMs":100}"#),
        )],
        own.addr,
        &[a.addr, b.addr],
    ));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    for request in [GET_SLOW, POST_SLOW] {
        let (head, took) = http1_call(&f, request).await;
        assert!(head.starts_with("HTTP/1.1 504"), "{head}");
        assert!(
            took >= Duration::from_millis(100) && took < Duration::from_millis(150),
            "{took:?}"
        );
    }
    assert_eq!(a.requests() + b.requests(), 2, "each reached one pod only");
    assert_eq!(count(&f, "mimic_mesh_retries_total{", &[]), 0.0);
    assert_eq!(count(&f, "mimic_mesh_retries_skipped_total{", &[]), 0.0);
    assert_eq!(state.window_retries_for_test("shop/other"), 0);
    assert_eq!(
        count(&f, "mimic_mesh_attempts_total{", &["result=\"timeout\""]),
        2.0
    );
}

#[tokio::test]
async fn rte_65_request_spans_a_blackholed_connect_and_frees_its_slot() {
    let logs = capture_logs();
    let hole = common::mesh_pods::Blackhole::new().await;
    let own = Pod::http1("own", 0).await;
    let state = state_configured(
        view(
            &[
                rule("/slow", "shop/other", Some(r#"{"requestMs":300}"#)),
                rule("/", "shop/cart", None),
            ],
            own.addr,
            &[hole.addr],
        ),
        |c| c.connect_timeout = Duration::from_secs(2),
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    for _ in 0..2 {
        // The pod is not avoided after it: the second request is tried on it as the first.
        let (head, took) = http1_call(&f, GET_SLOW).await;
        assert!(head.starts_with("HTTP/1.1 504"), "{head}");
        assert!(
            took >= Duration::from_millis(300) && took < Duration::from_millis(350),
            "{took:?}"
        );
    }
    let attempts = |result: &str| {
        count(
            &f,
            "mimic_mesh_attempts_total{",
            &[&format!("result=\"{result}\"")],
        )
    };
    assert_eq!(attempts("timeout"), 2.0);
    assert_eq!(attempts("connect_failed"), 0.0);
    let (head, took) = http1_call(&f, "GET /fast HTTP/1.1\r\nHost: cart.shop.svc:80\r\n\r\n").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(took < Duration::from_millis(100), "{took:?}");
    logs.settled().await;
}

#[tokio::test]
async fn rte_65_refusals_and_request_with_no_delay_the_second_try_is_cut() {
    let logs = capture_logs();
    let pods = [
        RefusingPod::start(Duration::from_millis(200), false).await,
        RefusingPod::start(Duration::from_millis(200), false).await,
        RefusingPod::start(Duration::from_millis(200), false).await,
    ];
    let own = Pod::http1("own", 0).await;
    let addrs: Vec<SocketAddr> = pods.iter().map(|p| p.addr).collect();
    let state = state_configured(
        view(
            &[rule("/slow", "shop/other", Some(r#"{"requestMs":300}"#))],
            own.addr,
            &addrs,
        ),
        |c| c.refusal_delay = Duration::ZERO,
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let (head, took) = http1_call(&f, POST_SLOW).await;
    assert!(head.starts_with("HTTP/1.1 504"), "{head}");
    assert!(
        took >= Duration::from_millis(300) && took < Duration::from_millis(350),
        "{took:?}"
    );
    let attempts = |result: &str| {
        count(
            &f,
            "mimic_mesh_attempts_total{",
            &[&format!("result=\"{result}\"")],
        )
    };
    assert_eq!(attempts("overloaded"), 1.0);
    assert_eq!(attempts("timeout"), 1.0);
    assert_eq!(
        count(&f, "mimic_mesh_retries_total{", &["reason=\"overloaded\""]),
        1.0
    );
    let lines = logs.access_exactly(1).await;
    assert_eq!(lines[0]["attempts"], 2, "{}", lines[0]);
}

#[tokio::test]
async fn rte_65_a_refusal_delay_longer_than_the_request_answers_504_during_the_delay() {
    let logs = capture_logs();
    let pods = [
        RefusingPod::start(Duration::from_millis(200), false).await,
        RefusingPod::start(Duration::from_millis(200), false).await,
    ];
    let own = Pod::http1("own", 0).await;
    let addrs: Vec<SocketAddr> = pods.iter().map(|p| p.addr).collect();
    let state = state_configured(
        view(
            &[rule("/slow", "shop/other", Some(r#"{"requestMs":300}"#))],
            own.addr,
            &addrs,
        ),
        |c| c.refusal_delay = Duration::from_millis(500),
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let (head, took) = http1_call(&f, POST_SLOW).await;
    assert!(head.starts_with("HTTP/1.1 504"), "{head}");
    assert!(
        took >= Duration::from_millis(300) && took < Duration::from_millis(350),
        "{took:?}"
    );
    assert_eq!(count(&f, "mimic_mesh_retries_total{", &[]), 0.0);
    let lines = logs.access_exactly(1).await;
    assert_eq!(lines[0]["attempts"], 1, "{}", lines[0]);
    assert_eq!(lines[0]["timeout"], "request", "{}", lines[0]);
}

#[tokio::test]
async fn rte_65_a_refusal_whose_body_never_ends_is_cut_by_request() {
    let _logs = capture_logs();
    let pods = [
        RefusingPod::start(Duration::from_millis(100), true).await,
        RefusingPod::start(Duration::from_millis(100), true).await,
    ];
    let own = Pod::http1("own", 0).await;
    let addrs: Vec<SocketAddr> = pods.iter().map(|p| p.addr).collect();
    let state = state_with(view(
        &[rule("/slow", "shop/other", Some(r#"{"requestMs":300}"#))],
        own.addr,
        &addrs,
    ));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let (head, took) = http1_call(&f, GET_SLOW).await;
    assert!(head.starts_with("HTTP/1.1 504"), "{head}");
    assert!(
        took >= Duration::from_millis(300) && took < Duration::from_millis(350),
        "{took:?}"
    );
}

#[tokio::test]
async fn rte_65_a_slow_pod_inside_the_request_answers_whole() {
    let _logs = capture_logs();
    let (own, slow) = (Pod::http1("own", 0).await, Pod::http1("slow", 100).await);
    let state = state_with(view(
        &[rule("/slow", "shop/other", Some(r#"{"requestMs":300}"#))],
        own.addr,
        &[slow.addr],
    ));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let (head, body) = H1Client::connect(f.addr).await.get("/slow").await;
    assert!(head.starts_with("HTTP/1.1 200") && body == "slow", "{head}");
}

// RTE-66: expiry after the response head.

/// An HTTP/1 pod whose answer starts and stalls: `head` and 100 bytes of body, then nothing,
/// the connection left open. `chunked` frames the 100 bytes as one chunk.
async fn trickling_pod(chunked: bool) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let mut seen = Vec::new();
                while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf).await {
                        Ok(n) if n > 0 => seen.extend_from_slice(&buf[..n]),
                        _ => return,
                    }
                }
                let body = "y".repeat(100);
                let answer = if chunked {
                    format!("HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n64\r\n{body}\r\n")
                } else {
                    format!("HTTP/1.1 200 OK\r\ncontent-length: 1000\r\n\r\n{body}")
                };
                let _ = s.write_all(answer.as_bytes()).await;
                while matches!(s.read(&mut buf).await, Ok(n) if n > 0) {}
            });
        }
    });
    addr
}

async fn rte_66_http1(chunked: bool) {
    let logs = capture_logs();
    let (own, trickle) = (Pod::http1("own", 0).await, trickling_pod(chunked).await);
    let state = state_with(view(
        &[rule("/slow", "shop/other", Some(r#"{"requestMs":300}"#))],
        own.addr,
        &[trickle],
    ));
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let f = fixture_mesh(cluster_ip(), &state, Some(&discovery)).await;
    let mut app = tokio::net::TcpStream::connect(f.addr).await.unwrap();
    let began = Instant::now();
    app.write_all(GET_SLOW.as_bytes()).await.unwrap();
    let mut got = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), app.read_to_end(&mut got))
        .await
        .expect("the connection is closed")
        .ok();
    let took = began.elapsed();
    assert!(
        took >= Duration::from_millis(300) && took < Duration::from_millis(350),
        "closed after {took:?}"
    );
    let text = String::from_utf8_lossy(&got).into_owned();
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert_eq!(text.matches('y').count(), 100, "the head and 100 bytes");
    assert!(!text.ends_with("0\r\n\r\n"), "no last chunk: {text:?}");
    let lines = logs.access_exactly(1).await;
    assert_eq!(lines[0]["status"], 200, "{}", lines[0]);
    assert_eq!(lines[0]["result"], "timeout", "{}", lines[0]);
    assert_eq!(lines[0]["timeout"], "request", "{}", lines[0]);
    let scrape = f.metrics.encode();
    assert_eq!(
        metric(&scrape, "mimic_mesh_attempts_total{", &["result=\"ok\""]),
        1.0
    );
    assert_eq!(
        metric(
            &scrape,
            "mimic_mesh_attempts_total{",
            &["result=\"timeout\""]
        ),
        0.0
    );
    assert_eq!(
        metric(
            &scrape,
            "mimic_mesh_requests_total{",
            &["status_class=\"2xx\"", "reason=\"none\""]
        ),
        1.0
    );
    // Discovery records the exchange as it records a response cut by an upstream reset.
    common::counted(
        &f.metrics,
        "mimic_discovery_exchanges_total{direction=\"outbound\",result=\"recorded\"} 1",
    )
    .await;
}

#[tokio::test]
async fn rte_66_http1_a_response_that_stalls_after_the_head_closes_the_connection() {
    rte_66_http1(false).await;
}

#[tokio::test]
async fn rte_66_http1_a_chunked_response_that_stalls_closes_without_a_last_chunk() {
    rte_66_http1(true).await;
}

/// An `h2c` pod whose `/slow` answers `200` and 100 bytes, then holds the stream open.
async fn h2_trickling_pod() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let Ok(mut conn) = h2::server::handshake(stream).await else {
                    return;
                };
                while let Some(Ok((_request, mut respond))) = conn.accept().await {
                    tokio::spawn(async move {
                        let Ok(mut send) = respond.send_response(hyper::Response::new(()), false)
                        else {
                            return;
                        };
                        let _ = send.send_data(Bytes::from(vec![b'y'; 100]), false);
                        let _ = std::future::poll_fn(|cx| send.poll_reset(cx)).await;
                    });
                }
            });
        }
    });
    addr
}

/// The reason of an `h2` reset behind `error`.
fn reset_reason(error: &(dyn std::error::Error + 'static)) -> Option<h2::Reason> {
    let mut at = Some(error);
    while let Some(e) = at {
        if let Some(h2) = e.downcast_ref::<h2::Error>() {
            return h2.reason();
        }
        at = e.source();
    }
    None
}

#[tokio::test]
async fn rte_66_h2c_a_stalled_stream_is_reset_internal_error_and_others_complete() {
    let _logs = capture_logs();
    let (own, trickle) = (Pod::h2c("own", 500, None).await, h2_trickling_pod().await);
    let state = state_with(view(
        &[
            rule("/slow", "shop/other", Some(r#"{"requestMs":300}"#)),
            rule("/ok", "shop/cart", None),
        ],
        own.addr,
        &[trickle],
    ));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let stream = tokio::net::TcpStream::connect(f.addr).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, Full<Bytes>>(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let mut other = sender.clone();
    let ok = tokio::spawn(async move {
        let request = Request::get("http://cart.shop.svc/ok")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let response = other.send_request(request).await.expect("an answer");
        let status = response.status();
        response.into_body().collect().await.unwrap();
        status
    });
    let began = Instant::now();
    let request = Request::get("http://cart.shop.svc/slow")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(3), sender.send_request(request))
        .await
        .expect("the head in time")
        .expect("the head");
    assert_eq!(response.status(), 200);
    let mut body = response.into_body();
    let mut seen = 0;
    let error = loop {
        let frame = tokio::time::timeout(Duration::from_secs(3), body.frame())
            .await
            .expect("the stream is reset in time");
        match frame {
            Some(Ok(frame)) => seen += frame.data_ref().map_or(0, Bytes::len),
            Some(Err(e)) => break e,
            None => panic!("the stream ended whole after {seen} bytes"),
        }
    };
    assert_eq!(seen, 100);
    assert_eq!(reset_reason(&error), Some(h2::Reason::INTERNAL_ERROR));
    let took = began.elapsed();
    assert!(
        took >= Duration::from_millis(300) && took < Duration::from_millis(350),
        "{took:?}"
    );
    assert_eq!(ok.await.unwrap(), 200, "another stream completes");
}

#[tokio::test]
async fn rte_66_a_large_response_streamed_inside_the_request_is_delivered_whole() {
    let _logs = capture_logs();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let pod = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                if s.read(&mut buf).await.unwrap_or(0) == 0 {
                    return;
                }
                let _ = s
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 102400\r\n\r\n")
                    .await;
                for _ in 0..100 {
                    if s.write_all(&[b'z'; 1024]).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                let _ = s.read(&mut buf).await;
            });
        }
    });
    let own = Pod::http1("own", 0).await;
    let state = state_with(view(
        &[rule("/slow", "shop/other", Some(r#"{"requestMs":10000}"#))],
        own.addr,
        &[pod],
    ));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    client.send(GET_SLOW.as_bytes()).await;
    let (head, body) = client.read().await.expect("the answer");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body.len(), 102_400);
}

// RTE-69: timer bounds and client abort.

/// An `h2c` pod that accepts streams and never answers; it counts the resets it sees.
struct SilentH2 {
    addr: SocketAddr,
    streams: Arc<AtomicUsize>,
    resets: Arc<AtomicUsize>,
}

impl SilentH2 {
    async fn start() -> SilentH2 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pod = SilentH2 {
            addr: listener.local_addr().unwrap(),
            streams: Arc::default(),
            resets: Arc::default(),
        };
        let (streams, resets) = (pod.streams.clone(), pod.resets.clone());
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (streams, resets) = (streams.clone(), resets.clone());
                tokio::spawn(async move {
                    let Ok(mut conn) = h2::server::Builder::new()
                        .max_concurrent_streams(5000)
                        .handshake::<_, Bytes>(stream)
                        .await
                    else {
                        return;
                    };
                    while let Some(Ok((_request, mut respond))) = conn.accept().await {
                        streams.fetch_add(1, Ordering::SeqCst);
                        let resets = resets.clone();
                        tokio::spawn(async move {
                            if std::future::poll_fn(|cx| respond.poll_reset(cx))
                                .await
                                .is_ok()
                            {
                                resets.fetch_add(1, Ordering::SeqCst);
                            }
                        });
                    }
                });
            }
        });
        pod
    }
}

const HOUR: &str = r#"{"requestMs":3600000,"backendRequestMs":3600000}"#;

async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..100 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ok(), "not within 1 s: {what}");
}

#[tokio::test]
async fn rte_69_http1_a_thousand_aborted_requests_leave_no_timer_and_no_connection() {
    let _logs = capture_logs();
    let (own, silent) = (Pod::http1("own", 0).await, SilentPod::start().await);
    let state = state_with(view(
        &[rule("/", "shop/other", Some(HOUR))],
        own.addr,
        &[silent.addr],
    ));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut clients = Vec::new();
    for i in 0..1000 {
        let mut c = tokio::net::TcpStream::connect(f.addr).await.unwrap();
        let request = if i % 2 == 0 {
            "GET /a HTTP/1.1\r\nHost: cart.shop.svc:80\r\n\r\n"
        } else {
            "POST /a HTTP/1.1\r\nHost: cart.shop.svc:80\r\ncontent-length: 100\r\n\r\n0123456789"
        };
        c.write_all(request.as_bytes()).await.unwrap();
        clients.push(c);
    }
    eventually("the pod has every head", || silent.requests() == 1000).await;
    let armed = state.armed_timers_for_test();
    assert!(
        (1..=2000).contains(&armed),
        "{armed} deadlines armed for 1000 requests"
    );
    // Half of the clients close, the others reset.
    for (i, c) in clients.into_iter().enumerate() {
        if i % 4 < 2 {
            socket2::SockRef::from(&c)
                .set_linger(Some(Duration::ZERO))
                .unwrap();
        }
        drop(c);
    }
    eventually("no deadline armed", || state.armed_timers_for_test() == 0).await;
    eventually("the pod saw every connection closed", || {
        silent.open.load(Ordering::SeqCst) == 0
    })
    .await;
    let scrape = f.metrics.encode();
    assert_eq!(metric(&scrape, "mimic_mesh_endpoint_in_flight{", &[]), 0.0);
    assert_eq!(metric(&scrape, "mimic_mesh_retry_buffer_bytes", &[]), 0.0);
}

#[tokio::test]
async fn rte_69_h2c_a_thousand_cancelled_streams_leave_no_timer() {
    let _logs = capture_logs();
    let (own, silent) = (Pod::h2c("own", 0, None).await, SilentH2::start().await);
    let state = state_with(view(
        &[rule("/", "shop/other", Some(HOUR))],
        own.addr,
        &[silent.addr],
    ));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut calls = Vec::new();
    for _ in 0..10 {
        let stream = tokio::net::TcpStream::connect(f.addr).await.unwrap();
        let (sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .handshake::<_, Full<Bytes>>(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(connection);
        for _ in 0..100 {
            let mut sender = sender.clone();
            calls.push(tokio::spawn(async move {
                let request = Request::get("http://cart.shop.svc/a")
                    .body(Full::new(Bytes::new()))
                    .unwrap();
                let _ = sender.send_request(request).await;
            }));
        }
    }
    eventually("the pod has every stream", || {
        silent.streams.load(Ordering::SeqCst) == 1000
    })
    .await;
    let armed = state.armed_timers_for_test();
    assert!(
        (1..=2000).contains(&armed),
        "{armed} deadlines armed for 1000 requests"
    );
    // Dropping the pending requests sends `RST_STREAM CANCEL`.
    for call in calls {
        call.abort();
    }
    eventually("no deadline armed", || state.armed_timers_for_test() == 0).await;
    eventually("the pod saw every stream reset", || {
        silent.resets.load(Ordering::SeqCst) == 1000
    })
    .await;
    let scrape = f.metrics.encode();
    assert_eq!(metric(&scrape, "mimic_mesh_endpoint_in_flight{", &[]), 0.0);
}

#[tokio::test]
async fn rte_69_a_retried_request_never_has_more_than_two_deadlines_armed() {
    let _logs = capture_logs();
    let silent = SilentPod::start().await;
    let refused = [
        common::closed_port().await,
        common::closed_port().await,
        common::closed_port().await,
    ];
    let state = state_with(view_on(
        silent.addr.port(),
        &[rule_on(
            "/slow",
            ("shop/cart", silent.addr.port()),
            Some(HOUR),
        )],
        refused[0],
        &refused[1..],
    ));
    let f = fixture_mesh(silent.addr, &state, None).await;
    let mut app = tokio::net::TcpStream::connect(f.addr).await.unwrap();
    app.write_all(GET_SLOW.as_bytes()).await.unwrap();
    let mut most = 0;
    for _ in 0..40 {
        most = most.max(state.armed_timers_for_test());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(silent.requests() >= 1, "the fallback try is pending");
    assert!(
        (1..=2).contains(&most),
        "{most} deadlines armed over the tries"
    );
    drop(app);
    eventually("no deadline armed", || state.armed_timers_for_test() == 0).await;
}
