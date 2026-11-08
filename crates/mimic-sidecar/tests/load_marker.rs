//! LDT-11 (sidecar load marker and `/load` probe) and the verification side of LDT-20: an
//! inbound sidecar in process, a fake app, tickets signed with the root of the test's
//! `MIMIC_TLS_CA`. Every wait has a deadline.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Request as AxumRequest, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::{Logs, capture_logs, counted};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::client::conn::http2::SendRequest;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::Mode;
use mimic_core::load::ticket::{self, Payload};
use mimic_runtime::metrics::Metrics;
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
use mimic_sidecar::listener::{Direction, OriginalDst, Shared, TunnelMetrics, spawn_listener};
use mimic_sidecar::load::{LoadConfig, Marker};
use mimic_sidecar::mode::{Announced, ModeConfig, ModeState};
use mimic_sidecar::red::ProxyMetrics;
use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair};
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;

const TIMEOUT: Duration = Duration::from_secs(10);
const NAMESPACE: &str = "shop";
const DEPLOYMENT: &str = "payment";
const POD: &str = "payment-6d4c8f9b7-xxxxx";
const UID: &str = "0b1f6d8a-3c52-4a63-9d0e-6a8f1f6d3c11";

// --- tickets ----------------------------------------------------------------------------

/// A deployment root: its certificate and the key that signs tickets.
struct Root {
    der: Vec<u8>,
    pkcs8: Vec<u8>,
}

fn root(name: &str) -> Root {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, name.to_string());
    let key = KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    Root {
        der: cert.der().to_vec(),
        pkcs8: key.serialize_der(),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn payload(namespace: &str, deployment: &str, uid: &str, exp: u64) -> Payload {
    Payload {
        deployment: deployment.into(),
        exp,
        namespace: namespace.into(),
        uid: uid.into(),
    }
}

fn sign(root: &Root, payload: &Payload) -> String {
    ticket::issue(payload, |input| {
        let rng = SystemRandom::new();
        let key =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &root.pkcs8, &rng).unwrap();
        key.sign(&rng, input).unwrap().as_ref().to_vec()
    })
}

fn valid(root: &Root) -> String {
    sign(root, &payload(NAMESPACE, DEPLOYMENT, UID, now() + 600))
}

// --- the fake apps ----------------------------------------------------------------------

/// What an app received: the request line (HTTP/1) and the headers, names in lower case.
#[derive(Clone, Debug, Default)]
struct Seen {
    line: String,
    headers: Vec<(String, String)>,
}

impl Seen {
    fn header(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }
}

struct App {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    /// Bytes read from every connection, whatever they were.
    bytes: Arc<AtomicUsize>,
}

/// Reads one HTTP/1 message (head, then `content-length` bytes) from `s`.
async fn read_message(s: &mut TcpStream, pending: &mut Vec<u8>) -> Option<(String, Vec<u8>)> {
    loop {
        if let Some(p) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&pending[..p]).into_owned();
            let length = head
                .lines()
                .find_map(|l| {
                    let (n, v) = l.split_once(':')?;
                    n.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            if pending.len() >= p + 4 + length {
                let body = pending[p + 4..p + 4 + length].to_vec();
                pending.drain(..p + 4 + length);
                return Some((head, body));
            }
        }
        let mut buf = vec![0u8; 64 * 1024];
        match s.read(&mut buf).await {
            Ok(n) if n > 0 => pending.extend_from_slice(&buf[..n]),
            _ => return None,
        }
    }
}

fn seen_of(head: &str) -> Seen {
    let mut lines = head.lines();
    let line = lines.next().unwrap_or_default().to_string();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Seen { line, headers }
}

/// An HTTP/1 app: answers `200` with `extra` response head lines.
async fn app_h1(extra: &'static str) -> App {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let bytes = Arc::new(AtomicUsize::new(0));
    let (log, count) = (seen.clone(), bytes.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let (log, count) = (log.clone(), count.clone());
            tokio::spawn(async move {
                let mut pending = Vec::new();
                loop {
                    let Some((head, _)) = read_message(&mut s, &mut pending).await else {
                        return;
                    };
                    count.fetch_add(head.len() + 4, Ordering::SeqCst);
                    log.lock().unwrap().push(seen_of(&head));
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n{extra}content-length: 2\r\n\r\nok"
                    );
                    if s.write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    App { addr, seen, bytes }
}

/// An h2c app: answers `200` with the `extra` headers.
async fn app_h2(extra: &'static [(&'static str, &'static str)]) -> App {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = listener.accept().await else {
                return;
            };
            let log = log.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let log = log.clone();
                    async move {
                        let headers = req
                            .headers()
                            .iter()
                            .map(|(n, v)| (n.as_str().to_string(), v.to_str().unwrap().into()))
                            .collect();
                        log.lock().unwrap().push(Seen {
                            line: format!("{} {}", req.method(), req.uri().path()),
                            headers,
                        });
                        let mut response = Response::builder().status(200);
                        for (n, v) in extra {
                            response = response.header(*n, *v);
                        }
                        Ok::<_, Infallible>(response.body(Full::new(Bytes::from("ok"))).unwrap())
                    }
                });
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(s), service)
                    .await;
            });
        }
    });
    App {
        addr,
        seen,
        bytes: Arc::new(AtomicUsize::new(0)),
    }
}

// --- the sidecar ------------------------------------------------------------------------

struct Rig {
    addr: SocketAddr,
    metrics: Arc<Metrics>,
    mode: ModeState,
    discovery: Discovery,
    marker: Marker,
    logs: Logs,
}

async fn rig(app: &App, mode: Mode, root: Option<&Root>) -> Rig {
    let logs = capture_logs();
    let mut metrics = Metrics::new("sidecar", "test");
    let mode = ModeState::start(
        &ModeConfig {
            mode,
            ..ModeConfig::default()
        },
        &mut metrics,
    )
    .await;
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some(POD.into())).unwrap();
    discovery.register(&mut metrics);
    let marker = Marker::new(
        LoadConfig {
            namespace: NAMESPACE.into(),
            deployment: DEPLOYMENT.into(),
            pod: POD.into(),
            root_der: root.map(|r| r.der.clone()),
        },
        mode.clone(),
        &mut metrics,
    );
    let tunnel = TunnelMetrics::register(&mut metrics);
    let proxy = ProxyMetrics::register(&mut metrics, 50);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let dst = app.addr;
    let original: OriginalDst = Arc::new(move |_| Ok(dst));
    let shared = Arc::new(
        Shared::new(vec![addr], tunnel, original)
            .with_proxy_metrics(proxy)
            .with_discovery(discovery.tap())
            .with_load(marker.clone()),
    );
    spawn_listener(listener, Direction::Inbound, shared);
    Rig {
        addr,
        metrics: Arc::new(metrics),
        mode,
        discovery,
        marker,
        logs,
    }
}

impl Rig {
    async fn switch(&self, mode: Mode) {
        self.mode
            .observe(Announced {
                mode: Ok(mode),
                factor: None,
            })
            .await;
    }

    /// The value of the series that starts with `series`, 0 when there is none.
    fn counter(&self, series: &str) -> u64 {
        self.metrics
            .encode()
            .lines()
            .find(|l| l.starts_with(series))
            .and_then(|l| l.rsplit(' ').next())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    fn load_counter(&self, result: &str) -> u64 {
        self.counter(&format!("mimic_load_inbound_total{{result=\"{result}\"}}"))
    }
}

// --- HTTP/1 client ----------------------------------------------------------------------

struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Answer {
    fn header(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }
}

async fn read_answer(c: &mut TcpStream, pending: &mut Vec<u8>) -> Answer {
    let (head, body) = tokio::time::timeout(TIMEOUT, read_message(c, pending))
        .await
        .expect("answered in time")
        .expect("a response");
    let seen = seen_of(&head);
    Answer {
        status: seen.line.split(' ').nth(1).unwrap().parse().unwrap(),
        headers: seen.headers,
        body,
    }
}

fn get(ticket: Option<&str>) -> Vec<u8> {
    let marker = ticket
        .map(|t| format!("mimic-load-run: {t}\r\n"))
        .unwrap_or_default();
    format!("GET /pay HTTP/1.1\r\nHost: app.test:8080\r\n{marker}\r\n").into_bytes()
}

/// One request on a new connection.
async fn call(addr: SocketAddr, request: &[u8]) -> Answer {
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(request).await.unwrap();
    read_answer(&mut c, &mut Vec::new()).await
}

// --- the h2c client ---------------------------------------------------------------------

async fn h2c(addr: SocketAddr) -> SendRequest<Full<Bytes>> {
    let tcp = TcpStream::connect(addr).await.unwrap();
    let (send, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp))
            .await
            .unwrap();
    tokio::spawn(conn);
    send
}

async fn h2_call(sender: &mut SendRequest<Full<Bytes>>, ticket: Option<&str>) -> Answer {
    let mut request = Request::builder().uri("http://app.test:8080/pay");
    if let Some(ticket) = ticket {
        request = request.header("mimic-load-run", ticket);
    }
    let response = tokio::time::timeout(
        TIMEOUT,
        sender.send_request(request.body(Full::new(Bytes::new())).unwrap()),
    )
    .await
    .expect("answered in time")
    .unwrap();
    let (parts, body) = response.into_parts();
    let body = tokio::time::timeout(TIMEOUT, body.collect())
        .await
        .expect("body in time")
        .unwrap()
        .to_bytes();
    Answer {
        status: parts.status.as_u16(),
        headers: parts
            .headers
            .iter()
            .map(|(n, v)| (n.as_str().to_string(), v.to_str().unwrap().into()))
            .collect(),
        body: body.to_vec(),
    }
}

// --- LDT-11: forwarded ------------------------------------------------------------------

/// What a forwarded load request leaves behind, on both protocols.
async fn assert_forwarded(rig: &Rig, app: &App, answer: &Answer, protocol: &str) {
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body, b"ok");
    assert_eq!(
        answer.header("x-mimic-load"),
        vec!["ok"],
        "exactly one marker: {:?}",
        answer.headers
    );
    let seen = app.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].header("mimic-load-run").is_empty(),
        "the app got the ticket: {:?}",
        seen[0]
    );
    counted(
        &rig.metrics,
        "mimic_load_inbound_total{result=\"forwarded\"} 1",
    )
    .await;
    counted(
        &rig.metrics,
        "mimic_discovery_exchanges_total{direction=\"inbound\",result=\"load\"} 1",
    )
    .await;
    assert_eq!(
        rig.counter(&format!(
            "mimic_http_requests_total{{direction=\"inbound\",protocol=\"{protocol}\",authority=\"app.test:8080\",status_class=\"2xx\"}}"
        )),
        1,
        "{}",
        rig.metrics.encode()
    );
    let access = rig.logs.access_exactly(1).await;
    assert_eq!(access[0]["load_run"], UID, "{access:?}");
    assert_eq!(access[0]["direction"], "inbound");
    // Not recorded: nothing reaches the report, whatever the recorder does with its queue.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(rig.discovery.report().endpoints().is_empty());
    assert_eq!(
        rig.counter("mimic_discovery_exchanges_total{direction=\"inbound\",result=\"recorded\"}"),
        0
    );
}

#[tokio::test]
async fn ldt_11_http1_in_mock_a_valid_ticket_reaches_the_app_without_the_header() {
    let root = root("deployment root");
    let app = app_h1("").await;
    let rig = rig(&app, Mode::Mock, Some(&root)).await;
    let answer = call(rig.addr, &get(Some(&valid(&root)))).await;
    assert_forwarded(&rig, &app, &answer, "http1").await;
    let seen = app.seen.lock().unwrap().clone();
    assert_eq!(seen[0].line, "GET /pay HTTP/1.1");
    assert_eq!(seen[0].header("host"), vec!["app.test:8080"]);
}

#[tokio::test]
async fn ldt_11_http1_the_marker_of_the_app_is_replaced_by_one_ok() {
    let root = root("deployment root");
    // Both spellings, and a refusal text: none of them reaches the client.
    let app =
        app_h1("x-mimic-load: refused; reason=mode\r\nX-Mimic-Load: ok\r\nx-mimic-load: ok\r\n")
            .await;
    let rig = rig(&app, Mode::Mock, Some(&root)).await;
    let answer = call(rig.addr, &get(Some(&valid(&root)))).await;
    assert_forwarded(&rig, &app, &answer, "http1").await;
}

#[tokio::test]
async fn ldt_11_h2c_in_mock_a_valid_ticket_reaches_the_app_without_the_header() {
    let root = root("deployment root");
    let app = app_h2(&[]).await;
    let rig = rig(&app, Mode::Mock, Some(&root)).await;
    let mut sender = h2c(rig.addr).await;
    let answer = h2_call(&mut sender, Some(&valid(&root))).await;
    assert_forwarded(&rig, &app, &answer, "h2").await;
}

#[tokio::test]
async fn ldt_11_h2c_the_marker_of_the_app_is_replaced_by_one_ok() {
    let root = root("deployment root");
    let app = app_h2(&[
        ("x-mimic-load", "refused; reason=mode"),
        ("x-mimic-load", "ok"),
    ])
    .await;
    let rig = rig(&app, Mode::Mock, Some(&root)).await;
    let mut sender = h2c(rig.addr).await;
    let answer = h2_call(&mut sender, Some(&valid(&root))).await;
    assert_forwarded(&rig, &app, &answer, "h2").await;
}

// --- LDT-11 / LDT-20: refused -----------------------------------------------------------

struct Case {
    name: &'static str,
    mode: Mode,
    ticket: String,
    /// Absent: the sidecar has no `MIMIC_TLS_CA`.
    root: bool,
    reason: &'static str,
}

fn cases(good: &Root) -> Vec<Case> {
    let other = root("another deployment root");
    let mode_ticket = valid(good);
    let case = |name, mode, ticket: String, root, reason| Case {
        name,
        mode,
        ticket,
        root,
        reason,
    };
    // The payload of a valid ticket changed to another uid, under the old signature.
    let tampered = {
        let original = valid(good);
        let signature = original.rsplit('.').next().unwrap().to_string();
        let changed = payload(NAMESPACE, DEPLOYMENT, "another-uid", now() + 600);
        format!("v1.{}.{signature}", URL_SAFE_NO_PAD.encode(changed.bytes()))
    };
    let mut flipped = valid(good).into_bytes();
    let last = flipped.len() - 2;
    flipped[last] = if flipped[last] == b'A' { b'B' } else { b'A' };
    vec![
        case("discovery", Mode::Discovery, mode_ticket, true, "mode"),
        case(
            "another deployment",
            Mode::Discovery,
            sign(good, &payload(NAMESPACE, "ledger", UID, now() + 600)),
            true,
            "workload",
        ),
        case(
            "another namespace",
            Mode::Mock,
            sign(good, &payload("other", DEPLOYMENT, UID, now() + 600)),
            true,
            "workload",
        ),
        case("changed payload", Mode::Discovery, tampered, true, "ticket"),
        case(
            "changed signature",
            Mode::Mock,
            String::from_utf8(flipped).unwrap(),
            true,
            "ticket",
        ),
        case("another root", Mode::Mock, valid(&other), true, "ticket"),
        case(
            "expired",
            Mode::Mock,
            sign(good, &payload(NAMESPACE, DEPLOYMENT, UID, now() - 10)),
            true,
            "ticket",
        ),
        case(
            "malformed",
            Mode::Discovery,
            "not-a-ticket".into(),
            true,
            "ticket",
        ),
        case(
            "no prefix",
            Mode::Mock,
            valid(good).strip_prefix("v1.").unwrap().into(),
            true,
            "ticket",
        ),
        case("no MIMIC_TLS_CA", Mode::Mock, valid(good), false, "ticket"),
    ]
}

fn assert_refused(answer: &Answer, reason: &str, name: &str) {
    assert_eq!(answer.status, 599, "{name}");
    assert_eq!(
        answer.header("x-mimic-load"),
        vec![format!("refused; reason={reason}")],
        "{name}"
    );
    assert_eq!(answer.header("content-length"), vec!["0"], "{name}");
    assert!(answer.body.is_empty(), "{name}");
}

#[tokio::test]
async fn ldt_11_http1_refusals_answer_599_and_the_app_receives_no_byte() {
    let good = root("deployment root");
    for case in cases(&good) {
        let app = app_h1("").await;
        let rig = rig(&app, case.mode, case.root.then_some(&good)).await;
        let answer = call(rig.addr, &get(Some(&case.ticket))).await;
        assert_refused(&answer, case.reason, case.name);
        assert_eq!(app.bytes.load(Ordering::SeqCst), 0, "{}", case.name);
        assert!(app.seen.lock().unwrap().is_empty(), "{}", case.name);
        counted(
            &rig.metrics,
            &format!(
                "mimic_load_inbound_total{{result=\"refused_{}\"}} 1",
                case.reason
            ),
        )
        .await;
        assert_eq!(rig.load_counter("forwarded"), 0, "{}", case.name);
        assert!(rig.discovery.report().endpoints().is_empty());
    }
}

#[tokio::test]
async fn ldt_11_h2c_refusals_answer_599_and_the_stream_never_reaches_the_app() {
    let good = root("deployment root");
    for case in cases(&good) {
        let app = app_h2(&[]).await;
        let rig = rig(&app, case.mode, case.root.then_some(&good)).await;
        let mut sender = h2c(rig.addr).await;
        let answer = h2_call(&mut sender, Some(&case.ticket)).await;
        assert_refused(&answer, case.reason, case.name);
        assert!(app.seen.lock().unwrap().is_empty(), "{}", case.name);
        counted(
            &rig.metrics,
            &format!(
                "mimic_load_inbound_total{{result=\"refused_{}\"}} 1",
                case.reason
            ),
        )
        .await;
    }
}

#[tokio::test]
async fn ldt_11_a_pull_to_discovery_refuses_the_next_request_on_the_same_connection() {
    let root = root("deployment root");
    let app = app_h1("").await;
    let rig = rig(&app, Mode::Mock, Some(&root)).await;
    let ticket = valid(&root);
    let mut c = TcpStream::connect(rig.addr).await.unwrap();
    let mut pending = Vec::new();
    c.write_all(&get(Some(&ticket))).await.unwrap();
    let first = read_answer(&mut c, &mut pending).await;
    assert_eq!(first.status, 200);
    assert_eq!(first.header("x-mimic-load"), vec!["ok"]);
    rig.switch(Mode::Discovery).await;
    c.write_all(&get(Some(&ticket))).await.unwrap();
    let second = read_answer(&mut c, &mut pending).await;
    assert_refused(&second, "mode", "after the pull");
    assert_eq!(app.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn ldt_11_a_marked_request_after_plain_ones_on_a_connection_is_checked_too() {
    let root = root("deployment root");
    let app = app_h1("").await;
    let rig = rig(&app, Mode::Discovery, Some(&root)).await;
    let mut c = TcpStream::connect(rig.addr).await.unwrap();
    let mut pending = Vec::new();
    c.write_all(&get(None)).await.unwrap();
    let plain = read_answer(&mut c, &mut pending).await;
    assert_eq!(plain.status, 200);
    assert!(plain.header("x-mimic-load").is_empty());
    c.write_all(&get(Some(&valid(&root)))).await.unwrap();
    let marked = read_answer(&mut c, &mut pending).await;
    assert_refused(&marked, "mode", "marked after a plain request");
    // The connection goes on for plain requests.
    c.write_all(&get(None)).await.unwrap();
    let again = read_answer(&mut c, &mut pending).await;
    assert_eq!(again.status, 200);
    let seen = app.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert!(seen.iter().all(|s| s.header("mimic-load-run").is_empty()));
}

#[tokio::test]
async fn ldt_11_a_request_head_in_pieces_is_judged_whole() {
    let root = root("deployment root");
    let app = app_h1("").await;
    let rig = rig(&app, Mode::Discovery, Some(&root)).await;
    let request = get(Some(&valid(&root)));
    let mut c = TcpStream::connect(rig.addr).await.unwrap();
    let (a, b) = request.split_at(request.len() / 2);
    c.write_all(a).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Nothing of an unfinished head reaches the app.
    assert_eq!(app.bytes.load(Ordering::SeqCst), 0);
    c.write_all(b).await.unwrap();
    let answer = read_answer(&mut c, &mut Vec::new()).await;
    assert_refused(&answer, "mode", "split head");
    assert!(app.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ldt_11_requests_without_the_header_behave_as_before() {
    let root = root("deployment root");
    let app = app_h1("").await;
    let rig = rig(&app, Mode::Discovery, Some(&root)).await;
    let request =
        b"GET /pay?x=1 HTTP/1.1\r\nHost: app.test:8080\r\nX-Mimic-Load-Other: 1\r\nAccept: */*\r\n\r\n";
    let answer = call(rig.addr, request).await;
    assert_eq!(answer.status, 200);
    assert!(answer.header("x-mimic-load").is_empty());
    let seen = app.seen.lock().unwrap().clone();
    assert_eq!(seen[0].header("x-mimic-load-other"), vec!["1"]);
    assert_eq!(rig.load_counter("forwarded"), 0);
    assert_eq!(rig.load_counter("refused_mode"), 0);
    // The exchange is discovered as ever.
    counted(
        &rig.metrics,
        "mimic_discovery_exchanges_total{direction=\"inbound\",result=\"recorded\"} 1",
    )
    .await;
    let h2 = app_h2(&[]).await;
    let rig = self::rig(&h2, Mode::Discovery, Some(&root)).await;
    let mut sender = h2c(rig.addr).await;
    let answer = h2_call(&mut sender, None).await;
    assert_eq!(answer.status, 200);
    assert!(answer.header("x-mimic-load").is_empty());
}

#[tokio::test]
async fn ldt_11_one_warning_per_reason_per_mode_period() {
    let root = root("deployment root");
    let app = app_h1("").await;
    let rig = rig(&app, Mode::Discovery, Some(&root)).await;
    let warnings = |reason: &str| {
        rig.logs
            .text()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["message"] == "load request refused" && v["reason"] == reason)
            .inspect(|v| assert_eq!(v["level"], "WARN"))
            .count()
    };
    for _ in 0..3 {
        call(rig.addr, &get(Some("garbage"))).await;
        call(rig.addr, &get(Some(&valid(&root)))).await;
    }
    rig.logs.settled().await;
    assert_eq!(warnings("ticket"), 1);
    assert_eq!(warnings("mode"), 1);
    // A new period: the mode changed.
    rig.switch(Mode::Mock).await;
    rig.switch(Mode::Discovery).await;
    call(rig.addr, &get(Some("garbage"))).await;
    rig.logs.settled().await;
    assert_eq!(warnings("ticket"), 2);
    assert_eq!(warnings("mode"), 1);
}

// --- LDT-11: the probe ------------------------------------------------------------------

async fn probe(rig: &Rig, method: &str) -> (StatusCode, Vec<u8>) {
    let response = rig
        .marker
        .router()
        .oneshot(
            AxumRequest::builder()
                .method(method)
                .uri("/load")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, body.to_vec())
}

#[tokio::test]
async fn ldt_11_get_load_answers_the_identity_and_the_mode() {
    let app = app_h1("").await;
    let rig = rig(&app, Mode::Mock, None).await;
    let (status, body) = probe(&rig, "GET").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        String::from_utf8(body).unwrap(),
        format!(
            r#"{{"deployment":"{DEPLOYMENT}","mode":"mock","namespace":"{NAMESPACE}","pod":"{POD}"}}"#
        )
    );
    rig.switch(Mode::Discovery).await;
    let (_, body) = probe(&rig, "GET").await;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["mode"], "discovery");
    for method in ["POST", "PUT", "DELETE", "PATCH"] {
        let (status, _) = probe(&rig, method).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
    }
}
