//! MCK-18: the sidecar's memory in mock mode (spec §8 "Bounds"): the PUL-18 snapshot held, one
//! endpoint that always draws the 300 s cap, the inbound half of the EGR-15 load running, and
//! 2 048 requests of 256 KiB (4 `h2c` connections of 256 streams, 1 024 HTTP/1 connections).
//! One test only, in its own process: the peak resident set size it checks must not move with
//! other tests.

mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::Mode;
use mimic_core::discovery::egress::EgressSet;
use mimic_core::discovery::observed::Observation;
use mimic_core::discovery::redact::Redactor;
use mimic_core::discovery::report::{Direction as Dir, Exchange, Limits, Report, Via};
use mimic_core::discovery::scope::ScopeKey;
use mimic_runtime::metrics::Metrics;
use mimic_runtime::testing::http_get;
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig, Target};
use mimic_sidecar::egress::{EgressConfig, EgressMetrics, Puller, Snapshot};
use mimic_sidecar::listener::{Direction, OriginalDst, Shared, TunnelMetrics, spawn_listener};
use mimic_sidecar::mock::Mock;
use mimic_sidecar::mode::{ModeConfig, ModeState};
use mimic_sidecar::red::ProxyMetrics;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

const BODY: usize = 256 * 1024;
const CAP: usize = 4 << 20;
const PENDING: usize = 1024;
/// The inbound half is 256 endpoints of 30 exchanges; it holds after 10% of them while the
/// mocked requests are pending.
const HOLD_AT: usize = 256 * 30 / 10;
const H2_CONNECTIONS: usize = 4;
const H2_STREAMS: usize = 256;
const H1_CONNECTIONS: usize = 1024;
const BLOB_AUTHORITY: &str = "blob.test:8080";
const REPLY: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"ok\":true}";

// ---- the inbound half of the EGR-15 load (copied from egress_memory.rs) ----

async fn app() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (s, _) = listener.accept().await.unwrap();
            tokio::spawn(serve(s));
        }
    });
    addr
}

async fn serve(mut s: TcpStream) {
    let mut pending = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let (head_end, length) = loop {
            if let Some(p) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&pending[..p]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:")?.trim().parse().ok())
                    .unwrap_or(0usize);
                break (p + 4, length);
            }
            match s.read(&mut chunk).await {
                Ok(n) if n > 0 => pending.extend_from_slice(&chunk[..n]),
                _ => return,
            }
        };
        while pending.len() < head_end + length {
            match s.read(&mut chunk).await {
                Ok(n) if n > 0 => pending.extend_from_slice(&chunk[..n]),
                _ => return,
            }
        }
        pending.drain(..head_end + length);
        if s.write_all(REPLY).await.is_err() {
            return;
        }
    }
}

/// A 256 KiB JSON body, different for every `seq`.
fn body(seq: usize) -> Vec<u8> {
    let head = format!("{{\"seq\":{seq},\"blob\":\"");
    let tail = "\"}";
    let pad = BODY - head.len() - tail.len();
    let fill = b'a' + (seq % 26) as u8;
    let mut out = head.into_bytes();
    out.resize(out.len() + pad, fill);
    out.extend_from_slice(tail.as_bytes());
    out
}

async fn post(c: &mut TcpStream, host: &str, path: &str, seq: usize) {
    let body = body(seq);
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);
    c.write_all(&request).await.unwrap();
    let mut got = vec![0u8; REPLY.len()];
    tokio::time::timeout(Duration::from_secs(60), c.read_exact(&mut got))
        .await
        .expect("answered")
        .unwrap();
    assert_eq!(got, REPLY);
}

/// The inbound half: 256 endpoints, 30 distinct 256 KiB exchanges each; `done` counts them.
/// The load pauses after `HOLD_AT` exchanges, still running, until `resume` is set.
async fn inbound_load(discovery: Arc<Discovery>, done: Arc<AtomicUsize>, resume: Arc<AtomicBool>) {
    let app = app().await;
    let inbound =
        common::fixture_discovery(Direction::Inbound, &discovery, move |_| Some(app)).await;
    let mut i = TcpStream::connect(inbound.addr).await.unwrap();
    let mut n = 0;
    for endpoint in 0..256 {
        for _ in 0..30 {
            if n == HOLD_AT {
                while !resume.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            post(&mut i, "svc.ns:8080", &format!("/svc/e{endpoint}"), n).await;
            done.fetch_add(1, Ordering::Relaxed);
            n += 1;
        }
    }
}

// ---- the fake sidecar API (PUL-18's, with one more endpoint and a gate on the second set) ----

fn limits() -> Limits {
    Limits {
        max_literals: 1000,
        max_endpoints: 10_000,
        max_authorities: 64,
        sample_budget: 1 << 30,
    }
}

/// An egress set of just under `CAP` bytes: endpoints of one authority, 20 samples each, with
/// JSON bodies of many small keys, plus the endpoint `POST /blob` of `BLOB_AUTHORITY`, whose
/// only sample has a 256 KiB JSON response body and whose latency is always in a high bucket.
fn big_set() -> Bytes {
    let build = |endpoints: usize| -> String {
        let mut report = Report::new(None);
        let resp = [("content-type", "application/json")];
        let blob = body(0);
        report.record_at(
            ScopeKey::Authority(BLOB_AUTHORITY.into()),
            &Via {
                tls: false,
                sni: None,
                dst: None,
            },
            &limits(),
            &Redactor::default(),
            &Exchange {
                direction: Dir::Outbound,
                port: 8080,
                method: "POST",
                path: "/blob",
                status: 200,
                head_ms: 5000,
                host: None,
                grpc: false,
                observation: Observation {
                    query: "",
                    req_headers: &[],
                    req_body: None,
                    resp_headers: &resp,
                    resp_body: Some(&blob),
                },
            },
        );
        for e in 0..endpoints {
            for s in 0..20 {
                let text = format!(
                    "{{{}}}",
                    (0..100)
                        .map(|k| format!("\"k{k}\":{}", (s * 7 + k) % 10))
                        .collect::<Vec<_>>()
                        .join(",")
                );
                let path = format!("/p{e:05}");
                report.record_at(
                    ScopeKey::Authority("api.example.com:443".into()),
                    &Via {
                        tls: true,
                        sni: Some("api.example.com"),
                        dst: None,
                    },
                    &limits(),
                    &Redactor::default(),
                    &Exchange {
                        direction: Dir::Outbound,
                        port: 8080,
                        method: "GET",
                        path: &path,
                        status: 200,
                        head_ms: 3,
                        host: None,
                        grpc: false,
                        observation: Observation {
                            query: "",
                            req_headers: &[],
                            req_body: None,
                            resp_headers: &resp,
                            resp_body: Some(text.as_bytes()),
                        },
                    },
                );
            }
        }
        EgressSet::project(&report).to_json().unwrap()
    };
    let base = build(0).len();
    assert!(base > BODY, "the blob sample is kept ({base} bytes)");
    let per = (build(20).len() - base) / 20;
    let json = build((CAP - CAP / 20 - base) / per);
    assert!(
        json.len() < CAP && json.len() > CAP / 10 * 9,
        "set of {} bytes for a cap of {CAP}",
        json.len()
    );
    Bytes::from(json)
}

/// The second set: the method of the first endpoint changes, the size does not.
fn second(set: &Bytes) -> (usize, &'static [u8]) {
    let needle: &[u8] = b"\"GET\"";
    let at = set.windows(needle.len()).position(|w| w == needle).unwrap();
    (at, b"\"PUT\"")
}

async fn fake_api(set: Bytes, gate: std::path::PathBuf) -> (SocketAddr, String) {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "controller ca".to_string());
    let ca_key = KeyPair::generate().unwrap();
    let ca_pem = params.self_signed(&ca_key).unwrap().pem();
    let ca = Issuer::new(params, ca_key);
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::default();
    params.subject_alt_names = vec![SanType::IpAddress("127.0.0.1".parse().unwrap())];
    let cert = params.signed_by(&key, &ca).unwrap();
    let config = ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from(cert.der().to_vec())],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
    )
    .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, set, served, gate) =
                (acceptor.clone(), set.clone(), served.clone(), gate.clone());
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let service = service_fn(move |_: Request<Incoming>| {
                    let (set, served, gate) = (set.clone(), served.clone(), gate.clone());
                    async move {
                        // The first pull gets the first set, the first pull after the gate
                        // file exists the second one, the others `304`.
                        let n = served.load(Ordering::SeqCst);
                        let which = match n {
                            0 => Some(0),
                            1 if gate.exists() => Some(1),
                            _ => None,
                        };
                        let Some(which) = which else {
                            let empty = Full::new(Bytes::new()).map_err(|e| match e {}).boxed();
                            return Ok::<_, std::io::Error>(
                                Response::builder().status(304).body(empty).unwrap(),
                            );
                        };
                        served.fetch_add(1, Ordering::SeqCst);
                        let (at, with) = if which == 1 {
                            second(&set)
                        } else {
                            (0, &b""[..])
                        };
                        let etag = ["\"1-first\"", "\"1-second\""][which];
                        let length = set.len();
                        let chunks = futures::stream::unfold(0usize, move |from| {
                            let set = set.clone();
                            async move {
                                if from >= set.len() {
                                    return None;
                                }
                                let to = (from + 64 * 1024).min(set.len());
                                let mut chunk = set.slice(from..to).to_vec();
                                if (from..to).contains(&at) && at + with.len() <= to {
                                    chunk[at - from..at - from + with.len()].copy_from_slice(with);
                                }
                                let frame = Frame::data(Bytes::from(chunk));
                                Some((Ok::<_, std::io::Error>(frame), to))
                            }
                        });
                        let body = StreamBody::new(chunks).boxed();
                        Ok(Response::builder()
                            .status(200)
                            .header("etag", etag)
                            .header("content-length", length)
                            .body(body)
                            .unwrap())
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), service)
                    .await;
            });
        }
    });
    (addr, ca_pem)
}

/// The child process of [`spawn_fake_api`]: the test binary run again on this test, with
/// `MIMIC_TEST_FAKE_API` set to the gate file. It serves until its stdin closes. Without the
/// variable it is a test that does nothing.
#[test]
fn mck_18_fake_api_child_process() {
    use std::io::{Read, Write};
    let Some(gate) = std::env::var_os("MIMIC_TEST_FAKE_API") else {
        return;
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (addr, ca) = runtime.block_on(fake_api(big_set(), gate.into()));
    let mut out = std::io::stdout();
    writeln!(out, "FAKE_API {addr} {}", ca.replace('\n', "|")).unwrap();
    out.flush().unwrap();
    let mut sink = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut sink);
}

struct ApiProcess(std::process::Child);

impl Drop for ApiProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_fake_api(gate: &std::path::Path) -> (ApiProcess, SocketAddr, String) {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    let mut api = ApiProcess(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "mck_18_fake_api_child_process", "--nocapture"])
            .env("MIMIC_TEST_FAKE_API", gate)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("the fake API starts"),
    );
    let stdout = api.0.stdout.take().unwrap();
    for line in BufReader::new(stdout).lines() {
        let line = line.unwrap();
        if let Some(rest) = line.strip_prefix("FAKE_API ") {
            let (addr, ca) = rest.split_once(' ').unwrap();
            return (api, addr.parse().unwrap(), ca.replace('|', "\n"));
        }
    }
    panic!("the fake API did not announce its address");
}

// ---- measures ----

/// Peak resident set size of this process, in bytes.
fn peak_rss() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let kb: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("VmHWM");
    kb * 1024
}

async fn egress_view(admin: SocketAddr) -> serde_json::Value {
    let (status, body) = tokio::task::spawn_blocking(move || http_get(admin, "/egress"))
        .await
        .unwrap();
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

async fn view_etag(admin: SocketAddr, etag: &str) {
    for _ in 0..1500 {
        if egress_view(admin).await["etag"] == etag {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("/egress never showed {etag}");
}

fn counter(metrics: &Metrics, series: &str) -> u64 {
    metrics
        .encode()
        .lines()
        .find(|l| l.starts_with(series))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

async fn until(what: &str, within: Duration, mut ok: impl FnMut() -> bool) {
    let end = Instant::now() + within;
    while !ok() {
        assert!(Instant::now() < end, "{what} not reached in {within:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

struct Sink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn log_lines(logs: &Mutex<Vec<u8>>) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(&logs.lock().unwrap())
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

// ---- the mocked clients: a child process, so that the RSS measured is the sidecar's ----

/// An HTTP/1 client: sends the request and keeps the connection.
async fn h1_client(addr: SocketAddr, payload: Bytes) {
    let Ok(mut s) = TcpStream::connect(addr).await else {
        return;
    };
    let head = format!(
        "POST /blob HTTP/1.1\r\nHost: {BLOB_AUTHORITY}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        payload.len()
    );
    let _ = s.write_all(head.as_bytes()).await;
    let _ = s.write_all(&payload).await;
    let mut buf = [0u8; 256];
    // The overload answer, or nothing for a pending request.
    let _ = s.read(&mut buf).await;
    std::future::pending::<()>().await;
}

/// An `h2c` connection of `H2_STREAMS` streams, kept open.
async fn h2_client(addr: SocketAddr, payload: Bytes) {
    let tcp = TcpStream::connect(addr).await.unwrap();
    let (send, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp))
            .await
            .unwrap();
    tokio::spawn(conn);
    for _ in 0..H2_STREAMS {
        let mut send = send.clone();
        let payload = payload.clone();
        tokio::spawn(async move {
            let request = Request::builder()
                .method("POST")
                .uri(format!("http://{BLOB_AUTHORITY}/blob"))
                .header("content-type", "application/json")
                .body(Full::new(payload))
                .unwrap();
            let _ = send.send_request(request).await;
        });
    }
    std::future::pending::<()>().await;
}

/// The child process of [`spawn_clients`]: the test binary run again on this test, with
/// `MIMIC_TEST_MOCK_CLIENTS` set to the address of the mock listener. It opens the 4 `h2c`
/// connections and the 1 024 HTTP/1 connections and keeps them until it is killed (the
/// clients "abort"). Without the variable it is a test that does nothing.
#[test]
fn mck_18_clients_child_process() {
    use std::io::Write;
    let Some(addr) = std::env::var("MIMIC_TEST_MOCK_CLIENTS")
        .ok()
        .and_then(|v| v.parse::<SocketAddr>().ok())
    else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let payload = Bytes::from(body(1));
        for _ in 0..H2_CONNECTIONS {
            tokio::spawn(h2_client(addr, payload.clone()));
        }
        for _ in 0..H1_CONNECTIONS {
            tokio::spawn(h1_client(addr, payload.clone()));
        }
        let mut out = std::io::stdout();
        writeln!(out, "CLIENTS_STARTED").unwrap();
        out.flush().unwrap();
        std::future::pending::<()>().await;
    });
}

/// Starts the clients in a child process; killing it aborts every client at once.
fn spawn_clients(addr: SocketAddr) -> ApiProcess {
    use std::process::{Command, Stdio};
    ApiProcess(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "mck_18_clients_child_process", "--nocapture"])
            .env("MIMIC_TEST_MOCK_CLIENTS", addr.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .expect("the clients start"),
    )
}

/// The test runs in a child process with the allocator settings the measurement assumes:
/// `MALLOC_ARENA_MAX=2` (as the chart sets it on the controller) and a fixed
/// `MALLOC_MMAP_THRESHOLD_` of 128 KiB. With glibc's defaults the freed 256 KiB buffers raise
/// the mmap threshold and arenas multiply with the worker threads: the heap peaks at 76 MiB
/// while the resident set reaches 140.
const INNER: &str = "MIMIC_TEST_MCK18_INNER";

#[test]
fn mck_18_pending_cap_peak_rss_and_cancellation_with_the_snapshot_and_discovery_held() {
    if std::env::var_os(INNER).is_some() {
        return measured();
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "mck_18_pending_cap_peak_rss_and_cancellation_with_the_snapshot_and_discovery_held",
            "--nocapture",
        ])
        .env(INNER, "1")
        .env("MALLOC_ARENA_MAX", "2")
        .env("MALLOC_MMAP_THRESHOLD_", "131072")
        .status()
        .expect("the measured test starts");
    assert!(status.success(), "the measured test failed: {status}");
}

fn measured() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(measure());
}

async fn measure() {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let (dispatch, _log_handle) = mimic_runtime::logging::build("info", Sink(logs.clone()));
    tracing::dispatcher::set_global_default(dispatch).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let gate = dir.path().join("gate");
    let token = dir.path().join("token");
    std::fs::write(&token, "token").unwrap();
    let (_api, addr, ca) = spawn_fake_api(&gate);

    // The sidecar: mock mode, the snapshot pulled from the fake API, discovery for inbound.
    let mut metrics = Metrics::new("sidecar", "test");
    let mode = ModeState::start(
        &ModeConfig {
            mode: Mode::Mock,
            factor: 10_000,
            ..ModeConfig::default()
        },
        &mut metrics,
    )
    .await;
    let snapshot = Snapshot::new(mode);
    let egress_metrics = EgressMetrics::default();
    egress_metrics.register(&mut metrics);
    let puller = Puller::new(
        EgressConfig {
            interval: Duration::from_millis(300),
            max_bytes: CAP,
            timeout: Duration::from_secs(30),
            target: Some(Target {
                url: format!("https://{addr}"),
                ca,
                token_file: token,
                workload: "pay".to_string(),
            }),
        },
        egress_metrics,
        snapshot.clone(),
    )
    .unwrap()
    .expect("a target");
    let mock = Mock::new(snapshot.clone(), 1024, Redactor::default());
    mock.register(&mut metrics);
    let tunnel = TunnelMetrics::register(&mut metrics);
    let proxy = ProxyMetrics::register(&mut metrics, 50);
    let listener = mimic_sidecar::listener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let mock_addr = listener.local_addr().unwrap();
    let nowhere: SocketAddr = "127.0.0.1:9".parse().unwrap();
    let original: OriginalDst = Arc::new(move |_| Ok(nowhere));
    let shared = Arc::new(
        Shared::new(vec![mock_addr], tunnel, original)
            .with_proxy_metrics(proxy)
            .with_mock(mock),
    );
    spawn_listener(listener, Direction::Outbound, shared);
    let metrics = Arc::new(metrics);
    let admin = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin_addr = admin.local_addr().unwrap();
    let router = snapshot.router();
    tokio::spawn(async move {
        let _ = axum::serve(
            admin,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    let discovery = Arc::new(
        Discovery::start(
            &DiscoveryConfig {
                max_literals: 1000,
                ..DiscoveryConfig::default()
            },
            None,
        )
        .unwrap(),
    );
    tokio::spawn(puller.run(std::future::pending()));
    view_etag(admin_addr, "\"1-first\"").await;
    eprintln!("mck-18 first set held: peak {} MiB", peak_rss() >> 20);

    // The inbound half of the EGR-15 load runs; it pauses after 10% while the mocked
    // requests are pending and the snapshot is swapped, then goes on to its end.
    let done = Arc::new(AtomicUsize::new(0));
    let resume = Arc::new(AtomicBool::new(false));
    let loading = tokio::spawn(inbound_load(discovery, done.clone(), resume.clone()));
    until("inbound load paused", Duration::from_secs(120), || {
        done.load(Ordering::Relaxed) >= HOLD_AT
    })
    .await;

    // 2 048 requests of 256 KiB from a child process.
    eprintln!("mck-18 before the mock load: peak {} MiB", peak_rss() >> 20);
    let mut clients = spawn_clients(mock_addr);
    let overload = "mimic_mock_miss_total{reason=\"overload\"}";
    until("1024 overload answers", Duration::from_secs(120), || {
        counter(&metrics, overload) >= 1024
    })
    .await;
    until("1024 pending", Duration::from_secs(30), || {
        counter(&metrics, "mimic_mock_pending") == PENDING as u64
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(counter(&metrics, "mimic_mock_pending"), PENDING as u64);
    assert_eq!(counter(&metrics, overload), 1024);
    let warns = log_lines(&logs)
        .into_iter()
        .filter(|v| v["message"] == "mock overload")
        .count();
    assert_eq!(warns, 1);
    eprintln!("mck-18 at the cap: peak {} MiB", peak_rss() >> 20);

    // A pull that swaps the snapshot while the answers are pending.
    std::fs::write(&gate, "open").unwrap();
    view_etag(admin_addr, "\"1-second\"").await;
    eprintln!("mck-18 after the second set: peak {} MiB", peak_rss() >> 20);
    assert_eq!(counter(&metrics, "mimic_mock_pending"), PENDING as u64);

    // Every client aborts.
    let _ = clients.0.kill();
    let _ = clients.0.wait();
    let aborted = Instant::now();
    until("pending released", Duration::from_secs(1), || {
        counter(&metrics, "mimic_mock_pending") == 0
    })
    .await;
    eprintln!(
        "mck-18 pending released in {:?}, peak {} MiB",
        aborted.elapsed(),
        peak_rss() >> 20
    );
    // The load goes on.
    resume.store(true, Ordering::Relaxed);
    until("client_abort lines", Duration::from_secs(30), || {
        log_lines(&logs)
            .iter()
            .filter(|v| v["target"] == "access" && v["result"] == "client_abort")
            .count()
            >= PENDING
    })
    .await;
    let lines = log_lines(&logs);
    let by_abort: Vec<_> = lines
        .iter()
        .filter(|v| v["target"] == "access" && v["result"] == "client_abort")
        .collect();
    assert_eq!(by_abort.len(), PENDING);
    // Nothing was written to the aborted clients.
    assert!(
        by_abort
            .iter()
            .all(|v| v["bytes_out"].as_u64().unwrap_or(0) == 0
                && v.get("status").is_none_or(|s| s.is_null()))
    );
    let ended = lines
        .iter()
        .filter(|v| v["message"] == "mock overload ended")
        .count();
    assert_eq!(ended, 1);
    loading.await.unwrap();

    let peak = peak_rss();
    eprintln!("mck-18 peak RSS: {} MiB", peak / (1024 * 1024));
    assert!(
        peak < 120 * 1024 * 1024,
        "peak RSS {} MiB over 120 MiB",
        peak / (1024 * 1024)
    );
}
