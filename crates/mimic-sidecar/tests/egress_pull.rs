//! PUL-13, PUL-14, PUL-15 and PUL-17 on the egress pull client of the sidecar (spec §7.7),
//! against a fake sidecar API served over TLS in the test process.

mod common;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::capture_logs;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use mimic_core::discovery::egress::EgressSet;
use mimic_core::discovery::observed::Observation;
use mimic_core::discovery::redact::Redactor;
use mimic_core::discovery::report::{Direction, Exchange, Limits, Report, Via};
use mimic_core::discovery::scope::ScopeKey;
use mimic_runtime::config::Env;
use mimic_runtime::metrics::Metrics;
use mimic_runtime::testing::{Process, http_get, run};
use mimic_sidecar::discovery::Target;
use mimic_sidecar::egress::{EgressConfig, EgressMetrics, Puller, Snapshot};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

const BIN: &str = env!("CARGO_BIN_EXE_mimic-sidecar");
const TOKEN: &str = "canary-token-5c1e08";
const INTERVAL: Duration = Duration::from_millis(300);
const CANARY_PATH: &str = "/canary-path-9d42";

/// What the fake API does with one pull.
#[derive(Clone, Debug)]
enum Reply {
    /// `200` with this body and `ETag`.
    Ok(String, &'static str),
    NotModified,
    Status(u16),
    /// A status with `Retry-After: <n>`.
    RetryAfter(u16, u32),
    /// The connection is closed without an answer.
    Reset,
    /// No answer for an hour.
    Silent,
    /// `200` with a body of 1 GiB, streamed.
    Endless,
}

struct Seen {
    at: Instant,
    target: String,
    authorization: String,
    if_none_match: Option<String>,
}

struct FakeApi {
    addr: SocketAddr,
    ca_pem: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    /// Bytes of endless bodies handed to the connection.
    sent: Arc<AtomicU64>,
}

impl FakeApi {
    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    fn at(&self, n: usize) -> Instant {
        self.seen.lock().unwrap()[n].at
    }

    fn if_none_match(&self, n: usize) -> Option<String> {
        self.seen.lock().unwrap()[n].if_none_match.clone()
    }

    fn authorization(&self, n: usize) -> String {
        self.seen.lock().unwrap()[n].authorization.clone()
    }

    fn target(&self, n: usize) -> String {
        self.seen.lock().unwrap()[n].target.clone()
    }
}

fn root() -> (Issuer<'static, KeyPair>, String) {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "controller ca".to_string());
    let key = KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    let pem = cert.pem();
    (Issuer::new(params, key), pem)
}

/// A fake API that follows `script`, then answers `fallback` for ever.
async fn fake_api(script: Vec<Reply>, fallback: Reply) -> FakeApi {
    let (ca, ca_pem) = root();
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
    let script: Arc<Mutex<VecDeque<Reply>>> = Arc::new(Mutex::new(script.into()));
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let sent: Arc<AtomicU64> = Arc::default();
    let (script_in, seen_in, sent_in) = (script.clone(), seen.clone(), sent.clone());
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, script, seen, sent, fallback) = (
                acceptor.clone(),
                script_in.clone(),
                seen_in.clone(),
                sent_in.clone(),
                fallback.clone(),
            );
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let service = service_fn(move |req: Request<Incoming>| {
                    let (script, seen, sent, fallback) =
                        (script.clone(), seen.clone(), sent.clone(), fallback.clone());
                    async move {
                        let header = |name: &str| {
                            req.headers()
                                .get(name)
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string)
                        };
                        seen.lock().unwrap().push(Seen {
                            at: Instant::now(),
                            target: req.uri().to_string(),
                            authorization: header("authorization").unwrap_or_default(),
                            if_none_match: header("if-none-match"),
                        });
                        let reply = script.lock().unwrap().pop_front().unwrap_or(fallback);
                        let mut response = Response::builder();
                        let body: BoxBody<Bytes, std::io::Error> = match reply {
                            Reply::Reset => return Err("reset"),
                            Reply::Silent => {
                                tokio::time::sleep(Duration::from_secs(3600)).await;
                                return Err("silent");
                            }
                            Reply::Ok(text, etag) => {
                                response = response.status(200).header("etag", etag);
                                Full::new(Bytes::from(text)).map_err(|e| match e {}).boxed()
                            }
                            Reply::NotModified => {
                                response = response.status(304);
                                Full::new(Bytes::new()).map_err(|e| match e {}).boxed()
                            }
                            Reply::Status(s) => {
                                response = response.status(s);
                                Full::new(Bytes::new()).map_err(|e| match e {}).boxed()
                            }
                            Reply::RetryAfter(s, n) => {
                                response = response.status(s).header("retry-after", n.to_string());
                                Full::new(Bytes::new()).map_err(|e| match e {}).boxed()
                            }
                            Reply::Endless => {
                                response = response.status(200).header("etag", "\"1-endless\"");
                                let chunks = futures::stream::unfold(0u64, move |n| {
                                    let sent = sent.clone();
                                    async move {
                                        const CHUNK: usize = 64 * 1024;
                                        if n >= 1 << 30 {
                                            return None;
                                        }
                                        sent.fetch_add(CHUNK as u64, Ordering::Relaxed);
                                        let frame = Frame::data(Bytes::from(vec![b' '; CHUNK]));
                                        Some((Ok::<_, std::io::Error>(frame), n + CHUNK as u64))
                                    }
                                });
                                StreamBody::new(chunks).boxed()
                            }
                        };
                        Ok(response.body(body).unwrap())
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), service)
                    .await;
            });
        }
    });
    FakeApi {
        addr,
        ca_pem,
        seen,
        sent,
    }
}

fn limits() -> Limits {
    Limits {
        max_literals: 8,
        max_endpoints: 256,
        max_authorities: 64,
        sample_budget: 16 << 20,
    }
}

/// The egress set of GET exchanges on `paths`, with a sample each when `body`.
fn set_json(paths: &[&str], body: bool) -> String {
    let mut report = Report::new(None);
    let text = "{\"id\":\"a\",\"ok\":true}";
    let resp = [("content-type", "application/json")];
    for path in paths {
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
                direction: Direction::Outbound,
                port: 8080,
                method: "GET",
                path,
                status: 200,
                head_ms: 3,
                host: None,
                grpc: false,
                observation: Observation {
                    query: "",
                    req_headers: &[],
                    req_body: None,
                    resp_headers: &resp,
                    resp_body: body.then_some(text.as_bytes()),
                },
            },
        );
    }
    EgressSet::project(&report).to_json().unwrap()
}

fn target(api: &FakeApi, token: &std::path::Path) -> Target {
    Target {
        url: format!("https://{}", api.addr),
        ca: api.ca_pem.clone(),
        token_file: token.to_path_buf(),
        workload: "pay".to_string(),
    }
}

struct Rig {
    api: FakeApi,
    snapshot: Snapshot,
    metrics: Arc<Metrics>,
    token: std::path::PathBuf,
    stop: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    started: Instant,
    _dir: tempfile::TempDir,
}

impl Rig {
    async fn start(script: Vec<Reply>, fallback: Reply) -> Rig {
        Rig::start_with(script, fallback, 4 << 20, Duration::from_secs(30)).await
    }

    async fn start_with(
        script: Vec<Reply>,
        fallback: Reply,
        max_bytes: usize,
        timeout: Duration,
    ) -> Rig {
        let api = fake_api(script, fallback).await;
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, TOKEN).unwrap();
        let config = EgressConfig {
            interval: INTERVAL,
            max_bytes,
            timeout,
            target: Some(target(&api, &token)),
        };
        let snapshot = Snapshot::default();
        let egress_metrics = EgressMetrics::default();
        let mut metrics = Metrics::new("sidecar", "test");
        egress_metrics.register(&mut metrics);
        let puller = Puller::new(config, egress_metrics, snapshot.clone())
            .unwrap()
            .expect("a target");
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            puller
                .run(async move {
                    let _ = stopped.wait_for(|s| *s).await;
                })
                .await;
        });
        Rig {
            api,
            snapshot,
            metrics: Arc::new(metrics),
            token,
            stop,
            task,
            started: Instant::now(),
            _dir: dir,
        }
    }

    fn metric(&self, line: &str) -> Option<f64> {
        self.metrics
            .encode()
            .lines()
            .find(|l| l.starts_with(line))
            .and_then(|l| l.rsplit(' ').next()?.parse().ok())
    }

    fn pulls(&self, result: &str) -> u64 {
        self.metric(&format!("mimic_egress_pulls_total{{result=\"{result}\"}}"))
            .unwrap_or(-1.0) as u64
    }

    async fn stop(self) {
        let _ = self.stop.send(true);
        self.task.await.unwrap();
    }
}

async fn wait(mut done: impl FnMut() -> bool) {
    for _ in 0..1500 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition not reached in 30 s");
}

fn env(pairs: &[(&str, &str)]) -> Env {
    Env::from_pairs(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())))
}

fn ok(body: &str, etag: &'static str) -> Reply {
    Reply::Ok(body.to_string(), etag)
}

#[tokio::test]
async fn pul_13_pulls_with_the_etag_held_and_follows_the_snapshot() {
    let logs = capture_logs();
    let one = set_json(&["/v1/ping"], false);
    let two = set_json(&["/v1/ping", "/v1/pong"], false);
    let empty = r#"{"authorities":{},"format":1}"#;
    let rig = Rig::start(
        vec![
            ok(&one, "\"1-aa\""),
            Reply::NotModified,
            ok(&two, "\"1-bb\""),
        ],
        ok(empty, "\"1-cc\""),
    )
    .await;
    // The first pull comes within one interval, with no `If-None-Match`.
    wait(|| rig.api.count() >= 1).await;
    assert!(rig.api.at(0).duration_since(rig.started) <= INTERVAL + Duration::from_millis(500));
    assert_eq!(rig.api.target(0), "/v1/workloads/pay/egress?format=1");
    assert_eq!(rig.api.if_none_match(0), None);
    assert_eq!(rig.api.authorization(0), format!("Bearer {TOKEN}"));
    // The token file changes between two pulls: the next pull carries the new one.
    std::fs::write(&rig.token, "token-two").unwrap();
    wait(|| rig.api.count() >= 2).await;
    assert_eq!(rig.api.authorization(1), "Bearer token-two");
    assert_eq!(rig.api.if_none_match(1).as_deref(), Some("\"1-aa\""));
    assert!(rig.api.at(1).duration_since(rig.api.at(0)) >= INTERVAL.mul_f32(0.9));
    // `304` keeps the snapshot and moves `pulled`.
    wait(|| rig.pulls("not_modified") == 1).await;
    assert_eq!(rig.snapshot.endpoints(), 1);
    assert_eq!(rig.snapshot.etag().as_deref(), Some("\"1-aa\""));
    assert_eq!(rig.pulls("ok"), 1);
    assert_eq!(rig.metric("mimic_egress_endpoints "), Some(1.0));
    assert_eq!(rig.metric("mimic_egress_bytes "), Some(one.len() as f64));
    assert!(rig.metric("mimic_egress_last_pull_timestamp_seconds ") > Some(1e9));
    // A new `200` replaces it.
    wait(|| rig.snapshot.etag().as_deref() == Some("\"1-bb\"")).await;
    assert_eq!(rig.snapshot.endpoints(), 2);
    assert_eq!(rig.api.if_none_match(2).as_deref(), Some("\"1-aa\""));
    wait(|| rig.metric("mimic_egress_endpoints ") == Some(2.0)).await;
    // An empty set empties it.
    wait(|| rig.snapshot.etag().as_deref() == Some("\"1-cc\"")).await;
    assert_eq!(rig.snapshot.endpoints(), 0);
    assert!(rig.snapshot.pulled().is_some());
    wait(|| rig.metric("mimic_egress_endpoints ") == Some(0.0)).await;
    assert_eq!(rig.metric("mimic_egress_bytes "), Some(empty.len() as f64));
    rig.stop().await;
    logs.settled().await;
    assert!(!logs.text().contains(TOKEN));
}

#[tokio::test]
async fn pul_13_the_metrics_name_the_sidecar_and_exist_from_the_start() {
    let rig = Rig::start(vec![Reply::Silent], Reply::Silent).await;
    let text = rig.metrics.encode();
    for name in [
        "mimic_egress_pulls_total{result=\"ok\"} 0",
        "mimic_egress_pulls_total{result=\"error\"} 0",
        "mimic_egress_endpoints 0",
        "mimic_egress_bytes 0",
        "mimic_egress_last_pull_timestamp_seconds 0",
    ] {
        assert!(text.contains(name), "{name} in\n{text}");
    }
    assert!(
        text.lines()
            .filter(|l| l.starts_with("# HELP mimic_egress_"))
            .all(|l| l.contains("sidecar")),
        "{text}"
    );
    rig.stop().await;
}

/// The attempts after `first` answered `status` come after the backoff of one second.
async fn error_status(reply: Reply) {
    let rig = Rig::start_with(
        vec![reply],
        Reply::NotModified,
        4 << 20,
        Duration::from_millis(500),
    )
    .await;
    wait(|| rig.api.count() >= 2).await;
    let gap = rig.api.at(1).duration_since(rig.api.at(0));
    assert!(gap >= Duration::from_millis(990), "{gap:?}");
    assert_eq!(rig.pulls("error"), 1);
    rig.stop().await;
}

#[tokio::test]
async fn pul_14_503_401_403_429_and_a_reset_are_errors_retried_after_a_second() {
    let logs = capture_logs();
    futures::future::join_all(
        [
            Reply::Status(503),
            Reply::Status(401),
            Reply::Status(403),
            Reply::Status(429),
            Reply::Reset,
        ]
        .into_iter()
        .map(error_status),
    )
    .await;
    logs.settled().await;
    assert!(!logs.text().contains(TOKEN));
}

#[tokio::test]
async fn pul_14_an_answer_past_the_timeout_is_an_error_and_is_retried() {
    let rig = Rig::start_with(
        vec![Reply::Silent],
        Reply::NotModified,
        4 << 20,
        Duration::from_millis(400),
    )
    .await;
    wait(|| rig.api.count() >= 2).await;
    let gap = rig.api.at(1).duration_since(rig.api.at(0));
    assert!(gap >= Duration::from_millis(1300), "{gap:?}");
    assert_eq!(rig.pulls("error"), 1);
    rig.stop().await;
}

#[tokio::test]
async fn pul_14_failures_back_off_and_log_one_warning_then_one_recovery() {
    let logs = capture_logs();
    let rig = Rig::start(
        vec![
            Reply::Status(503),
            Reply::Status(503),
            Reply::Status(503),
            Reply::NotModified,
        ],
        Reply::NotModified,
    )
    .await;
    wait(|| rig.api.count() >= 4).await;
    let gaps: Vec<Duration> = (1..4)
        .map(|n| rig.api.at(n).duration_since(rig.api.at(n - 1)))
        .collect();
    assert!(gaps[0] >= Duration::from_millis(990), "{gaps:?}");
    assert!(gaps[1] >= Duration::from_millis(1990), "{gaps:?}");
    wait(|| rig.pulls("not_modified") >= 1).await;
    assert_eq!(rig.pulls("error"), 3);
    rig.stop().await;
    logs.settled().await;
    let lines: Vec<serde_json::Value> = logs
        .text()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(|v: &serde_json::Value| v["target"] == "egress")
        .collect();
    let failed: Vec<_> = lines
        .iter()
        .filter(|v| v["message"] == "egress pull failed")
        .collect();
    assert_eq!(failed.len(), 1, "{lines:?}");
    assert_eq!(failed[0]["level"], "WARN");
    assert_eq!(failed[0]["status"], 503);
    assert_eq!(failed[0]["result"], "error");
    let recovered: Vec<_> = lines
        .iter()
        .filter(|v| v["message"] == "egress pull recovered")
        .collect();
    assert_eq!(recovered.len(), 1, "{lines:?}");
    assert_eq!(recovered[0]["level"], "INFO");
    assert!(!logs.text().contains(TOKEN));
}

#[tokio::test]
async fn pul_14_retry_after_waits_at_least_that_long() {
    let rig = Rig::start(vec![Reply::RetryAfter(503, 3)], Reply::NotModified).await;
    wait(|| rig.api.count() >= 2).await;
    let gap = rig.api.at(1).duration_since(rig.api.at(0));
    assert!(gap >= Duration::from_millis(2990), "{gap:?}");
    rig.stop().await;
}

#[tokio::test]
async fn pul_14_404_and_405_are_unsupported_and_retried_one_interval_later() {
    for status in [404, 405] {
        let rig = Rig::start(vec![Reply::Status(status)], Reply::Status(status)).await;
        wait(|| rig.api.count() >= 3).await;
        for n in [1, 2] {
            let gap = rig.api.at(n).duration_since(rig.api.at(n - 1));
            assert!(gap >= INTERVAL.mul_f32(0.9), "{status}: {gap:?}");
            assert!(gap < Duration::from_millis(900), "{status}: {gap:?}");
        }
        assert!(rig.pulls("unsupported") >= 2);
        assert_eq!(rig.pulls("error"), 0);
        assert_eq!(rig.snapshot.endpoints(), 0);
        rig.stop().await;
    }
}

#[tokio::test]
async fn pul_14_a_507_is_too_large_and_keeps_the_snapshot() {
    let one = set_json(&["/v1/ping"], false);
    let rig = Rig::start(vec![ok(&one, "\"1-aa\"")], Reply::Status(507)).await;
    wait(|| rig.pulls("too_large") >= 2).await;
    assert_eq!(rig.snapshot.endpoints(), 1);
    assert_eq!(rig.snapshot.etag().as_deref(), Some("\"1-aa\""));
    rig.stop().await;
}

#[tokio::test]
async fn pul_14_a_body_over_the_cap_is_too_large_and_the_read_stops_at_the_cap() {
    let one = set_json(&["/v1/ping"], false);
    let cap = 64 * 1024;
    let rig = Rig::start_with(
        vec![ok(&one, "\"1-aa\""), Reply::Endless],
        Reply::NotModified,
        cap,
        Duration::from_secs(30),
    )
    .await;
    wait(|| rig.pulls("too_large") == 1).await;
    // The old snapshot is untouched, and the fake was not drained to its end.
    assert_eq!(rig.snapshot.etag().as_deref(), Some("\"1-aa\""));
    assert_eq!(rig.snapshot.endpoints(), 1);
    assert!(
        rig.api.sent.load(Ordering::Relaxed) < cap as u64 + (8 << 20),
        "{}",
        rig.api.sent.load(Ordering::Relaxed)
    );
    rig.stop().await;
}

#[tokio::test]
async fn pul_14_a_refused_body_is_invalid_with_one_warning_and_leaves_the_snapshot() {
    let logs = capture_logs();
    let good = set_json(&["/v1/ping", CANARY_PATH], false);
    let format2 = good.replace("\"format\":1", "\"format\":2");
    let mut with_sample: serde_json::Value =
        serde_json::from_str(&set_json(&["/v1/ping"], true)).unwrap();
    let endpoint = &mut with_sample["authorities"]["api.example.com:443"]["endpoints"][0];
    let sample = endpoint["samples"][0].clone();
    assert!(sample.is_object(), "{with_sample}");
    endpoint["samples"] = serde_json::Value::Array(vec![sample; 21]);
    let rig = Rig::start(
        vec![
            ok(&good, "\"1-aa\""),
            ok(&format2, "\"1-bb\""),
            ok("this is not json", "\"1-cc\""),
            ok(&with_sample.to_string(), "\"1-dd\""),
        ],
        Reply::NotModified,
    )
    .await;
    wait(|| rig.pulls("invalid") == 3).await;
    assert_eq!(rig.snapshot.etag().as_deref(), Some("\"1-aa\""));
    assert_eq!(rig.snapshot.endpoints(), 2);
    // Each refused body is answered at the next interval, not the backoff.
    assert_eq!(rig.pulls("error"), 0);
    rig.stop().await;
    logs.settled().await;
    let rejected: Vec<serde_json::Value> = logs
        .text()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(|v: &serde_json::Value| v["message"] == "egress rejected")
        .collect();
    let reasons: Vec<&str> = rejected
        .iter()
        .map(|v| v["reason"].as_str().unwrap())
        .collect();
    assert_eq!(
        reasons,
        ["format", "malformed", "malformed"],
        "{rejected:?}"
    );
    assert!(rejected.iter().all(|v| v["level"] == "WARN"));
    let text = logs.text();
    assert!(!text.contains(TOKEN));
    assert!(!text.contains(CANARY_PATH), "{text}");
}

#[test]
fn pul_15_without_a_controller_url_nothing_is_pulled_and_one_line_says_so() {
    let mut process = Process::spawn(
        BIN,
        &[
            ("MIMIC_ADMIN_ADDR", "127.0.0.1:0"),
            ("MIMIC_INBOUND_ADDRS", "127.0.0.1:0"),
            ("MIMIC_OUTBOUND_ADDRS", "127.0.0.1:0"),
        ],
    );
    let line = process.wait_log(Duration::from_secs(10), |v| {
        v["message"] == "egress pull disabled"
    });
    assert_eq!(line["level"], "INFO");
    process.terminate();
    assert!(process.wait_exit(Duration::from_secs(10)).success());
    let n = process
        .logs()
        .iter()
        .filter(|v| v["message"] == "egress pull disabled")
        .count();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn pul_15_without_a_target_the_puller_is_not_built() {
    let config = EgressConfig {
        interval: INTERVAL,
        max_bytes: 4 << 20,
        timeout: Duration::from_secs(1),
        target: None,
    };
    let puller = Puller::new(config, EgressMetrics::default(), Snapshot::default()).unwrap();
    assert!(puller.is_none());
}

#[tokio::test]
async fn pul_15_no_pull_starts_after_the_stop_and_a_pull_in_flight_is_abandoned() {
    let rig = Rig::start(vec![], Reply::NotModified).await;
    wait(|| rig.api.count() >= 2).await;
    let api = rig.api.seen.clone();
    let _ = rig.stop.send(true);
    // The puller ends at once, whatever it was doing.
    tokio::time::timeout(Duration::from_secs(1), rig.task)
        .await
        .unwrap()
        .unwrap();
    let after = api.lock().unwrap().len();
    tokio::time::sleep(INTERVAL * 4).await;
    assert_eq!(api.lock().unwrap().len(), after);

    let rig = Rig::start(vec![], Reply::Silent).await;
    wait(|| rig.api.count() >= 1).await;
    let begun = Instant::now();
    let _ = rig.stop.send(true);
    tokio::time::timeout(Duration::from_secs(1), rig.task)
        .await
        .unwrap()
        .unwrap();
    assert!(begun.elapsed() < Duration::from_secs(1));
}

fn sidecar(api: &FakeApi, dir: &std::path::Path, extra: &[(&str, &str)]) -> Process {
    let token = dir.join("token");
    std::fs::write(&token, TOKEN).unwrap();
    let url = format!("https://{}", api.addr);
    let mut env = vec![
        ("MIMIC_ADMIN_ADDR", "127.0.0.1:0"),
        ("MIMIC_INBOUND_ADDRS", "127.0.0.1:0"),
        ("MIMIC_OUTBOUND_ADDRS", "127.0.0.1:0"),
        ("MIMIC_CONTROLLER_URL", &url),
        ("MIMIC_CONTROLLER_CA", &api.ca_pem),
        ("MIMIC_TLS_TOKEN_FILE", token.to_str().unwrap()),
        ("MIMIC_WORKLOAD", "pay"),
        ("MIMIC_EGRESS_PULL_INTERVAL", "1s"),
    ];
    env.extend_from_slice(extra);
    let mut process = Process::spawn(BIN, &env);
    let admin = process.admin_addr();
    mimic_runtime::testing::wait_status(admin, "/readyz", 200, Duration::from_secs(10));
    process
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pul_15_sigterm_with_a_silent_api_exits_within_the_bound() {
    let dir = tempfile::tempdir().unwrap();
    let api = fake_api(vec![], Reply::Silent).await;
    let mut process = sidecar(&api, dir.path(), &[]);
    wait(|| api.count() >= 1).await;
    let begun = Instant::now();
    process.terminate();
    let status = process.wait_exit(Duration::from_secs(10));
    assert!(status.success(), "{status}");
    assert!(
        begun.elapsed() < Duration::from_secs(3),
        "{:?}",
        begun.elapsed()
    );
    assert_eq!(api.count(), 1);
}

/// Peak resident set size of the process `pid`, in bytes.
fn peak_rss(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let kb: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("VmHWM");
    kb * 1024
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pul_14_a_one_gib_body_is_read_up_to_the_cap_only() {
    let dir = tempfile::tempdir().unwrap();
    let api = fake_api(vec![], Reply::Endless).await;
    // The first pull comes at a random time within one interval.
    let mut process = sidecar(&api, dir.path(), &[("MIMIC_EGRESS_PULL_INTERVAL", "5s")]);
    let before = peak_rss(process.pid());
    let admin = process.admin_addr();
    let counted = || {
        http_get(admin, "/metrics").1.lines().any(|l| {
            l.starts_with("mimic_egress_pulls_total{result=\"too_large\"}") && !l.ends_with(" 0")
        })
    };
    for _ in 0..600 {
        if tokio::task::block_in_place(counted) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(counted(), "no too_large pull");
    let after = peak_rss(process.pid());
    let cap = 4u64 << 20;
    assert!(after < before + 2 * cap, "{before} -> {after}");
    assert!(api.sent.load(Ordering::Relaxed) < cap + (16 << 20));
    process.terminate();
    assert!(process.wait_exit(Duration::from_secs(10)).success());
}

#[test]
fn pul_17_defaults_are_60s_and_4mi() {
    let config = EgressConfig::from_env(&env(&[])).unwrap();
    assert_eq!(config.interval, Duration::from_secs(60));
    assert_eq!(config.max_bytes, 4 << 20);
    assert_eq!(config.timeout, Duration::from_secs(30));
    assert!(config.target.is_none());
}

#[test]
fn pul_17_values_are_read_and_bounded() {
    let config = EgressConfig::from_env(&env(&[
        ("MIMIC_EGRESS_PULL_INTERVAL", "1s"),
        ("MIMIC_EGRESS_MAX_BYTES", "64Ki"),
    ]))
    .unwrap();
    assert_eq!(config.interval, Duration::from_secs(1));
    assert_eq!(config.max_bytes, 64 * 1024);
    let config = EgressConfig::from_env(&env(&[
        ("MIMIC_EGRESS_PULL_INTERVAL", "1h"),
        ("MIMIC_EGRESS_MAX_BYTES", "1024Mi"),
    ]))
    .unwrap();
    assert_eq!(config.interval, Duration::from_secs(3600));
    assert_eq!(config.max_bytes, 1 << 30);
    for (var, value) in [
        ("MIMIC_EGRESS_PULL_INTERVAL", "0s"),
        ("MIMIC_EGRESS_PULL_INTERVAL", "2h"),
        ("MIMIC_EGRESS_PULL_INTERVAL", "abc"),
        ("MIMIC_EGRESS_MAX_BYTES", "63Ki"),
        ("MIMIC_EGRESS_MAX_BYTES", "2Gi"),
        ("MIMIC_EGRESS_MAX_BYTES", "1025Mi"),
        ("MIMIC_EGRESS_MAX_BYTES", "abc"),
    ] {
        let e = EgressConfig::from_env(&env(&[(var, value)])).unwrap_err();
        assert!(e.to_string().contains(var), "{var}={value}: {e}");
    }
}

#[test]
fn pul_17_the_binary_exits_2_on_an_invalid_value() {
    for (var, value) in [
        ("MIMIC_EGRESS_PULL_INTERVAL", "0s"),
        ("MIMIC_EGRESS_PULL_INTERVAL", "2h"),
        ("MIMIC_EGRESS_PULL_INTERVAL", "abc"),
        ("MIMIC_EGRESS_MAX_BYTES", "63Ki"),
        ("MIMIC_EGRESS_MAX_BYTES", "2Gi"),
        ("MIMIC_EGRESS_MAX_BYTES", "abc"),
    ] {
        let (status, _, stderr) = run(
            BIN,
            &[],
            &[("MIMIC_ADMIN_ADDR", "127.0.0.1:0"), (var, value)],
        );
        assert_eq!(status.code(), Some(2), "{var}={value}: {stderr}");
        assert!(stderr.contains(var), "{var}={value}: {stderr}");
    }
}

#[test]
fn pul_17_gi_is_a_size_unit_and_1gi_is_the_maximum() {
    let config = EgressConfig::from_env(&env(&[("MIMIC_EGRESS_MAX_BYTES", "1Gi")])).unwrap();
    assert_eq!(config.max_bytes, 1 << 30);
    for value in ["2Gi", "0Gi", "Gi", "1.5Gi"] {
        let e = EgressConfig::from_env(&env(&[("MIMIC_EGRESS_MAX_BYTES", value)])).unwrap_err();
        assert!(
            e.to_string().contains("MIMIC_EGRESS_MAX_BYTES"),
            "{value}: {e}"
        );
    }
}

const CANARY_BODY: &str = "canary-sample-7b3e1f";
const EMPTY_VIEW: &str =
    r#"{"authorities":{},"etag":null,"latency_factor":1,"mode":"discovery","pulled":null}"#;

/// A set whose samples hold `CANARY_BODY`.
fn canary_set() -> String {
    let mut report = Report::new(None);
    let text = format!("{{\"id\":\"{CANARY_BODY}\"}}");
    let resp = [("content-type", "application/json")];
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
            direction: Direction::Outbound,
            port: 8080,
            method: "GET",
            path: "/v1/ping",
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
    let json = EgressSet::project(&report).to_json().unwrap();
    assert!(json.contains(CANARY_BODY), "the set holds the canary");
    json
}

#[test]
fn pul_16_a_loopback_peer_gets_the_summary_and_no_canary_leaves_the_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let api = rt.block_on(fake_api(
        vec![Reply::Status(503), ok(&canary_set(), "\"1-aa\"")],
        Reply::NotModified,
    ));
    let mut process = sidecar(&api, dir.path(), &[]);
    let admin = process.admin_addr();
    // Before the first answer: no snapshot.
    let (status, body) = http_get(admin, "/egress");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, EMPTY_VIEW);
    let mut view = serde_json::Value::Null;
    for _ in 0..600 {
        let (status, body) = http_get(admin, "/egress");
        assert_eq!(status, 200, "{body}");
        view = serde_json::from_str(&body).unwrap();
        if view["etag"] == "\"1-aa\"" {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(view["etag"], "\"1-aa\"", "{view}");
    assert!(
        view["pulled"].as_str().is_some_and(|t| t.contains('T')),
        "{view}"
    );
    let endpoint = &view["authorities"]["api.example.com:443"]["endpoints"][0];
    assert_eq!(endpoint["method"], "GET");
    assert_eq!(endpoint["template"], "/v1/ping");
    assert_eq!(endpoint["exchanges"], 1);
    assert_eq!(endpoint["statuses"]["200"], 1);
    assert!(endpoint.get("samples").is_none(), "{view}");
    let (_, body) = http_get(admin, "/egress");
    let (_, metrics) = http_get(admin, "/metrics");
    process.terminate();
    assert!(process.wait_exit(Duration::from_secs(10)).success());
    let stdout = process.lines().join("\n");
    for (what, text) in [("/egress", body), ("/metrics", metrics), ("stdout", stdout)] {
        assert!(!text.contains(CANARY_BODY), "{what} holds the canary");
    }
}

#[test]
fn pul_16_a_non_loopback_peer_gets_403() {
    use std::net::{IpAddr, Ipv4Addr, UdpSocket};
    let dir = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let api = rt.block_on(fake_api(vec![], Reply::Status(503)));
    let mut process = sidecar(&api, dir.path(), &[("MIMIC_ADMIN_ADDR", "0.0.0.0:0")]);
    let port = process.admin_addr().port();
    let (status, _) = http_get(SocketAddr::from(([127, 0, 0, 1], port)), "/egress");
    assert_eq!(status, 200);
    // The address a route to a public destination would leave from.
    let pod = UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9").map(|()| s))
        .and_then(|s| s.local_addr())
        .map(|a| a.ip())
        .ok()
        .filter(|ip| !ip.is_loopback() && *ip != IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    match pod {
        Some(ip) => {
            let (status, body) = http_get(SocketAddr::new(ip, port), "/egress");
            assert_eq!(status, 403, "{body}");
            assert!(!body.contains("authorities"), "{body}");
        }
        None => eprintln!("no non-loopback address: 403 covered by the in-process test"),
    }
    process.terminate();
    assert!(process.wait_exit(Duration::from_secs(10)).success());
}

#[tokio::test]
async fn pul_16_the_router_refuses_a_non_loopback_peer_in_process() {
    use axum::extract::ConnectInfo;
    use tower::ServiceExt;
    let get = |peer: &str| {
        let mut request = axum::http::Request::get("/egress")
            .body(axum::body::Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        Snapshot::default().router().oneshot(request)
    };
    for peer in ["10.1.2.3:4000", "[2001:db8::1]:4000"] {
        let response = get(peer).await.unwrap();
        assert_eq!(response.status(), 403, "{peer}");
    }
    for peer in ["127.0.0.1:4000", "[::1]:4000", "[::ffff:127.0.0.1]:4000"] {
        let response = get(peer).await.unwrap();
        assert_eq!(response.status(), 200, "{peer}");
        let body = BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(body, EMPTY_VIEW);
    }
}

/// An app that answers every request with the same fixed response.
async fn fixed_app() -> SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                while let Ok(n) = s.read(&mut buf).await {
                    let reply: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"ok\":true}";
                    if n == 0 || s.write_all(reply).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

fn untimed(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(o) => {
            o.remove("head_ms");
            o.remove("latency");
            o.values_mut().for_each(untimed);
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(untimed),
        _ => {}
    }
}

/// One outbound call through the proxy: the raw answer, the projection of what discovery
/// recorded and the access lines without what varies from one run to the next.
async fn one_call(held: Option<&str>) -> (Vec<u8>, String, Vec<serde_json::Value>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let logs = capture_logs();
    let rig = match held {
        Some(set) => {
            let rig = Rig::start(vec![ok(set, "\"1-aa\"")], Reply::NotModified).await;
            wait(|| rig.snapshot.endpoints() == 1).await;
            Some(rig)
        }
        None => None,
    };
    let app = fixed_app().await;
    let discovery = mimic_sidecar::discovery::Discovery::start(
        &mimic_sidecar::discovery::DiscoveryConfig::default(),
        None,
    )
    .unwrap();
    let proxy = common::fixture_discovery(
        mimic_sidecar::listener::Direction::Outbound,
        &discovery,
        move |_| Some(app),
    )
    .await;
    let mut c = tokio::net::TcpStream::connect(proxy.addr).await.unwrap();
    c.write_all(b"GET /v1/ping HTTP/1.1\r\nHost: api.example.com:443\r\n\r\n")
        .await
        .unwrap();
    let mut answer = vec![0u8; 256];
    let mut got = 0;
    while !answer[..got].ends_with(b"{\"ok\":true}") {
        let n = c.read(&mut answer[got..]).await.unwrap();
        assert!(n > 0, "closed after {got} bytes");
        got += n;
    }
    answer.truncate(got);
    let lines = logs.access_exactly(1).await;
    let recorded = EgressSet::project(&discovery.report()).to_json().unwrap();
    // Timings aside.
    let mut recorded: serde_json::Value = serde_json::from_str(&recorded).unwrap();
    untimed(&mut recorded);
    let recorded = recorded.to_string();
    if let Some(rig) = rig {
        rig.stop().await;
    }
    let steady = lines
        .into_iter()
        .map(|mut l| {
            if let Some(o) = l.as_object_mut() {
                o.retain(|k, _| {
                    !["time", "duration", "latency", "src", "dst", "peer", "ms"]
                        .iter()
                        .any(|v| k.contains(v))
                });
            }
            l
        })
        .collect();
    (answer, recorded, steady)
}

#[tokio::test]
async fn pul_15_a_held_snapshot_changes_no_traffic() {
    let set = set_json(&["/v1/ping"], true);
    let (plain, plain_recorded, plain_lines) = one_call(None).await;
    let (held, held_recorded, held_lines) = one_call(Some(&set)).await;
    let text = String::from_utf8_lossy(&held).to_ascii_lowercase();
    assert!(text.starts_with("http/1.1 200"), "{text}");
    assert!(!text.contains("x-mimic-match"), "{text}");
    assert_eq!(plain, held);
    assert_eq!(plain_recorded, held_recorded);
    assert!(plain_recorded.contains("/v1/ping"), "{plain_recorded}");
    assert_eq!(plain_lines, held_lines);
}
