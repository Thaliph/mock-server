//! CTL-14 (push interval), CTL-15, CTL-16 and CTL-17 on the shipping client of the sidecar
//! (spec §7.2), against a fake sidecar API served over TLS in the test process.

mod common;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{capture_logs, fixture_discovery};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use mimic_core::discovery::report::Report;
use mimic_runtime::config::Env;
use mimic_runtime::metrics::Metrics;
use mimic_runtime::testing::{Process, run};
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig, ShipConfig, Shipper, Target};
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

const BIN: &str = env!("CARGO_BIN_EXE_mimic-sidecar");
const TOKEN: &str = "canary-token-7f3a91";

/// What the fake API does with one post.
#[derive(Clone, Copy, Debug)]
enum Reply {
    Ok,
    Status(u16),
    /// A status with `Retry-After: <n>`.
    RetryAfter(u16, u32),
    /// The connection is closed without an answer.
    Reset,
    /// No answer for an hour.
    Silent,
}

struct Seen {
    at: Instant,
    authorization: String,
    path: String,
    body: Vec<u8>,
    /// The status the fake answered, 0 when it did not.
    status: u16,
}

impl Seen {
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    fn sequence(&self) -> u64 {
        self.json()["sequence"].as_u64().unwrap()
    }

    fn session(&self) -> String {
        self.json()["session"].as_str().unwrap().to_string()
    }

    fn report(&self) -> Report {
        Report::from_json(&self.json()["report"].to_string()).unwrap()
    }
}

struct FakeApi {
    addr: SocketAddr,
    ca_pem: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl FakeApi {
    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    /// `(sequence, status, exchanges)` of every post so far.
    fn posts(&self) -> Vec<(u64, u16, u64)> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|s| (s.sequence(), s.status, s.report().exchanges()))
            .collect()
    }

    /// Exchanges of the posts the fake acknowledged.
    fn acknowledged(&self) -> u64 {
        self.posts()
            .iter()
            .filter(|(_, status, _)| *status == 200)
            .map(|(_, _, n)| n)
            .sum()
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
    (Issuer::new(params, key), cert.pem())
}

async fn fake_api(script: Vec<Reply>) -> FakeApi {
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
    let (script_in, seen_in) = (script.clone(), seen.clone());
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, script, seen) = (acceptor.clone(), script_in.clone(), seen_in.clone());
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let service = service_fn(move |req: Request<Incoming>| {
                    let (script, seen) = (script.clone(), seen.clone());
                    async move {
                        let authorization = req
                            .headers()
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        let path = req.uri().path().to_string();
                        let body = req.into_body().collect().await.unwrap().to_bytes().to_vec();
                        let reply = script.lock().unwrap().pop_front().unwrap_or(Reply::Ok);
                        let (status, retry_after) = match reply {
                            Reply::Ok => (200, None),
                            Reply::Status(s) => (s, None),
                            Reply::RetryAfter(s, n) => (s, Some(n)),
                            Reply::Reset | Reply::Silent => (0, None),
                        };
                        seen.lock().unwrap().push(Seen {
                            at: Instant::now(),
                            authorization,
                            path,
                            body,
                            status,
                        });
                        match reply {
                            Reply::Reset => Err("reset"),
                            Reply::Silent => {
                                tokio::time::sleep(Duration::from_secs(3600)).await;
                                Err("silent")
                            }
                            _ => {
                                let mut response = Response::builder().status(status);
                                if let Some(n) = retry_after {
                                    response = response.header("retry-after", n.to_string());
                                }
                                Ok(response
                                    .body(Full::new(Bytes::from_static(b"{\"applied\":true}")))
                                    .unwrap())
                            }
                        }
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), service)
                    .await;
            });
        }
    });
    FakeApi { addr, ca_pem, seen }
}

/// An app answering every request with `200 {}`.
async fn app() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf).await {
                        Ok(n) if n > 0 => head.extend_from_slice(&buf[..n]),
                        _ => return,
                    }
                }
                let body = "{\"ok\":true,\"name\":\"user\"}";
                let _ = s
                    .write_all(
                        format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                            .as_bytes(),
                    )
                    .await;
            });
        }
    });
    addr
}

struct Rig {
    api: FakeApi,
    discovery: Discovery,
    proxy: common::Fixture,
    token: std::path::PathBuf,
    config: ShipConfig,
    _dir: tempfile::TempDir,
}

impl Rig {
    async fn new(script: Vec<Reply>, interval: Duration, config: DiscoveryConfig) -> Rig {
        let api = fake_api(script).await;
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, TOKEN).unwrap();
        let discovery = Discovery::start(&config, Some("pod-a".into())).unwrap();
        let app = app().await;
        let proxy = fixture_discovery(Direction::Inbound, &discovery, move |_| Some(app)).await;
        let config = ShipConfig {
            interval,
            timeout: Duration::from_secs(1),
            target: Some(Target {
                url: format!("https://{}", api.addr),
                ca: api.ca_pem.clone(),
                token_file: token.clone(),
                workload: "pay".into(),
            }),
        };
        Rig {
            api,
            discovery,
            proxy,
            token,
            config,
            _dir: dir,
        }
    }

    fn shipper(&self) -> Shipper {
        self.discovery
            .shipper(self.config.clone())
            .unwrap()
            .expect("shipping is enabled")
    }

    /// Starts shipping; the handle gives the shipper back once `stop` is sent.
    fn start(
        &self,
    ) -> (
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<Shipper>,
    ) {
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let mut shipper = self.shipper();
        let task = tokio::spawn(async move {
            shipper
                .run(async {
                    let _ = stopped.await;
                })
                .await;
            shipper
        });
        (stop, task)
    }

    /// `GET /<segment>/<n>` through the proxy.
    async fn call(&self, segment: &str, n: u32) {
        let mut c = TcpStream::connect(self.proxy.addr).await.unwrap();
        c.write_all(
            format!(
                "GET /{segment}/{n} HTTP/1.1\r\nHost: svc.ns:8080\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let mut out = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), c.read_to_end(&mut out))
            .await
            .expect("answered in time")
            .unwrap();
        assert!(out.starts_with(b"HTTP/1.1 200"));
    }

    /// Waits until the recorder holds `n` exchanges (or the shipper took them).
    async fn recorded(&self, n: u64) {
        wait(|| self.discovery.report().exchanges() >= n).await;
    }
}

async fn wait(mut done: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition not reached in 20 s");
}

const INTERVAL: Duration = Duration::from_millis(200);

fn env(pairs: &[(&str, &str)]) -> Env {
    Env::from_pairs(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())))
}

#[test]
fn ctl_14_the_push_interval_is_validated() {
    for bad in ["0s", "999ms", "2h", "1x", "abc", "-1s"] {
        assert!(
            ShipConfig::from_env(&env(&[("MIMIC_DISCOVERY_PUSH_INTERVAL", bad)])).is_err(),
            "{bad:?}"
        );
    }
    for (good, secs) in [("1s", 1), ("1m", 60), ("1h", 3600)] {
        let config = ShipConfig::from_env(&env(&[("MIMIC_DISCOVERY_PUSH_INTERVAL", good)]));
        assert_eq!(config.unwrap().interval, Duration::from_secs(secs));
    }
    assert_eq!(
        ShipConfig::from_env(&env(&[])).unwrap().interval,
        Duration::from_secs(30)
    );
}

#[test]
fn ctl_14_the_binary_exits_2_on_an_invalid_push_interval() {
    let (status, _, stderr) = run(
        BIN,
        &[],
        &[
            ("MIMIC_ADMIN_ADDR", "127.0.0.1:0"),
            ("MIMIC_DISCOVERY_PUSH_INTERVAL", "soon"),
        ],
    );
    assert_eq!(status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("MIMIC_DISCOVERY_PUSH_INTERVAL"), "{stderr}");
}

#[test]
fn ctl_15_without_a_controller_url_the_binary_logs_that_shipping_is_disabled() {
    let mut process = Process::spawn(
        BIN,
        &[
            ("MIMIC_ADMIN_ADDR", "127.0.0.1:0"),
            ("MIMIC_INBOUND_ADDRS", "127.0.0.1:0"),
            ("MIMIC_OUTBOUND_ADDRS", "127.0.0.1:0"),
        ],
    );
    let line = process.wait_log(Duration::from_secs(10), |v| {
        v["message"] == "report shipping disabled"
    });
    assert_eq!(line["level"], "INFO");
    process.terminate();
    assert!(process.wait_exit(Duration::from_secs(10)).success());
    let n = process
        .logs()
        .iter()
        .filter(|v| v["message"] == "report shipping disabled")
        .count();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn ctl_15_without_a_target_nothing_is_shipped() {
    let logs = capture_logs();
    let discovery = Discovery::start(&DiscoveryConfig::default(), None).unwrap();
    let config = ShipConfig {
        interval: INTERVAL,
        timeout: Duration::from_secs(1),
        target: None,
    };
    assert!(discovery.shipper(config).unwrap().is_none());
    logs.settled().await;
    let text = logs.text();
    assert_eq!(
        text.matches("report shipping disabled").count(),
        1,
        "{text}"
    );
}

#[tokio::test]
async fn ctl_15_posts_ordered_reports_with_one_session_and_the_current_token() {
    let _logs = capture_logs();
    let rig = Rig::new(vec![], INTERVAL, DiscoveryConfig::default()).await;
    let (stop, task) = rig.start();
    for n in 1..=3 {
        rig.call("users", n).await;
    }
    wait(|| rig.api.acknowledged() == 3).await;
    // The kubelet rotates the token: it is read again for the next post.
    std::fs::write(&rig.token, "rotated-token").unwrap();
    for n in 4..=5 {
        rig.call("users", n).await;
    }
    wait(|| rig.api.acknowledged() == 5).await;
    // An interval without exchanges posts nothing.
    let posted = rig.api.count();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(rig.api.count(), posted);
    // After a 200, the report holds only what was recorded since.
    assert_eq!(rig.discovery.report().exchanges(), 0);

    {
        let seen = rig.api.seen.lock().unwrap();
        let first = seen[0].session();
        assert_eq!(first.len(), 32);
        assert!(
            first
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        );
        for (i, s) in seen.iter().enumerate() {
            assert_eq!(s.path, "/v1/workloads/pay/report");
            assert_eq!(s.session(), first);
            assert_eq!(s.sequence(), i as u64 + 1);
            assert!(s.report().exchanges() > 0);
        }
        assert_eq!(seen[0].authorization, format!("Bearer {TOKEN}"));
        assert_eq!(seen.last().unwrap().authorization, "Bearer rotated-token");
    }
    drop(stop);
    task.await.unwrap();
}

#[tokio::test]
async fn ctl_15_a_restarted_sidecar_posts_a_new_session_from_sequence_1() {
    let _logs = capture_logs();
    let mut sessions = Vec::new();
    for _ in 0..2 {
        let rig = Rig::new(vec![], INTERVAL, DiscoveryConfig::default()).await;
        let (_stop, _task) = rig.start();
        rig.call("users", 1).await;
        wait(|| rig.api.acknowledged() == 1).await;
        let seen = rig.api.seen.lock().unwrap();
        assert_eq!(seen[0].sequence(), 1);
        sessions.push(seen[0].session());
    }
    assert_ne!(sessions[0], sessions[1]);
}

#[tokio::test]
async fn ctl_16_a_pending_report_is_retried_unchanged_with_backoff() {
    let _logs = capture_logs();
    let rig = Rig::new(
        vec![Reply::Status(503), Reply::Reset, Reply::Silent],
        INTERVAL,
        DiscoveryConfig::default(),
    )
    .await;
    let (_stop, _task) = rig.start();
    rig.call("users", 1).await;
    rig.call("users", 2).await;
    wait(|| rig.api.count() >= 1).await;
    // Recorded while the first report is pending: sequence 2, after the acknowledgement.
    rig.call("orders", 1).await;
    wait(|| rig.api.count() >= 5).await;
    let seen = rig.api.seen.lock().unwrap();
    for s in &seen[..4] {
        assert_eq!(s.body, seen[0].body, "same bytes");
    }
    assert_eq!(seen[0].sequence(), 1);
    assert!(seen[1].at - seen[0].at >= Duration::from_secs(1));
    assert!(seen[2].at - seen[1].at >= Duration::from_secs(2));
    // The silent post times out after 1 s, then the backoff of 4 s.
    assert!(seen[3].at - seen[2].at >= Duration::from_secs(5));
    assert_eq!(seen[3].status, 200);
    assert_eq!(seen[4].sequence(), 2);
    assert_eq!(
        seen[4].report().exchanges(),
        3 - seen[0].report().exchanges()
    );
    assert!(seen[4].at >= seen[3].at);
}

#[tokio::test]
async fn ctl_16_retry_after_delays_the_retry() {
    let _logs = capture_logs();
    let rig = Rig::new(
        vec![Reply::RetryAfter(429, 3)],
        INTERVAL,
        DiscoveryConfig::default(),
    )
    .await;
    let (_stop, _task) = rig.start();
    rig.call("users", 1).await;
    wait(|| rig.api.count() >= 2).await;
    let seen = rig.api.seen.lock().unwrap();
    assert!(seen[1].at - seen[0].at >= Duration::from_secs(3));
    assert_eq!(seen[1].body, seen[0].body);
}

#[tokio::test]
async fn ctl_16_a_413_resends_the_same_sequence_without_samples() {
    let _logs = capture_logs();
    let rig = Rig::new(
        vec![Reply::Status(413)],
        INTERVAL,
        DiscoveryConfig::default(),
    )
    .await;
    let (_stop, _task) = rig.start();
    rig.call("users", 1).await;
    wait(|| rig.api.count() >= 2).await;
    let seen = rig.api.seen.lock().unwrap();
    assert_eq!(seen[1].sequence(), seen[0].sequence());
    assert_eq!(seen[1].session(), seen[0].session());
    assert!(seen[0].report().sample_bytes() > 0);
    assert_eq!(seen[1].report().sample_bytes(), 0);
    assert_eq!(seen[1].report().exchanges(), 1);
    assert_eq!(seen[1].status, 200);
}

#[tokio::test]
async fn ctl_16_a_400_drops_the_report_and_the_next_sequence_follows() {
    let logs = capture_logs();
    let rig = Rig::new(
        vec![Reply::Status(400)],
        INTERVAL,
        DiscoveryConfig::default(),
    )
    .await;
    let (_stop, _task) = rig.start();
    rig.call("users", 1).await;
    wait(|| rig.api.count() >= 1).await;
    // Not retried.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(rig.api.count(), 1);
    rig.call("users", 2).await;
    wait(|| rig.api.count() >= 2).await;
    {
        let seen = rig.api.seen.lock().unwrap();
        assert_eq!(seen[1].sequence(), 2);
        assert_eq!(seen[1].report().exchanges(), 1);
    }
    logs.settled().await;
    let text = logs.text();
    assert_eq!(text.matches("report rejected").count(), 1, "{text}");
    let line: serde_json::Value = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .find(|v: &serde_json::Value| v["message"] == "report rejected")
        .unwrap();
    assert_eq!(line["level"], "WARN");
    assert_eq!(line["status"], 400);
}

#[tokio::test]
async fn ctl_16_pending_and_current_samples_share_the_budget() {
    let _logs = capture_logs();
    let budget = 6000;
    let rig = Rig::new(
        vec![Reply::Status(503); 100],
        INTERVAL,
        DiscoveryConfig {
            sample_budget: budget,
            max_literals: 1000,
            ..DiscoveryConfig::default()
        },
    )
    .await;
    let (_stop, _task) = rig.start();
    for n in 0..10 {
        rig.call(&format!("a{n}"), 1).await;
    }
    wait(|| rig.api.count() >= 1).await;
    // Far more than the budget holds, recorded while the first report is pending.
    for n in 0..60 {
        rig.call(&format!("b{n}"), 1).await;
    }
    rig.recorded(60).await;
    let pending = rig.api.seen.lock().unwrap()[0].report().sample_bytes();
    let current = rig.discovery.report().sample_bytes();
    assert!(pending > 0 && current > 0, "{pending} {current}");
    assert!(pending + current <= budget, "{pending} + {current}");
}

#[tokio::test]
async fn ctl_16_metrics_and_logs_follow_a_failure_streak() {
    let logs = capture_logs();
    let rig = Rig::new(
        vec![Reply::Status(503), Reply::Status(503)],
        INTERVAL,
        DiscoveryConfig::default(),
    )
    .await;
    let mut metrics = Metrics::new("sidecar", "test");
    rig.discovery.register(&mut metrics);
    assert!(
        metrics
            .encode()
            .contains("mimic_discovery_last_push_timestamp_seconds 0")
    );
    let (_stop, _task) = rig.start();
    rig.call("users", 1).await;
    wait(|| rig.api.acknowledged() == 1).await;
    let text = metrics.encode();
    assert!(
        text.contains("mimic_discovery_pushes_total{result=\"error\"} 2"),
        "{text}"
    );
    assert!(
        text.contains("mimic_discovery_pushes_total{result=\"ok\"} 1"),
        "{text}"
    );
    assert!(
        !text.contains("mimic_discovery_last_push_timestamp_seconds 0\n"),
        "{text}"
    );
    logs.settled().await;
    let text = logs.text();
    assert_eq!(text.matches("report push failed").count(), 1, "{text}");
    assert_eq!(text.matches("report push recovered").count(), 1, "{text}");
    assert!(!text.contains(TOKEN), "{text}");
}

#[tokio::test]
async fn ctl_17_flush_posts_the_pending_then_the_current_report() {
    let _logs = capture_logs();
    let rig = Rig::new(
        vec![Reply::Status(503)],
        Duration::from_secs(1),
        DiscoveryConfig::default(),
    )
    .await;
    let (stop, task) = rig.start();
    rig.call("users", 1).await;
    rig.call("users", 2).await;
    wait(|| rig.api.count() >= 1).await;
    rig.call("orders", 1).await;
    rig.recorded(1).await;
    drop(stop);
    let mut shipper = task.await.unwrap();
    let started = Instant::now();
    shipper.flush(Duration::from_secs(5)).await;
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(rig.api.acknowledged(), 3, "{:?}", rig.api.posts());
    let posts = rig.api.posts();
    let acknowledged: Vec<u64> = posts
        .iter()
        .filter(|(_, status, _)| *status == 200)
        .map(|(sequence, ..)| *sequence)
        .collect();
    assert_eq!(acknowledged, vec![1, 2]);
    assert_eq!(rig.discovery.report().exchanges(), 0);
}

#[tokio::test]
async fn ctl_17_flush_gives_up_within_the_grace_and_counts_the_loss() {
    let logs = capture_logs();
    let rig = Rig::new(
        vec![Reply::Silent; 20],
        Duration::from_secs(1),
        DiscoveryConfig::default(),
    )
    .await;
    let (stop, task) = rig.start();
    rig.call("users", 1).await;
    rig.call("users", 2).await;
    wait(|| rig.api.count() >= 1).await;
    rig.call("orders", 1).await;
    rig.recorded(1).await;
    drop(stop);
    let mut shipper = task.await.unwrap();
    let started = Instant::now();
    shipper.flush(Duration::from_secs(2)).await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    logs.settled().await;
    let lines: Vec<serde_json::Value> = logs
        .text()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(|v: &serde_json::Value| v["message"] == "report not shipped")
        .collect();
    assert_eq!(lines.len(), 1, "{}", logs.text());
    assert_eq!(lines[0]["level"], "WARN");
    assert_eq!(lines[0]["exchanges"], 3);
}
