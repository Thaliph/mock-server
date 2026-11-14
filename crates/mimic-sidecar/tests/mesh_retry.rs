//! RTY-08 to RTY-15 and RTY-20: the retry loop of mesh-routed HTTP/1 and `h2c` requests (spec
//! §7.9 "Retries"): connect failures, refused streams, resets, trusted refusals, bodies, the M1
//! paths reconciled, and the metrics and the access line of the tries.

mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::mesh_pods::{EchoPod, H1Client, Pod, cluster_ip, view_on};
use common::{capture_logs, closed_port, fixture_mesh};
use http_body_util::{BodyExt, Full};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_sidecar::mesh::{MeshConfig, MeshState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn state_for(
    cluster: SocketAddr,
    pods: &[SocketAddr],
    configure: impl FnOnce(&mut MeshConfig),
) -> MeshState {
    let mut config = MeshConfig {
        enabled: true,
        seed: Some(9),
        ..MeshConfig::default()
    };
    configure(&mut config);
    let state = MeshState::new(&config);
    state.hold(view_on(cluster, pods, 0, true), "\"1-a\"");
    state
}

/// The sum of the lines of `name` that carry every fragment of `labels`.
fn metric(metrics: &str, name: &str, labels: &[&str]) -> f64 {
    metrics
        .lines()
        .filter(|l| l.starts_with(name) && labels.iter().all(|f| l.contains(f)))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
}

fn attempts(metrics: &str, result: &str) -> f64 {
    metric(
        metrics,
        "mimic_mesh_attempts_total{",
        &[&format!("result=\"{result}\"")],
    )
}

fn retries(metrics: &str, reason: &str) -> f64 {
    metric(
        metrics,
        "mimic_mesh_retries_total{",
        &[&format!("reason=\"{reason}\"")],
    )
}

fn skipped(metrics: &str, reason: &str) -> f64 {
    metric(
        metrics,
        "mimic_mesh_retries_skipped_total{",
        &[&format!("reason=\"{reason}\"")],
    )
}

/// What a probe pod does with a request.
#[derive(Clone)]
enum Mode {
    Ok,
    /// `503` with this `x-mimic-error` and `x-mimic-endpoint` the pod's own address.
    Refuse(&'static str),
    /// The same, answered before the body is read.
    RefuseEarly(&'static str),
    /// The same for the first `n` requests, then answers.
    RefuseTimes(&'static str, usize),
    /// `503 overloaded` naming this endpoint.
    Names(String),
    /// `503 overloaded` naming the pod itself twice.
    Twice,
    Status(u16, Option<&'static str>),
}

/// (method, body, when) of every request.
type Seen = Arc<Mutex<Vec<(String, Vec<u8>, Instant)>>>;

struct Probe {
    addr: SocketAddr,
    seen: Seen,
    mode: Arc<Mutex<Mode>>,
}

impl Probe {
    async fn start(name: &'static str, h2: bool, mode: Mode) -> Probe {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Seen = Arc::default();
        let mode = Arc::new(Mutex::new(mode));
        let (log, shared) = (seen.clone(), mode.clone());
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (log, mode) = (log.clone(), shared.clone());
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let (log, mode) = (log.clone(), mode.clone());
                    async move {
                        let method = req.method().to_string();
                        let early = matches!(*mode.lock().unwrap(), Mode::RefuseEarly(_));
                        let body = if early {
                            Bytes::new()
                        } else {
                            req.into_body().collect().await.unwrap().to_bytes()
                        };
                        let nth = {
                            let mut log = log.lock().unwrap();
                            log.push((method, body.to_vec(), Instant::now()));
                            log.len()
                        };
                        let mode = mode.lock().unwrap().clone();
                        let own = addr.to_string();
                        let mut builder = Response::builder();
                        let status = match &mode {
                            Mode::Refuse(e) | Mode::RefuseEarly(e) => {
                                refusal(&mut builder, e, &[&own])
                            }
                            Mode::RefuseTimes(e, n) if nth <= *n => {
                                refusal(&mut builder, e, &[&own])
                            }
                            Mode::Names(other) => refusal(&mut builder, "overloaded", &[other]),
                            Mode::Twice => refusal(&mut builder, "overloaded", &[&own, &own]),
                            Mode::Status(code, error) => {
                                if let Some(error) = error {
                                    builder = builder.header("x-mimic-error", *error);
                                }
                                *code
                            }
                            _ => 200,
                        };
                        Ok::<_, std::convert::Infallible>(
                            builder
                                .status(status)
                                .header("x-pod", name)
                                .body(Full::new(Bytes::from(name)))
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
        Probe { addr, seen, mode }
    }

    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    fn set(&self, mode: Mode) {
        *self.mode.lock().unwrap() = mode;
    }
}

fn refusal(builder: &mut hyper::http::response::Builder, error: &str, endpoints: &[&str]) -> u16 {
    let mut b = std::mem::take(builder).header("x-mimic-error", error);
    for e in endpoints {
        b = b.header("x-mimic-endpoint", *e);
    }
    *builder = b;
    503
}

/// An `h2c` server that resets every stream with `reason`.
async fn h2_refuser(reason: h2::Reason, requests: Arc<AtomicUsize>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let requests = requests.clone();
            tokio::spawn(async move {
                let Ok(mut conn) = h2::server::handshake(stream).await else {
                    return;
                };
                while let Some(Ok((_, mut respond))) = conn.accept().await {
                    requests.fetch_add(1, Ordering::SeqCst);
                    respond.send_reset(reason);
                }
            });
        }
    });
    addr
}

/// An `h2c` server written frame by frame: on the first `HEADERS` it sends `GOAWAY` with a
/// last-stream-id of `last` and keeps the connection open.
async fn h2_goaway(requests: Arc<AtomicUsize>, last: u8) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let requests = requests.clone();
            tokio::spawn(async move {
                let mut preface = [0u8; 24];
                if stream.read_exact(&mut preface).await.is_err() {
                    return;
                }
                let _ = stream.write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0]).await;
                loop {
                    let mut head = [0u8; 9];
                    if stream.read_exact(&mut head).await.is_err() {
                        return;
                    }
                    let len = usize::from(head[0]) << 16
                        | usize::from(head[1]) << 8
                        | usize::from(head[2]);
                    let mut payload = vec![0u8; len];
                    if stream.read_exact(&mut payload).await.is_err() {
                        return;
                    }
                    if head[3] == 1 {
                        requests.fetch_add(1, Ordering::SeqCst);
                        let ack = [0, 0, 0, 4, 1, 0, 0, 0, 0];
                        let goaway = [0, 0, 8, 7, 0, 0, 0, 0, 0, 0, 0, 0, last, 0, 0, 0, 0];
                        let _ = stream.write_all(&ack).await;
                        let _ = stream.write_all(&goaway).await;
                        if last > 0 {
                            // The stream was processed: the pod goes away with it unanswered.
                            return;
                        }
                    }
                }
            });
        }
    });
    addr
}

/// An `h2c` request through the sidecar: the status, the head and the body, or the error.
async fn h2_call(
    addr: SocketAddr,
    method: &str,
    body: Vec<u8>,
) -> Result<(u16, hyper::HeaderMap, Vec<u8>), String> {
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, Full<Bytes>>(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let request = Request::builder()
        .method(method)
        .uri("http://cart.shop.svc/p")
        .body(Full::new(Bytes::from(body)))
        .unwrap();
    let response = sender
        .send_request(request)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let (parts, body) = response.into_parts();
    let body = body
        .collect()
        .await
        .map_err(|e| format!("{e:?}"))?
        .to_bytes();
    Ok((parts.status.as_u16(), parts.headers, body.to_vec()))
}

async fn h1_request(client: &mut H1Client, method: &str, body: &[u8]) -> (String, Vec<u8>) {
    let mut request = format!(
        "{method} /p HTTP/1.1\r\nHost: cart.shop.svc\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(body);
    client.send(&request).await;
    client.read().await.expect("an answer")
}

fn payload(n: usize) -> Vec<u8> {
    (0..n).map(|i| u8::try_from(i % 251).unwrap()).collect()
}

fn lower(head: &str) -> String {
    head.to_lowercase()
}

#[tokio::test]
async fn rty_08_a_pod_refusing_connections_is_skipped_by_every_method_and_protocol() {
    let logs = capture_logs();
    for h2 in [false, true] {
        let (a, b) = (
            Probe::start("a", h2, Mode::Ok).await,
            Probe::start("b", h2, Mode::Ok).await,
        );
        let state = state_for(cluster_ip(), &[a.addr, closed_port().await, b.addr], |_| {});
        let f = fixture_mesh(cluster_ip(), &state, None).await;
        let body = payload(1024);
        let mut client = H1Client::connect(f.addr).await;
        for n in 0..150 {
            let method = if n % 2 == 0 { "POST" } else { "GET" };
            let sent = if n % 2 == 0 { body.clone() } else { Vec::new() };
            let (status, got) = if h2 {
                let (status, _, got) = h2_call(f.addr, method, sent).await.unwrap();
                (status, got)
            } else {
                let (head, got) = h1_request(&mut client, method, &sent).await;
                (head[9..12].parse().unwrap(), got)
            };
            assert_eq!(status, 200, "request {n} (h2 {h2})");
            assert!(got == b"a" || got == b"b");
        }
        for pod in [&a, &b] {
            for (method, got, _) in pod.seen.lock().unwrap().iter() {
                let want = if method == "POST" { &body[..] } else { &[][..] };
                assert_eq!(got, want, "the body of a {method}");
            }
        }
        assert_eq!(
            a.count() + b.count(),
            150,
            "each request reached a pod once"
        );
        let scrape = f.metrics.encode();
        let failed = attempts(&scrape, "connect_failed");
        assert!(failed >= 1.0, "{scrape}");
        assert_eq!(retries(&scrape, "connect_failed"), failed, "retries");
        assert_eq!(
            metric(&scrape, "mimic_mesh_fallbacks_total", &[]),
            0.0,
            "no fallback"
        );
    }
    drop(logs);
}

#[tokio::test]
async fn rty_08_five_refusing_pods_end_on_the_cluster_ip_after_three_pod_connects() {
    let logs = capture_logs();
    let cluster = Pod::http1("cluster", 0).await;
    let mut pods = Vec::new();
    for _ in 0..5 {
        pods.push(closed_port().await);
    }
    let state = state_for(cluster.addr, &pods, |_| {});
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..4 {
        let (head, body) = client.get("/").await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(body, "cluster");
    }
    let scrape = f.metrics.encode();
    assert_eq!(
        attempts(&scrape, "connect_failed"),
        12.0,
        "3 pod connects each"
    );
    assert_eq!(attempts(&scrape, "ok"), 4.0);
    assert_eq!(
        metric(&scrape, "mimic_mesh_fallbacks_total", &[]),
        4.0,
        "fallbacks"
    );
    for entry in logs.access_exactly(4).await {
        assert_eq!(entry["attempts"], 4, "{entry}");
        assert_eq!(entry["upstream"], cluster.addr.to_string(), "{entry}");
    }
}

#[tokio::test]
async fn rty_08_the_cluster_ip_refusing_too_answers_503_connect_failed_after_four_tries() {
    let logs = capture_logs();
    let dead = closed_port().await;
    let mut pods = Vec::new();
    for _ in 0..5 {
        pods.push(closed_port().await);
    }
    let state = state_for(dead, &pods, |_| {});
    let f = fixture_mesh(dead, &state, None).await;
    let (head, body) = H1Client::connect(f.addr).await.get("/").await;
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    assert!(
        lower(&head).contains("x-mimic-error: connect-failed"),
        "{head}"
    );
    assert!(body.is_empty());
    let scrape = f.metrics.encode();
    assert_eq!(attempts(&scrape, "connect_failed"), 4.0, "{scrape}");
    assert_eq!(skipped(&scrape, "exhausted"), 1.0, "{scrape}");
    let lines = logs.access_exactly(1).await;
    assert_eq!(lines[0]["attempts"], 4, "{}", lines[0]);
}

#[tokio::test]
async fn rty_09_refused_streams_and_goaway_are_retried_on_another_pod() {
    let _logs = capture_logs();
    for reset in [true, false] {
        let seen = Arc::new(AtomicUsize::new(0));
        let refuser = if reset {
            h2_refuser(h2::Reason::REFUSED_STREAM, seen.clone()).await
        } else {
            h2_goaway(seen.clone(), 0).await
        };
        let ok = Probe::start("ok", true, Mode::Ok).await;
        let state = state_for(cluster_ip(), &[refuser, ok.addr], |_| {});
        let f = fixture_mesh(cluster_ip(), &state, None).await;
        let body = payload(512);
        for _ in 0..20 {
            let (status, _, got) = h2_call(f.addr, "POST", body.clone()).await.unwrap();
            assert_eq!(status, 200);
            assert_eq!(got, b"ok");
        }
        assert!(ok.seen.lock().unwrap().iter().all(|(_, b, _)| *b == body));
        let scrape = f.metrics.encode();
        let refused = attempts(&scrape, "refused_stream");
        assert!(refused >= 1.0, "reset {reset}: {scrape}");
        assert_eq!(retries(&scrape, "refused_stream"), refused);
    }
}

#[tokio::test]
async fn rty_09_every_pod_refusing_resets_the_app_stream_with_refused_stream() {
    let logs = capture_logs();
    let seen = Arc::new(AtomicUsize::new(0));
    let (a, b) = (
        h2_refuser(h2::Reason::REFUSED_STREAM, seen.clone()).await,
        h2_refuser(h2::Reason::REFUSED_STREAM, seen.clone()).await,
    );
    let state = state_for(cluster_ip(), &[a, b], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let error = h2_call(f.addr, "POST", payload(64)).await.unwrap_err();
    assert!(error.contains("REFUSED_STREAM"), "{error}");
    assert_eq!(seen.load(Ordering::SeqCst), 4, "four tries");
    let lines = logs.access_exactly(1).await;
    assert_eq!(lines[0]["attempts"], 4, "{}", lines[0]);
}

#[tokio::test]
async fn rty_09_an_internal_error_reset_is_retried_for_a_get_and_not_for_a_post() {
    let _logs = capture_logs();
    let seen = Arc::new(AtomicUsize::new(0));
    let broken = h2_refuser(h2::Reason::INTERNAL_ERROR, seen.clone()).await;
    let ok = Probe::start("ok", true, Mode::Ok).await;
    let state = state_for(cluster_ip(), &[broken, ok.addr], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    for _ in 0..10 {
        let (status, _, got) = h2_call(f.addr, "GET", Vec::new()).await.unwrap();
        assert_eq!((status, got), (200, b"ok".to_vec()));
    }
    let single = state_for(cluster_ip(), &[broken], |_| {});
    let f = fixture_mesh(cluster_ip(), &single, None).await;
    let before = seen.load(Ordering::SeqCst);
    assert!(h2_call(f.addr, "POST", payload(8)).await.is_err());
    assert_eq!(seen.load(Ordering::SeqCst) - before, 1, "sent once");
    assert_eq!(skipped(&f.metrics.encode(), "method"), 1.0);
}

/// Reads the request and closes without a byte of answer; counts the requests by `method`.
fn count_of(pod: &EchoPod, method: &str) -> usize {
    String::from_utf8_lossy(&pod.received())
        .matches(&format!("{method} /p HTTP/1.1"))
        .count()
}

#[tokio::test]
async fn rty_10_a_pod_that_closes_without_answering_is_retried_for_idempotent_methods() {
    let _logs = capture_logs();
    for method in ["GET", "OPTIONS", "TRACE", "PUT", "DELETE"] {
        let closer = EchoPod::closer().await;
        let ok = Probe::start("ok", false, Mode::Ok).await;
        let state = state_for(cluster_ip(), &[closer.addr, ok.addr], |_| {});
        let f = fixture_mesh(cluster_ip(), &state, None).await;
        let mut client = H1Client::connect(f.addr).await;
        for _ in 0..12 {
            let (head, body) = h1_request(&mut client, method, b"").await;
            assert!(head.starts_with("HTTP/1.1 200"), "{method}: {head}");
            assert_eq!(body, b"ok");
        }
        assert_eq!(
            ok.count(),
            12,
            "{method}: every request was answered by the other pod"
        );
        let saw = count_of(&closer, method);
        assert!((1..=12).contains(&saw), "{method}: {saw}");
        let scrape = f.metrics.encode();
        assert_eq!(attempts(&scrape, "reset"), saw as f64, "{method}");
        assert_eq!(retries(&scrape, "reset"), saw as f64, "{method}");
    }
}

#[tokio::test]
async fn rty_10_a_head_request_is_retried_too_and_post_and_patch_are_not() {
    let _logs = capture_logs();
    let closer = EchoPod::closer().await;
    let ok = Probe::start("ok", false, Mode::Ok).await;
    let state = state_for(cluster_ip(), &[closer.addr, ok.addr], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    for _ in 0..8 {
        let mut s = tokio::net::TcpStream::connect(f.addr).await.unwrap();
        s.write_all(b"HEAD /p HTTP/1.1\r\nHost: cart.shop.svc\r\n\r\n")
            .await
            .unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 512];
        while !got.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf))
                .await
                .unwrap()
                .unwrap();
            assert!(n > 0, "closed without an answer");
            got.extend_from_slice(&buf[..n]);
        }
        assert!(got.starts_with(b"HTTP/1.1 200"));
    }
    for method in ["POST", "PATCH"] {
        let closer = EchoPod::closer().await;
        let state = state_for(cluster_ip(), &[closer.addr], |_| {});
        let f = fixture_mesh(cluster_ip(), &state, None).await;
        let mut client = H1Client::connect(f.addr).await;
        client
            .send(
                format!(
                    "{method} /p HTTP/1.1\r\nHost: cart.shop.svc\r\nContent-Length: 2\r\n\r\nab"
                )
                .as_bytes(),
            )
            .await;
        assert!(client.read().await.is_none(), "{method}: closed, no answer");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(count_of(&closer, method), 1, "{method} seen once");
        assert_eq!(skipped(&f.metrics.encode(), "method"), 1.0, "{method}");
    }
}

#[tokio::test]
async fn rty_10_a_failure_after_the_response_head_is_not_retried() {
    let _logs = capture_logs();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let partial = listener.local_addr().unwrap();
    let heads = Arc::new(AtomicUsize::new(0));
    let seen = heads.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                if s.read(&mut buf).await.unwrap_or(0) > 0 {
                    seen.fetch_add(1, Ordering::SeqCst);
                    let _ = s
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\nab")
                        .await;
                }
            });
        }
    });
    let ok = Probe::start("ok", false, Mode::Ok).await;
    let state = state_for(cluster_ip(), &[partial], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    client
        .send(b"GET /p HTTP/1.1\r\nHost: cart.shop.svc\r\n\r\n")
        .await;
    assert!(
        client.read().await.is_none(),
        "the partial answer and the close"
    );
    assert_eq!(heads.load(Ordering::SeqCst), 1, "seen once");
    assert_eq!(ok.count(), 0);
    assert_eq!(retries(&f.metrics.encode(), "reset"), 0.0);
}

#[tokio::test]
async fn rty_11_answers_of_the_app_are_passed_as_received_and_never_retried() {
    let logs = capture_logs();
    let cases = [
        (503, None),
        (503, Some("connect-failed")),
        (500, None),
        (502, None),
    ];
    for (status, error) in cases {
        let bad = Probe::start("bad", false, Mode::Status(status, error)).await;
        let other = Probe::start("other", false, Mode::Ok).await;
        let state = state_for(cluster_ip(), &[bad.addr], |_| {});
        let f = fixture_mesh(cluster_ip(), &state, None).await;
        let _ = &other;
        let mut client = H1Client::connect(f.addr).await;
        let (head, body) = client.get("/p").await;
        assert!(head.starts_with(&format!("HTTP/1.1 {status}")), "{head}");
        assert_eq!(
            lower(&head).contains("x-mimic-error: connect-failed"),
            error.is_some(),
            "{head}"
        );
        assert_eq!(body, "bad");
        assert_eq!(bad.count(), 1, "seen once");
        let scrape = f.metrics.encode();
        assert_eq!(
            metric(&scrape, "mimic_mesh_retries_total{", &[]),
            0.0,
            "{scrape}"
        );
    }
    for entry in logs.access_exactly(4).await {
        assert_eq!(entry["attempts"], 1, "{entry}");
    }
}

#[tokio::test]
async fn rty_12_a_trusted_refusal_is_retried_on_another_pod_after_a_delay() {
    let _logs = capture_logs();
    for (error, result) in [
        ("overloaded", "overloaded"),
        ("queue-timeout", "queue_timeout"),
    ] {
        let refusing = Probe::start("r", false, Mode::Refuse(error)).await;
        let (a, b) = (
            Probe::start("a", false, Mode::Ok).await,
            Probe::start("b", false, Mode::Ok).await,
        );
        let state = state_for(cluster_ip(), &[refusing.addr, a.addr, b.addr], |_| {});
        let f = fixture_mesh(cluster_ip(), &state, None).await;
        let mut client = H1Client::connect(f.addr).await;
        let body = payload(300);
        for _ in 0..20 {
            let (head, got) = h1_request(&mut client, "POST", &body).await;
            assert!(head.starts_with("HTTP/1.1 200"), "{head}");
            assert!(
                !lower(&head).contains("x-mimic-"),
                "the refusal reached the app: {head}"
            );
            assert!(got == b"a" || got == b"b");
        }
        for pod in [&a, &b] {
            assert!(
                pod.seen
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(_, got, _)| *got == body)
            );
        }
        let scrape = f.metrics.encode();
        let refused = attempts(&scrape, result);
        assert!(refused >= 1.0, "{scrape}");
        assert_eq!(retries(&scrape, result), refused, "{scrape}");
        // The try after a refusal starts at least 12 ms later.
        let mut events: Vec<(Instant, bool)> = Vec::new();
        events.extend(refusing.seen.lock().unwrap().iter().map(|e| (e.2, true)));
        for pod in [&a, &b] {
            events.extend(pod.seen.lock().unwrap().iter().map(|e| (e.2, false)));
        }
        events.sort();
        for pair in events.windows(2) {
            if pair[0].1 {
                assert!(
                    pair[1].0 - pair[0].0 >= Duration::from_millis(12),
                    "{:?}",
                    pair[1].0 - pair[0].0
                );
            }
        }
    }
}

#[tokio::test]
async fn rty_12_every_pod_refusing_passes_the_last_refusal_as_received() {
    let logs = capture_logs();
    let pods = [
        Probe::start("a", false, Mode::Refuse("overloaded")).await,
        Probe::start("b", false, Mode::Refuse("overloaded")).await,
        Probe::start("c", false, Mode::Refuse("overloaded")).await,
    ];
    let addrs: Vec<SocketAddr> = pods.iter().map(|p| p.addr).collect();
    let state = state_for(cluster_ip(), &addrs, |_| {});
    let discovery = mimic_sidecar::discovery::Discovery::start(
        &mimic_sidecar::discovery::DiscoveryConfig::default(),
        Some("pod-a".into()),
    )
    .unwrap();
    let f = fixture_mesh(cluster_ip(), &state, Some(&discovery)).await;
    let mut client = H1Client::connect(f.addr).await;
    let (head, _) = h1_request(&mut client, "POST", b"hello").await;
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    assert!(lower(&head).contains("x-mimic-error: overloaded"), "{head}");
    assert!(
        lower(&head).contains("x-mimic-endpoint: 127.0.0.1:"),
        "{head}"
    );
    assert_eq!(
        pods.iter().map(Probe::count).sum::<usize>(),
        4,
        "four tries"
    );
    let scrape = f.metrics.encode();
    assert_eq!(skipped(&scrape, "exhausted"), 1.0, "{scrape}");
    assert_eq!(
        metric(
            &scrape,
            "mimic_mesh_requests_total{",
            &["reason=\"overloaded\"", "status_class=\"5xx\""]
        ),
        1.0,
        "{scrape}"
    );
    common::counted(
        &f.metrics,
        "mimic_discovery_exchanges_total{direction=\"outbound\",result=\"synthetic\"} 1",
    )
    .await;
    assert!(discovery.report().endpoints().is_empty());
    let lines = logs.access_exactly(1).await;
    assert_eq!(
        (lines[0]["status"].as_u64(), lines[0]["attempts"].as_u64()),
        (Some(503), Some(4))
    );
}

#[tokio::test]
async fn rty_13_a_refusal_not_bound_to_the_pod_of_the_try_is_passed_as_received() {
    let _logs = capture_logs();
    let other = Probe::start("other", false, Mode::Ok).await;
    for mode in [
        Mode::Names(other.addr.to_string()),
        Mode::Names(cluster_ip().to_string()),
        Mode::Twice,
    ] {
        let forger = Probe::start("forger", false, mode).await;
        let state = state_for(cluster_ip(), &[forger.addr], |_| {});
        let f = fixture_mesh(cluster_ip(), &state, None).await;
        let (head, _) = H1Client::connect(f.addr).await.get("/p").await;
        assert!(head.starts_with("HTTP/1.1 503"), "{head}");
        assert!(lower(&head).contains("x-mimic-error: overloaded"), "{head}");
        assert_eq!(forger.count(), 1, "seen once");
        assert_eq!(retries(&f.metrics.encode(), "overloaded"), 0.0);
    }
}

#[tokio::test]
async fn rty_13_the_cluster_ip_is_never_trusted() {
    let _logs = capture_logs();
    let cluster = Probe::start("cluster", false, Mode::Ok).await;
    cluster.set(Mode::Names(cluster.addr.to_string()));
    // The only pod refuses connections: the ClusterIP answers, naming itself.
    let state = state_for(cluster.addr, &[closed_port().await], |_| {});
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let (head, _) = H1Client::connect(f.addr).await.get("/p").await;
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    assert_eq!(cluster.count(), 1, "the ClusterIP saw it once");
    // A stale view sends to the ClusterIP once, whatever it answers.
    let state = state_for(cluster.addr, &[], |c| {
        c.stale_after = Duration::from_millis(1)
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let before = cluster.count();
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let (head, _) = H1Client::connect(f.addr).await.get("/p").await;
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    assert_eq!(cluster.count() - before, 1, "sent once");
}

#[tokio::test]
async fn rty_14_a_body_held_in_the_buffer_is_retried_and_one_past_it_is_not_once_written() {
    let _logs = capture_logs();
    let heads = [
        |n: usize| {
            format!("POST /p HTTP/1.1\r\nHost: cart.shop.svc\r\nContent-Length: {n}\r\n\r\n")
        },
        |_| "POST /p HTTP/1.1\r\nHost: cart.shop.svc\r\nTransfer-Encoding: chunked\r\n\r\n".into(),
    ];
    for (i, head) in heads.iter().enumerate() {
        for (size, retried) in [(60 * 1024, true), (70 * 1024, false)] {
            let refusing = Probe::start("r", false, Mode::Refuse("overloaded")).await;
            let ok = Probe::start("ok", false, Mode::Ok).await;
            let state = state_for(cluster_ip(), &[refusing.addr], |_| {});
            state.hold(
                view_on(cluster_ip(), &[refusing.addr, ok.addr], 0, true),
                "\"2-a\"",
            );
            let f = fixture_mesh(cluster_ip(), &state, None).await;
            let body = payload(size);
            let mut request = head(size).into_bytes();
            if i == 0 {
                request.extend_from_slice(&body);
            } else {
                for chunk in body.chunks(8192) {
                    request.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
                    request.extend_from_slice(chunk);
                    request.extend_from_slice(b"\r\n");
                }
                request.extend_from_slice(b"0\r\n\r\n");
            }
            let mut sent_to_ok = 0;
            let mut refused_to_app = 0;
            let mut skipped_buffer = 0.0;
            for _ in 0..12 {
                let mut client = H1Client::connect(f.addr).await;
                client.send(&request).await;
                let (answer, _) = client.read().await.expect("an answer");
                if answer.starts_with("HTTP/1.1 200") {
                    sent_to_ok += 1;
                } else {
                    refused_to_app += 1;
                    assert!(answer.starts_with("HTTP/1.1 503"), "{answer}");
                }
            }
            skipped_buffer += skipped(&f.metrics.encode(), "buffer");
            if retried {
                assert_eq!(refused_to_app, 0, "{size} bytes, head {i}");
                assert_eq!(skipped_buffer, 0.0);
                assert!(ok.seen.lock().unwrap().iter().all(|(_, b, _)| *b == body));
            } else {
                assert!(
                    refused_to_app >= 1 && sent_to_ok >= 1,
                    "{size}: {refused_to_app}/{sent_to_ok}"
                );
                assert_eq!(skipped_buffer, f64::from(refused_to_app), "{size}");
            }
        }
    }
}

#[tokio::test]
async fn rty_14_a_large_body_to_a_pod_refusing_connections_is_retried() {
    let _logs = capture_logs();
    let ok = Probe::start("ok", false, Mode::Ok).await;
    let state = state_for(cluster_ip(), &[closed_port().await, ok.addr], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let body = payload(70 * 1024);
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..10 {
        let (head, _) = h1_request(&mut client, "POST", &body).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    }
    assert!(ok.seen.lock().unwrap().iter().all(|(_, b, _)| *b == body));
}

#[tokio::test]
async fn rty_14_an_h2c_post_in_the_buffer_is_retried() {
    let _logs = capture_logs();
    let refusing = Probe::start("r", true, Mode::Refuse("overloaded")).await;
    let ok = Probe::start("ok", true, Mode::Ok).await;
    let state = state_for(cluster_ip(), &[refusing.addr, ok.addr], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let body = payload(60 * 1024);
    for _ in 0..10 {
        let (status, _, got) = h2_call(f.addr, "POST", body.clone()).await.unwrap();
        assert_eq!((status, got), (200, b"ok".to_vec()));
    }
    assert!(ok.seen.lock().unwrap().iter().all(|(_, b, _)| *b == body));
}

#[tokio::test]
async fn rty_15_an_unwritten_request_goes_to_another_pod_not_the_cluster_ip() {
    let _logs = capture_logs();
    let cluster = Pod::http1("cluster", 0).await;
    let (a, b, c) = (
        Pod::http1("a", 0).await,
        Pod::http1("b", 0).await,
        Pod::http1("c", 0).await,
    );
    let state = state_for(cluster.addr, &[a.addr, b.addr, c.addr], |_| {});
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    client.get("/one").await;
    state.poison_next_checkout();
    state.refuse_next_reconnect();
    let (head, body) = client.get("/two").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_ne!(body, "cluster");
    let scrape = f.metrics.encode();
    assert_eq!(
        metric(&scrape, "mimic_mesh_reconnects_total", &[]),
        1.0,
        "{scrape}"
    );
    assert_eq!(attempts(&scrape, "connect_failed"), 1.0, "{scrape}");
    assert_eq!(retries(&scrape, "connect_failed"), 1.0, "{scrape}");
    assert_eq!(
        metric(&scrape, "mimic_mesh_fallbacks_total", &[]),
        0.0,
        "{scrape}"
    );
    assert_eq!(cluster.counts.requests(), 0);
}

#[tokio::test]
async fn rty_15_no_endpoint_and_the_mock_make_no_try() {
    let _logs = capture_logs();
    let state = state_for(cluster_ip(), &[], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let (head, _) = H1Client::connect(f.addr).await.get("/").await;
    assert!(
        lower(&head).contains("x-mimic-error: no-endpoint"),
        "{head}"
    );
    assert_eq!(
        metric(&f.metrics.encode(), "mimic_mesh_attempts_total{", &[]),
        0.0
    );
}

#[tokio::test]
async fn rty_20_the_metrics_and_the_access_line_follow_the_tries() {
    let logs = capture_logs();
    let cluster = Probe::start("cluster", false, Mode::Ok).await;
    let pod = Probe::start("p", false, Mode::Ok).await;
    let state = state_for(cluster.addr, &[pod.addr], |c| {
        c.refusal_delay = Duration::from_millis(2)
    });
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let mut etag = 1;
    let mut hold = |pods: &[SocketAddr]| {
        etag += 1;
        state.hold(
            view_on(cluster.addr, pods, 0, true),
            &format!("\"{etag}-a\""),
        );
    };
    // One ok.
    assert!(client.get("/").await.0.starts_with("HTTP/1.1 200"));
    // One connect failure, then the ClusterIP.
    hold(&[closed_port().await]);
    assert!(client.get("/").await.0.starts_with("HTTP/1.1 200"));
    // One refusal, then ok.
    pod.set(Mode::RefuseTimes("overloaded", pod.count() + 1));
    hold(&[pod.addr]);
    assert!(client.get("/").await.0.starts_with("HTTP/1.1 200"));
    // One exhausted refusal.
    pod.set(Mode::Refuse("overloaded"));
    assert!(client.get("/").await.0.starts_with("HTTP/1.1 503"));
    let refused_by_pod = pod.count();
    // One POST reset.
    let closer = EchoPod::closer().await;
    hold(&[closer.addr]);
    let mut post = H1Client::connect(f.addr).await;
    post.send(b"POST /p HTTP/1.1\r\nHost: cart.shop.svc\r\nContent-Length: 2\r\n\r\nab")
        .await;
    assert!(post.read().await.is_none());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let scrape = f.metrics.encode();
    assert_eq!(attempts(&scrape, "ok"), 3.0, "{scrape}");
    assert_eq!(attempts(&scrape, "connect_failed"), 1.0, "{scrape}");
    assert_eq!(attempts(&scrape, "overloaded"), 5.0, "{scrape}");
    assert_eq!(attempts(&scrape, "reset"), 1.0, "{scrape}");
    assert_eq!(retries(&scrape, "connect_failed"), 1.0, "{scrape}");
    assert_eq!(retries(&scrape, "overloaded"), 4.0, "{scrape}");
    assert_eq!(retries(&scrape, "reset"), 0.0, "{scrape}");
    assert_eq!(skipped(&scrape, "exhausted"), 1.0, "{scrape}");
    assert_eq!(skipped(&scrape, "method"), 1.0, "{scrape}");
    assert_eq!(
        metric(&scrape, "mimic_mesh_requests_total{", &[]),
        5.0,
        "{scrape}"
    );
    assert_eq!(
        metric(
            &scrape,
            "mimic_mesh_endpoint_requests_total{",
            &[&format!("endpoint=\"{}\"", pod.addr)]
        ),
        refused_by_pod as f64,
        "{scrape}"
    );
    let lines = logs.access_exactly(5).await;
    let tries: Vec<u64> = lines
        .iter()
        .map(|l| l["attempts"].as_u64().unwrap_or(0))
        .collect();
    assert_eq!(tries, [1, 2, 2, 4, 1], "{lines:?}");
    assert!(
        !logs.text().contains("x-mimic-endpoint"),
        "a header value was logged"
    );

    // The series of a backend that left the view are gone 5 minutes after (MSH-79).
    state.hold(common::mesh_pods::view_text(&[], 0, false), "\"99-a\"");
    let now = Instant::now();
    state.evict(now + Duration::from_secs(301));
    let scrape = f.metrics.encode();
    for name in [
        "mimic_mesh_attempts_total{",
        "mimic_mesh_retries_total{",
        "mimic_mesh_retries_skipped_total{",
    ] {
        assert_eq!(
            metric(&scrape, name, &["backend=\"shop/cart:80\""]),
            0.0,
            "{name}"
        );
        assert!(
            !scrape
                .lines()
                .any(|l| l.starts_with(name) && l.contains("shop/cart:80")),
            "{name} series left:\n{scrape}"
        );
    }
}

#[tokio::test]
async fn rty_14_the_total_buffer_in_use_streams_the_next_request() {
    let _logs = capture_logs();
    let slow = Pod::http1("slow", 1500).await;
    let state = state_for(cluster_ip(), &[slow.addr], |c| {
        c.retry_buffer_total = 1024 * 1024
    });
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let body = payload(64 * 1024);
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let (addr, body) = (f.addr, body.clone());
        tasks.push(tokio::spawn(async move {
            let mut client = H1Client::connect(addr).await;
            h1_request(&mut client, "POST", &body).await.0
        }));
    }
    let mut peak = 0.0f64;
    for _ in 0..250 {
        let held = metric(&f.metrics.encode(), "mimic_mesh_retry_buffer_bytes", &[]);
        peak = peak.max(held);
        assert!(held <= 1024.0 * 1024.0, "{held}");
        if held >= 1024.0 * 1024.0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(peak, 1024.0 * 1024.0, "the 16 requests hold the total");
    let refusing = Probe::start("r", false, Mode::Refuse("overloaded")).await;
    state.hold(view_on(cluster_ip(), &[refusing.addr], 0, true), "\"2-a\"");
    let mut client = H1Client::connect(f.addr).await;
    let (head, _) = h1_request(&mut client, "POST", &body).await;
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    assert_eq!(refusing.count(), 1, "not sent again");
    assert_eq!(skipped(&f.metrics.encode(), "buffer"), 1.0);
    for task in tasks {
        assert!(task.await.unwrap().starts_with("HTTP/1.1 200"));
    }
    assert_eq!(
        metric(&f.metrics.encode(), "mimic_mesh_retry_buffer_bytes", &[]),
        0.0,
        "released"
    );
}

#[tokio::test]
async fn rty_09_a_goaway_that_covers_the_stream_is_not_a_refusal() {
    let _logs = capture_logs();
    let seen = Arc::new(AtomicUsize::new(0));
    let pod = h2_goaway(seen.clone(), 1).await;
    let state = state_for(cluster_ip(), &[pod], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    assert!(h2_call(f.addr, "POST", payload(16)).await.is_err());
    assert_eq!(seen.load(Ordering::SeqCst), 1, "sent once");
    let scrape = f.metrics.encode();
    assert_eq!(attempts(&scrape, "refused_stream"), 0.0, "refused");
    assert_eq!(skipped(&scrape, "method"), 1.0, "skipped");
}

/// A chunked body of 8 KiB chunks, the first alone and the rest after 300 ms.
fn slow_chunks(
    body: Vec<u8>,
) -> impl futures::Stream<Item = Result<hyper::body::Frame<Bytes>, std::io::Error>> {
    futures::stream::unfold(0usize, move |at| {
        let body = body.clone();
        async move {
            if at >= body.len() {
                return None;
            }
            if at > 0 {
                tokio::time::sleep(Duration::from_millis(if at == 8192 { 300 } else { 5 })).await;
            }
            let end = (at + 8192).min(body.len());
            let frame = hyper::body::Frame::data(Bytes::copy_from_slice(&body[at..end]));
            Some((Ok(frame), end))
        }
    })
}

#[tokio::test]
async fn rty_14_a_slow_body_refused_early_reaches_the_next_pod_whole() {
    let _logs = capture_logs();
    let body = payload(40 * 1024);
    for h2 in [false, true] {
        let refusing = Probe::start("r", h2, Mode::RefuseEarly("overloaded")).await;
        let ok = Probe::start("ok", h2, Mode::Ok).await;
        let state = state_for(cluster_ip(), &[refusing.addr, ok.addr], |_| {});
        let f = fixture_mesh(cluster_ip(), &state, None).await;
        for _ in 0..6 {
            let one = async {
                if h2 {
                    let stream = tokio::net::TcpStream::connect(f.addr).await.unwrap();
                    let (mut sender, connection) = hyper::client::conn::http2::Builder::new(
                    TokioExecutor::new(),
                )
                .handshake::<_, http_body_util::combinators::UnsyncBoxBody<Bytes, std::io::Error>>(
                    TokioIo::new(stream),
                )
                .await
                .unwrap();
                    tokio::spawn(connection);
                    let request = Request::post("http://cart.shop.svc/p")
                        .body(
                            http_body_util::StreamBody::new(slow_chunks(body.clone()))
                                .boxed_unsync(),
                        )
                        .unwrap();
                    let response = sender.send_request(request).await.unwrap();
                    assert_eq!(response.status(), 200);
                    let _ = response.into_body().collect().await;
                } else {
                    let mut client = H1Client::connect(f.addr).await;
                    client
                    .send(b"POST /p HTTP/1.1\r\nHost: cart.shop.svc\r\nTransfer-Encoding: chunked\r\n\r\n")
                    .await;
                    for (n, chunk) in body.chunks(8192).enumerate() {
                        if n == 1 {
                            tokio::time::sleep(Duration::from_millis(300)).await;
                        }
                        let mut part = format!("{:x}\r\n", chunk.len()).into_bytes();
                        part.extend_from_slice(chunk);
                        part.extend_from_slice(b"\r\n");
                        client.send(&part).await;
                    }
                    client.send(b"0\r\n\r\n").await;
                    let (head, _) = client.read().await.expect("an answer");
                    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
                }
            };
            tokio::time::timeout(Duration::from_secs(10), one)
                .await
                .expect("an answer, not a stalled upload");
        }
        assert!(
            retries(&f.metrics.encode(), "overloaded") >= 1.0,
            "a request was refused (h2 {h2})"
        );
        let seen = ok.seen.lock().unwrap();
        assert_eq!(seen.len(), 6, "h2 {h2}");
        assert!(
            seen.iter().all(|(_, got, _)| *got == body),
            "a body with a hole (h2 {h2})"
        );
    }
}
