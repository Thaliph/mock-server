//! PUL-18: the sidecar's memory with the EGR-15 worst case of discovery and egress pulls of
//! just under `MIMIC_EGRESS_MAX_BYTES`. One test only, in its own process: the peak resident
//! set size it checks must not move with other tests.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use mimic_core::discovery::egress::EgressSet;
use mimic_core::discovery::observed::Observation;
use mimic_core::discovery::redact::Redactor;
use mimic_core::discovery::report::{Direction as Dir, Exchange, Limits, Report, Via};
use mimic_core::discovery::scope::ScopeKey;
use mimic_runtime::metrics::Metrics;
use mimic_runtime::testing::http_get;
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig, Target};
use mimic_sidecar::egress::{EgressConfig, EgressMetrics, Puller, Snapshot};
use mimic_sidecar::listener::Direction;
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
/// Exchanges of the load, and the share of them that comes before the pulls: the pulls run
/// when discovery is at its worst case, the load not yet over.
const TOTAL: usize = 2 * 256 * 30;
const PULLS_FROM: usize = TOTAL / 10 * 9;
const REPLY: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"ok\":true}";

// ---- the EGR-15 load (copied from discovery_memory_egress.rs) ----

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
    tokio::time::timeout(Duration::from_secs(30), c.read_exact(&mut got))
        .await
        .expect("answered")
        .unwrap();
    assert_eq!(got, REPLY);
}

fn recorded_count(text: &str, label: &str) -> u64 {
    text.lines()
        .filter(|l| l.starts_with("mimic_discovery_exchanges_total") && l.contains(label))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .map(|v| v as u64)
        .sum()
}

async fn recorded(metrics: &Metrics, label: &str, n: u64) {
    for _ in 0..1500 {
        if recorded_count(&metrics.encode(), label) >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{n} x {label} not reached:\n{}", metrics.encode());
}

/// 256 outbound and 256 inbound endpoints over 64 authorities, 30 exchanges each, `TOTAL` in
/// all; `done` counts the exchanges answered.
async fn load(discovery: Arc<Discovery>, done: Arc<AtomicUsize>) {
    let app = app().await;
    let out = common::fixture_discovery(Direction::Outbound, &discovery, move |_| Some(app)).await;
    let inbound =
        common::fixture_discovery(Direction::Inbound, &discovery, move |_| Some(app)).await;
    let mut c = TcpStream::connect(out.addr).await.unwrap();
    let mut seq = 0;
    for path in 0..4 {
        for authority in 0..64 {
            for _ in 0..30 {
                let host = format!("a{authority}.example");
                post(&mut c, &host, &format!("/p{path}"), seq).await;
                done.fetch_add(1, Ordering::Relaxed);
                seq += 1;
            }
            recorded(
                &out.metrics,
                "direction=\"outbound\",result=\"recorded\"",
                seq as u64,
            )
            .await;
        }
    }
    let mut i = TcpStream::connect(inbound.addr).await.unwrap();
    let mut n = 0;
    for endpoint in 0..256 {
        for _ in 0..30 {
            post(&mut i, "svc.ns:8080", &format!("/svc/e{endpoint}"), n).await;
            done.fetch_add(1, Ordering::Relaxed);
            n += 1;
        }
        recorded(
            &inbound.metrics,
            "direction=\"inbound\",result=\"recorded\"",
            n as u64,
        )
        .await;
    }
}

// ---- the fake sidecar API ----

fn limits() -> Limits {
    Limits {
        max_literals: 1000,
        max_endpoints: 10_000,
        max_authorities: 64,
        sample_budget: 1 << 30,
    }
}

/// An egress set of just under `CAP` bytes: endpoints of one authority, 20 samples each, with
/// JSON bodies of many small keys, the costliest to parse.
fn big_set() -> Bytes {
    let build = |endpoints: usize| -> String {
        let mut report = Report::new(None);
        let resp = [("content-type", "application/json")];
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
    let per = build(20).len() / 20;
    let json = build((CAP - CAP / 20) / per);
    assert!(
        json.len() < CAP && json.len() > CAP / 10 * 9,
        "set of {} bytes for a cap of {CAP}",
        json.len()
    );
    Bytes::from(json)
}

/// What the fake does with the n-th pull.
#[derive(Clone, Copy)]
enum Variant {
    /// The set as built.
    First,
    /// Another set of the same size: the method of the first endpoint changes.
    Second,
    /// The same size, refused after the full parse: two endpoints of one name.
    Refused,
}

fn patch(set: &Bytes, variant: Variant) -> (usize, &'static [u8]) {
    let find = |needle: &[u8]| set.windows(needle.len()).position(|w| w == needle).unwrap();
    match variant {
        Variant::First => (0, b""),
        Variant::Second => (find(b"\"GET\""), b"\"PUT\""),
        Variant::Refused => {
            let at = find(b"\"template\":\"/p00001\"");
            (at + 11, b"\"/p00000\"")
        }
    }
}

async fn fake_api(set: Bytes) -> (SocketAddr, String, Arc<AtomicUsize>) {
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
    let pulls = Arc::new(AtomicUsize::new(0));
    let served = pulls.clone();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, set, pulls) = (acceptor.clone(), set.clone(), served.clone());
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let service = service_fn(move |_: Request<Incoming>| {
                    let (set, n) = (set.clone(), pulls.fetch_add(1, Ordering::SeqCst));
                    async move {
                        let variant = [Variant::First, Variant::Second, Variant::Refused].get(n);
                        let Some(variant) = variant else {
                            let empty = Full::new(Bytes::new()).map_err(|e| match e {}).boxed();
                            return Ok::<_, std::io::Error>(
                                Response::builder().status(304).body(empty).unwrap(),
                            );
                        };
                        let (at, with) = patch(&set, *variant);
                        let etag = ["\"1-first\"", "\"1-second\"", "\"1-refused\""][n];
                        let length = set.len();
                        // Streamed in chunks: the fake holds the set once, never a copy.
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
                        let body: BoxBody<Bytes, std::io::Error> = StreamBody::new(chunks).boxed();
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
    (addr, ca_pem, pulls)
}

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

async fn view_etag(admin: SocketAddr, etag: &str) -> serde_json::Value {
    for _ in 0..1500 {
        let view = egress_view(admin).await;
        if view["etag"] == etag {
            return view;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("/egress never showed {etag}");
}

/// The child process of [`spawn_fake_api`]: the test binary run again on this test, with
/// `MIMIC_TEST_FAKE_API` set. It serves until its stdin closes. Without the variable it is a
/// test that does nothing.
#[test]
fn pul_18_fake_api_child_process() {
    use std::io::{Read, Write};
    if std::env::var_os("MIMIC_TEST_FAKE_API").is_none() {
        return;
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (addr, ca, _) = runtime.block_on(fake_api(big_set()));
    let mut out = std::io::stdout();
    writeln!(out, "FAKE_API {addr} {}", ca.replace('\n', "|")).unwrap();
    out.flush().unwrap();
    let mut sink = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut sink);
}

/// Starts the fake API in a child process; dropping the handle stops it.
fn spawn_fake_api() -> (ApiProcess, SocketAddr, String) {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "pul_18_fake_api_child_process", "--nocapture"])
        .env("MIMIC_TEST_FAKE_API", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the fake API starts");
    let stdout = child.stdout.take().unwrap();
    for line in BufReader::new(stdout).lines() {
        let line = line.unwrap();
        if let Some(rest) = line.strip_prefix("FAKE_API ") {
            let (addr, ca) = rest.split_once(' ').unwrap();
            return (
                ApiProcess(child),
                addr.parse().unwrap(),
                ca.replace('|', "\n"),
            );
        }
    }
    panic!("the fake API did not announce its address");
}

struct ApiProcess(std::process::Child);

impl Drop for ApiProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pul_18_discovery_worst_case_with_two_pulls_and_a_refused_body_stays_under_120_mib() {
    // The fake API runs in a child process: this one holds what the sidecar holds (puller,
    // snapshot, discovery) and the load's client and app side, never the sets being served.
    let (_api, addr, ca) = spawn_fake_api();
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "token").unwrap();
    let config = EgressConfig {
        interval: Duration::from_millis(300),
        max_bytes: CAP,
        timeout: Duration::from_secs(30),
        target: Some(Target {
            url: format!("https://{addr}"),
            ca,
            token_file: token,
            workload: "pay".to_string(),
        }),
    };
    let snapshot = Snapshot::default();
    let egress_metrics = EgressMetrics::default();
    let mut metrics = Metrics::new("sidecar", "test");
    egress_metrics.register(&mut metrics);
    let puller = Puller::new(config, egress_metrics, snapshot.clone())
        .unwrap()
        .expect("a target");
    let admin = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin_addr = admin.local_addr().unwrap();
    tokio::spawn(async move {
        let router = snapshot.router();
        let _ = axum::serve(
            admin,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    let discovery_config = DiscoveryConfig {
        max_literals: 1000,
        ..DiscoveryConfig::default()
    };
    let discovery = Arc::new(Discovery::start(&discovery_config, None).unwrap());
    let metrics = Arc::new(metrics);

    let done = Arc::new(AtomicUsize::new(0));
    let loading = tokio::spawn(load(discovery.clone(), done.clone()));
    // The pulls start when 90% of the exchanges are recorded.
    for _ in 0..6000 {
        if done.load(Ordering::Relaxed) >= PULLS_FROM {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let level = |label: &str, done: &AtomicUsize| {
        let n = done.load(Ordering::Relaxed);
        eprintln!("pul-18 load at {label}: {n}/{TOTAL}");
        n
    };
    assert!(level("start of the pulls", &done) >= PULLS_FROM);
    tokio::spawn(puller.run(std::future::pending()));

    // After each accepted pull `/egress` shows the new set, the load still running.
    let first = view_etag(admin_addr, "\"1-first\"").await;
    assert!(level("first set", &done) >= PULLS_FROM);
    let second = view_etag(admin_addr, "\"1-second\"").await;
    assert!(level("second set", &done) >= PULLS_FROM);
    assert_ne!(first["authorities"], second["authorities"]);
    // The third body is refused, and the second set is still held.
    let refused = "mimic_egress_pulls_total{result=\"invalid\"} 1";
    for _ in 0..1500 {
        if metrics.encode().contains(refused) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(metrics.encode().contains(refused), "{}", metrics.encode());
    assert!(
        level("refused body", &done) < TOTAL,
        "the load ended before the refused pull"
    );
    assert_eq!(egress_view(admin_addr).await["etag"], "\"1-second\"");
    assert!(!loading.is_finished());
    assert!(
        metrics
            .encode()
            .contains("mimic_egress_pulls_total{result=\"ok\"} 2"),
        "{}",
        metrics.encode()
    );
    loading.await.unwrap();

    let peak = peak_rss();
    eprintln!("pul-18 peak RSS: {} MiB", peak / (1024 * 1024));
    assert!(
        peak < 120 * 1024 * 1024,
        "peak RSS {} MiB over 120 MiB",
        peak / (1024 * 1024)
    );
}
