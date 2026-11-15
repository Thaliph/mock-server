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
    accepted: Arc<AtomicUsize>,
    open: Arc<AtomicUsize>,
}

impl SilentPod {
    async fn start() -> SilentPod {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (accepted, open) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (log, a, o) = (received.clone(), accepted.clone(), open.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                a.fetch_add(1, Ordering::SeqCst);
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
            accepted,
            open,
        }
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
    let timeouts = timeouts.map_or_else(String::new, |t| format!(r#","timeouts":{t}"#));
    format!(
        r#"{{"backends":[{{"port":80,"service":"{service}","weight":1}}],"matches":[{{"path":{{"type":"PathPrefix","value":"{path}"}}}}]{timeouts}}}"#
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
    let state = MeshState::new(&MeshConfig {
        enabled: true,
        seed: Some(7),
        ..MeshConfig::default()
    });
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
            took >= Duration::from_millis(200) && took < Duration::from_millis(350),
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
            took >= Duration::from_millis(200) && took < Duration::from_millis(350),
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
                    loop {
                        let Ok(n) = stream.read(&mut buf).await else {
                            break;
                        };
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
                                tokio::time::sleep(Duration::from_millis(200)).await;
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
        stale_after: Duration::from_millis(200),
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
    tokio::time::sleep(Duration::from_millis(400)).await;
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
