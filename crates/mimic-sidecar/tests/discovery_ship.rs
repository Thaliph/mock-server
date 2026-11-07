//! CTL-14 (push interval), CTL-15, CTL-16, CTL-17, SHR-15 and SHR-16 on the shipping client of
//! the sidecar (spec §7.2), against a fake sidecar API served over TLS in the test process.

mod common;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// `200` with this body.
    Body(&'static str),
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
    /// Set: past its script the fake answers as a controller up to F7 (`400` to any report
    /// that is not `"version": 1`). Past the script, it acknowledges.
    f7: Arc<AtomicBool>,
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

    /// The posts so far, as `(sequence, version, status)`.
    fn versions(&self) -> Vec<(u64, u64, u16)> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|s| {
                let version = s.json()["report"]["version"].as_u64().unwrap();
                (s.sequence(), version, s.status)
            })
            .collect()
    }

    /// The reports of the posts that satisfy `keep`.
    fn reports(&self, keep: impl Fn(&Seen) -> bool) -> Vec<Report> {
        let seen = self.seen.lock().unwrap();
        seen.iter().filter(|s| keep(s)).map(Seen::report).collect()
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
    let f7 = Arc::new(AtomicBool::new(false));
    let (script_in, seen_in, f7_in) = (script.clone(), seen.clone(), f7.clone());
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, script, seen, f7) = (
                acceptor.clone(),
                script_in.clone(),
                seen_in.clone(),
                f7_in.clone(),
            );
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let service = service_fn(move |req: Request<Incoming>| {
                    let (script, seen, f7) = (script.clone(), seen.clone(), f7.clone());
                    async move {
                        let authorization = req
                            .headers()
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        let path = req.uri().path().to_string();
                        // The egress pull of the sidecar (spec §7.7) is not a push.
                        if path.ends_with("/egress") {
                            return Ok(Response::builder()
                                .status(404)
                                .body(Full::new(Bytes::new()))
                                .unwrap());
                        }
                        let body = req.into_body().collect().await.unwrap().to_bytes().to_vec();
                        let scripted = script.lock().unwrap().pop_front();
                        let reply = scripted.unwrap_or_else(|| {
                            let version = serde_json::from_slice::<serde_json::Value>(&body)
                                .map_or(0, |v| v["report"]["version"].as_u64().unwrap_or(0));
                            if f7.load(Ordering::Relaxed) && version != 1 {
                                Reply::Status(400)
                            } else {
                                Reply::Ok
                            }
                        });
                        let (status, retry_after) = match reply {
                            Reply::Ok | Reply::Body(_) => (200, None),
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
                                let body = match reply {
                                    Reply::Body(text) => text,
                                    _ => "{\"applied\":true}",
                                };
                                Ok(response.body(Full::new(Bytes::from(body))).unwrap())
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
    FakeApi {
        addr,
        ca_pem,
        seen,
        f7,
    }
}

/// An app answering every request with `200 {}`.
async fn app() -> SocketAddr {
    app_on("127.0.0.1:0").await
}

/// The same app on `bind`; a request for `/hang/...` is never answered.
async fn app_on(bind: &str) -> SocketAddr {
    let listener = TcpListener::bind(bind).await.unwrap();
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
                if head.starts_with(b"GET /hang") {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    return;
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
    /// The outbound proxy, towards the app.
    out: common::Fixture,
    /// The outbound proxy, towards an echo server: its connections are tunnels.
    tcp: common::Fixture,
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
        let out = fixture_discovery(Direction::Outbound, &discovery, move |_| Some(app)).await;
        let echo = common::echo_server().await;
        let tcp = fixture_discovery(Direction::Outbound, &discovery, move |_| Some(echo)).await;
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
            out,
            tcp,
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

    /// `GET /v1/items/<n>` to `api.example.test:8080` through the outbound proxy.
    async fn outbound_call(&self, n: u32) {
        let mut c = TcpStream::connect(self.out.addr).await.unwrap();
        c.write_all(
            format!(
                "GET /v1/items/{n} HTTP/1.1\r\nHost: api.example.test:8080\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
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

    /// One opaque TCP connection through the outbound proxy: a tunnel.
    async fn tunnel(&self) {
        let mut c = TcpStream::connect(self.tcp.addr).await.unwrap();
        c.write_all(&[0u8, 1, 2, 3]).await.unwrap();
        let mut back = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(10), c.read_exact(&mut back))
            .await
            .expect("echoed in time")
            .unwrap();
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
            max_authorities: 64,
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
    // The API acknowledges before the sidecar counts the answer.
    // The counter is set before the timestamp: wait for both.
    wait(|| {
        let text = metrics.encode();
        text.contains("mimic_discovery_pushes_total{result=\"ok\"} 1")
            && !text.contains("mimic_discovery_last_push_timestamp_seconds 0\n")
    })
    .await;
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

#[tokio::test]
async fn ctl_17_flush_retries_fast_and_ignores_retry_after() {
    // The kubelet leaves a sidecar about 2 s after the app containers stopped (#298): the flush
    // must not sleep a backoff or a `Retry-After` of 1 s or more between two posts.
    let _logs = capture_logs();
    let rig = Rig::new(
        vec![Reply::RetryAfter(503, 3), Reply::Reset],
        Duration::from_secs(3600),
        DiscoveryConfig::default(),
    )
    .await;
    rig.call("users", 1).await;
    rig.call("users", 2).await;
    rig.recorded(2).await;
    let mut shipper = rig.shipper();
    let started = Instant::now();
    shipper.flush(Duration::from_secs(10)).await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(rig.api.acknowledged(), 2, "{:?}", rig.api.posts());
    assert_eq!(rig.api.count(), 3, "{:?}", rig.api.posts());
}

#[tokio::test]
async fn ctl_17_flush_pauses_200_ms_even_after_a_retry_after_of_0() {
    // A `Retry-After: 0` must not turn the flush into back-to-back posts: the pause is fixed.
    let _logs = capture_logs();
    let rig = Rig::new(
        vec![Reply::RetryAfter(503, 0); 50],
        Duration::from_secs(3600),
        DiscoveryConfig::default(),
    )
    .await;
    rig.call("users", 1).await;
    rig.recorded(1).await;
    let mut shipper = rig.shipper();
    shipper.flush(Duration::from_secs(1)).await;
    // One post, then one every 200 ms within the second: at most 6, never the 50 replies.
    assert!(rig.api.count() <= 6, "{:?}", rig.api.posts());
    assert!(rig.api.count() >= 3, "{:?}", rig.api.posts());
}

#[tokio::test]
async fn ctl_16_a_failure_streak_ended_by_a_dropped_report_logs_the_next_failure() {
    let logs = capture_logs();
    // 503 then 400 for the first report, 503 then 200 for the second.
    let rig = Rig::new(
        vec![Reply::Status(503), Reply::Status(400), Reply::Status(503)],
        INTERVAL,
        DiscoveryConfig::default(),
    )
    .await;
    let (_stop, _task) = rig.start();
    rig.call("users", 1).await;
    wait(|| rig.api.count() >= 2).await;
    rig.call("users", 2).await;
    wait(|| rig.api.acknowledged() == 1).await;
    {
        let seen = rig.api.seen.lock().unwrap();
        assert_eq!(seen.len(), 4);
        // The backoff restarted at 1 s for the second report.
        assert!(seen[3].at - seen[2].at < Duration::from_millis(1800));
    }
    logs.settled().await;
    let text = logs.text();
    assert_eq!(text.matches("report push failed").count(), 2, "{text}");
}

#[tokio::test]
async fn ctl_16_a_200_with_an_unreadable_body_is_a_failed_attempt() {
    let _logs = capture_logs();
    let rig = Rig::new(
        vec![Reply::Body("not json"), Reply::Body("{}")],
        INTERVAL,
        DiscoveryConfig::default(),
    )
    .await;
    let mut metrics = Metrics::new("sidecar", "test");
    rig.discovery.register(&mut metrics);
    let (_stop, _task) = rig.start();
    rig.call("users", 1).await;
    wait(|| rig.api.count() >= 3).await;
    {
        let seen = rig.api.seen.lock().unwrap();
        // The same report, three times: the first two answers did not acknowledge it.
        assert_eq!(seen[1].body, seen[0].body);
        assert_eq!(seen[2].body, seen[0].body);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
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
        text.contains("mimic_discovery_pushes_total{result=\"duplicate\"} 0"),
        "{text}"
    );
}

#[tokio::test]
async fn ctl_16_gauges_cover_the_pending_and_the_current_report() {
    let _logs = capture_logs();
    let rig = Rig::new(
        vec![Reply::Status(503); 3],
        INTERVAL,
        DiscoveryConfig::default(),
    )
    .await;
    let mut metrics = Metrics::new("sidecar", "test");
    rig.discovery.register(&mut metrics);
    let gauge = |name: &str| -> i64 {
        metrics
            .encode()
            .lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.rsplit(' ').next())
            .and_then(|v| v.parse().ok())
            .unwrap_or(-1)
    };
    let (_stop, _task) = rig.start();
    rig.call("users", 1).await;
    wait(|| rig.api.count() >= 1).await;
    // The first report is pending (the fake refuses it), a second endpoint is current.
    rig.call("orders", 1).await;
    rig.recorded(1).await;
    let pending = rig.api.seen.lock().unwrap()[0].report().sample_bytes();
    let current = rig.discovery.report().sample_bytes();
    assert!(pending > 0 && current > 0, "{pending} {current}");
    wait(|| gauge("mimic_discovery_sample_bytes") == (pending + current) as i64).await;
    wait(|| gauge("mimic_discovery_endpoints{direction=\"inbound\"}") == 2).await;
    // Everything acknowledged: nothing is held.
    wait(|| rig.api.acknowledged() == 2).await;
    wait(|| gauge("mimic_discovery_sample_bytes") == 0).await;
    wait(|| gauge("mimic_discovery_endpoints{direction=\"inbound\"}") == 0).await;
}

// ---------------------------------------------------------------------------------------
// SHR-15 and SHR-16: shipping the outbound part, and the fallback to inbound only.

fn metric(rig: &Rig, name: &str) -> i64 {
    let mut metrics = Metrics::new("sidecar", "test");
    rig.discovery.register(&mut metrics);
    metrics
        .encode()
        .lines()
        .find(|l| l.starts_with(name))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(-1)
}

const INBOUND_ONLY: &str = "mimic_discovery_pushes_total{result=\"inbound_only\"}";
const DROPPED: &str = "mimic_discovery_outbound_dropped_total";

/// The `outbound not shipped` lines so far.
async fn not_shipped(logs: &common::Logs) -> Vec<serde_json::Value> {
    logs.settled().await;
    logs.text()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["message"] == "outbound not shipped")
        .collect()
}

/// The tunnels counted by the outbound scopes of `report`.
fn tunnels(report: &Report) -> u64 {
    let v: serde_json::Value = serde_json::from_str(&report.to_json().unwrap()).unwrap();
    v["scopes"]["outbound"].as_object().map_or(0, |scopes| {
        scopes
            .values()
            .filter_map(|s| s["tunnels"].as_object())
            .flat_map(|t| t.values().filter_map(serde_json::Value::as_u64))
            .sum()
    })
}

fn holds_outbound(report: &Report) -> bool {
    report
        .endpoints()
        .iter()
        .any(|e| e.direction == mimic_core::discovery::report::Direction::Outbound)
}

fn outbound_scope_in(seen: &Seen) -> bool {
    seen.json()["report"]["scopes"].get("outbound").is_some()
}

/// Outbound exchanges in the posts the fake acknowledged.
fn shipped_outbound(rig: &Rig) -> u64 {
    rig.api
        .reports(|s| s.status == 200)
        .iter()
        .map(Report::outbound_exchanges)
        .sum()
}

#[tokio::test]
async fn shr_15_pushes_carry_the_outbound_part_and_reset_it_after_the_acknowledgement() {
    let _logs = capture_logs();
    let rig = Rig::new(vec![], INTERVAL, DiscoveryConfig::default()).await;
    for n in 1..=3 {
        rig.outbound_call(n).await;
    }
    rig.tunnel().await;
    rig.tunnel().await;
    rig.call("users", 1).await;
    rig.call("users", 2).await;
    wait(|| rig.discovery.report().exchanges() >= 5 && tunnels(&rig.discovery.report()) == 2).await;
    wait(|| metric(&rig, "mimic_discovery_endpoints{direction=\"outbound\"}") == 1).await;
    assert!(metric(&rig, "mimic_discovery_sample_bytes") > 0);

    let (stop, task) = rig.start();
    wait(|| rig.api.acknowledged() == 5).await;
    // Every push that holds an outbound scope is version 2, the others version 1.
    {
        let seen = rig.api.seen.lock().unwrap();
        assert!(!seen.is_empty());
        for s in seen.iter() {
            let version = s.json()["report"]["version"].as_u64().unwrap();
            assert_eq!(version, if outbound_scope_in(s) { 2 } else { 1 });
        }
    }
    let all = rig.api.reports(|s| s.status == 200);
    let outbound: u64 = all.iter().map(Report::outbound_exchanges).sum();
    let inbound: u64 = all
        .iter()
        .map(|r| r.exchanges() - r.outbound_exchanges())
        .sum();
    assert_eq!((inbound, outbound), (2, 3));
    assert_eq!(all.iter().map(tunnels).sum::<u64>(), 2);
    // After the acknowledgement /discovery is reset: counters, samples, origin and tunnels.
    wait(|| rig.discovery.report().exchanges() == 0).await;
    let after = rig.discovery.report();
    assert!(!holds_outbound(&after));
    assert_eq!(tunnels(&after), 0);
    assert_eq!(after.sample_bytes(), 0);
    wait(|| metric(&rig, "mimic_discovery_endpoints{direction=\"outbound\"}") <= 0).await;
    wait(|| metric(&rig, "mimic_discovery_sample_bytes") == 0).await;
    drop(stop);
    task.await.unwrap();

    // Outbound traffic recorded since is all that /discovery holds.
    rig.outbound_call(4).await;
    wait(|| rig.discovery.report().outbound_exchanges() == 1).await;
    assert_eq!(rig.discovery.report().exchanges(), 1);
    assert!(metric(&rig, "mimic_discovery_sample_bytes") > 0);
    assert_eq!(
        metric(&rig, "mimic_discovery_endpoints{direction=\"outbound\"}"),
        1
    );
}

#[tokio::test]
async fn shr_15_an_interval_with_only_a_tunnel_is_pushed() {
    let _logs = capture_logs();
    let rig = Rig::new(vec![], INTERVAL, DiscoveryConfig::default()).await;
    rig.tunnel().await;
    wait(|| tunnels(&rig.discovery.report()) == 1).await;
    let (stop, task) = rig.start();
    wait(|| rig.api.count() >= 1).await;
    let posts = rig.api.versions();
    assert_eq!(posts[0], (1, 2, 200), "{posts:?}");
    let report = rig.api.seen.lock().unwrap()[0].report();
    assert_eq!((report.exchanges(), tunnels(&report)), (0, 1));
    wait(|| tunnels(&rig.discovery.report()) == 0).await;
    // Nothing recorded since: nothing is pushed.
    tokio::time::sleep(INTERVAL * 3).await;
    assert_eq!(rig.api.count(), 1);
    drop(stop);
    task.await.unwrap();
}

#[tokio::test]
async fn shr_15_outbound_samples_of_pending_and_current_stay_within_half_the_budget() {
    let _logs = capture_logs();
    let budget = 6_000;
    let config = DiscoveryConfig {
        sample_budget: budget,
        ..DiscoveryConfig::default()
    };
    // The fake refuses every push: the first report stays pending.
    let rig = Rig::new(vec![Reply::Status(503); 50], INTERVAL, config).await;
    let call = |n: u32| {
        let addr = rig.out.addr;
        async move {
            let mut c = TcpStream::connect(addr).await.unwrap();
            c.write_all(
                format!(
                    "GET /seg{n}/x HTTP/1.1\r\nHost: api.example.test:8080\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            let mut out = Vec::new();
            c.read_to_end(&mut out).await.unwrap();
        }
    };
    for n in 0..6 {
        call(n).await;
    }
    wait(|| rig.discovery.report().outbound_exchanges() == 6).await;
    let (_stop, _task) = rig.start();
    wait(|| rig.api.count() >= 1).await;
    for n in 6..40 {
        call(n).await;
    }
    wait(|| rig.discovery.report().outbound_exchanges() >= 30).await;
    tokio::time::sleep(INTERVAL).await;
    let pending = rig.api.seen.lock().unwrap()[0].report().sample_bytes();
    let current = rig.discovery.report().sample_bytes();
    assert!(pending > 0, "the pending report holds outbound samples");
    assert!(
        pending + current <= budget / 2,
        "{pending} + {current} over {}",
        budget / 2
    );
}

#[tokio::test]
async fn shr_15_without_a_controller_url_the_outbound_part_accumulates() {
    let _logs = capture_logs();
    let rig = Rig::new(vec![], INTERVAL, DiscoveryConfig::default()).await;
    let mut config = rig.config.clone();
    config.target = None;
    assert!(rig.discovery.shipper(config).unwrap().is_none());
    for n in 1..=3 {
        rig.outbound_call(n).await;
    }
    wait(|| rig.discovery.report().outbound_exchanges() == 3).await;
    rig.outbound_call(4).await;
    wait(|| rig.discovery.report().outbound_exchanges() == 4).await;
    assert_eq!(rig.api.count(), 0);
}

/// Inbound and outbound exchanges, recorded before the next push.
async fn traffic(rig: &Rig, inbound: u32, outbound: u32) {
    for n in 1..=inbound {
        rig.call("users", n).await;
    }
    for n in 1..=outbound {
        rig.outbound_call(n).await;
    }
    wait(|| {
        let report = rig.discovery.report();
        report.exchanges() >= u64::from(inbound + outbound)
            && report.outbound_exchanges() >= u64::from(outbound)
    })
    .await;
}

#[tokio::test]
async fn shr_16_a_400_falls_back_to_inbound_only_and_the_next_interval_tries_version_2_again() {
    let logs = capture_logs();
    let rig = Rig::new(vec![], INTERVAL, DiscoveryConfig::default()).await;
    rig.api.f7.store(true, Ordering::Relaxed);
    traffic(&rig, 2, 3).await;
    let (stop, task) = rig.start();
    // Version 2 refused, then the same session and sequence, inbound only, applied.
    wait(|| rig.api.count() >= 2).await;
    let posts = rig.api.versions();
    assert_eq!(posts[0], (1, 2, 400), "{posts:?}");
    assert_eq!(posts[1], (1, 1, 200), "{posts:?}");
    {
        let seen = rig.api.seen.lock().unwrap();
        assert_eq!(seen[0].session(), seen[1].session());
        assert!(!outbound_scope_in(&seen[1]));
        assert_eq!(seen[1].report().exchanges(), 2);
    }
    // Every outbound exchange is still in /discovery, none lost.
    wait(|| rig.discovery.report().outbound_exchanges() == 3).await;
    assert_eq!(rig.discovery.report().exchanges(), 3);
    // The next interval tries version 2 again (the outbound part alone: nothing else posts).
    wait(|| rig.api.count() >= 3).await;
    assert_eq!(rig.api.versions()[2], (2, 2, 400));
    wait(|| metric(&rig, INBOUND_ONLY) >= 2).await;
    // A streak of fallbacks writes one line.
    let lines = not_shipped(&logs).await;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["level"], "WARN");
    assert_eq!(lines[0]["status"], 400);
    assert!(lines[0].get("reason").is_none(), "{lines:?}");
    assert_eq!(metric(&rig, DROPPED), 0);

    // The fake accepts version 2: every outbound exchange recorded since the start, once.
    rig.api.f7.store(false, Ordering::Relaxed);
    wait(|| shipped_outbound(&rig) == 3).await;
    let inbound: u64 = rig
        .api
        .reports(|s| s.status == 200)
        .iter()
        .map(|r| r.exchanges() - r.outbound_exchanges())
        .sum();
    assert_eq!(inbound, 2);
    // The acknowledged version 2 push ended the streak: the next fallback writes again.
    rig.api.f7.store(true, Ordering::Relaxed);
    let before = metric(&rig, INBOUND_ONLY);
    rig.outbound_call(9).await;
    wait(|| metric(&rig, INBOUND_ONLY) > before).await;
    assert_eq!(not_shipped(&logs).await.len(), 2);
    for line in logs.text().lines() {
        assert!(!line.contains(TOKEN), "{line}");
    }
    drop(stop);
    task.await.unwrap();
}

#[tokio::test]
async fn shr_16_an_outbound_report_without_inbound_posts_nothing_after_the_400() {
    let _logs = capture_logs();
    let rig = Rig::new(vec![], INTERVAL, DiscoveryConfig::default()).await;
    rig.api.f7.store(true, Ordering::Relaxed);
    traffic(&rig, 0, 3).await;
    let (stop, task) = rig.start();
    wait(|| rig.api.count() >= 3).await;
    // Only version 2 attempts, all of sequence 1: the next freeze uses the same sequence.
    for post in rig.api.versions() {
        assert_eq!(post, (1, 2, 400));
    }
    wait(|| rig.discovery.report().outbound_exchanges() == 3).await;
    rig.api.f7.store(false, Ordering::Relaxed);
    wait(|| rig.api.acknowledged() == 3).await;
    let last = *rig.api.versions().last().unwrap();
    assert_eq!(last, (1, 2, 200));
    drop(stop);
    task.await.unwrap();
}

#[tokio::test]
async fn shr_16_a_507_falls_back_the_same_way() {
    let _logs = capture_logs();
    let rig = Rig::new(
        vec![Reply::Status(507)],
        INTERVAL,
        DiscoveryConfig::default(),
    )
    .await;
    traffic(&rig, 2, 3).await;
    let (stop, task) = rig.start();
    wait(|| rig.api.count() >= 2).await;
    let posts = rig.api.versions();
    assert_eq!(posts[0], (1, 2, 507), "{posts:?}");
    assert_eq!(posts[1], (1, 1, 200), "{posts:?}");
    // Nothing is lost: the outbound part follows in version 2.
    wait(|| shipped_outbound(&rig) == 3).await;
    drop(stop);
    task.await.unwrap();
}

#[tokio::test]
async fn shr_16_a_400_to_the_inbound_only_post_drops_it_with_one_warning() {
    let logs = capture_logs();
    let rig = Rig::new(
        vec![Reply::Status(400), Reply::Status(400)],
        INTERVAL,
        DiscoveryConfig::default(),
    )
    .await;
    traffic(&rig, 2, 3).await;
    let (stop, task) = rig.start();
    wait(|| rig.api.count() >= 2).await;
    wait(|| metric(&rig, "mimic_discovery_pushes_total{result=\"rejected\"}") == 1).await;
    let posts = rig.api.versions();
    assert_eq!(posts[0], (1, 2, 400), "{posts:?}");
    assert_eq!(posts[1], (1, 1, 400), "{posts:?}");
    logs.settled().await;
    let rejected: Vec<_> = logs
        .text()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["message"] == "report rejected")
        .collect();
    assert_eq!(rejected.len(), 1, "{rejected:?}");
    assert_eq!(rejected[0]["status"], 400);
    // The outbound part was kept: it is shipped by a later push.
    wait(|| shipped_outbound(&rig) == 3).await;
    drop(stop);
    task.await.unwrap();
}

/// A version 2 report whose first attempt gets `first`, then `400`, then is acknowledged
/// (the inbound-only post). Returns once that post is applied.
async fn first_then_400(first: Reply) -> (Rig, common::Logs) {
    let logs = capture_logs();
    let rig = Rig::new(
        vec![first, Reply::Status(400), Reply::Ok],
        INTERVAL,
        DiscoveryConfig::default(),
    )
    .await;
    traffic(&rig, 2, 3).await;
    let (stop, task) = rig.start();
    // The three posts, the last one acknowledged (a `200` that the fake sends with a body that
    // does not acknowledge is not).
    wait(|| rig.api.count() >= 3).await;
    wait(|| metric(&rig, "mimic_discovery_pushes_total{result=\"ok\"}") >= 1).await;
    drop(stop);
    task.await.unwrap();
    (rig, logs)
}

#[tokio::test]
async fn shr_16_after_an_unknown_first_attempt_a_400_drops_the_outbound_part() {
    let (rig, logs) = first_then_400(Reply::Reset).await;
    let posts = rig.api.versions();
    assert_eq!(posts[0], (1, 2, 0), "{posts:?}");
    assert_eq!(posts[1], (1, 2, 400), "{posts:?}");
    assert_eq!(posts[2], (1, 1, 200), "{posts:?}");
    // Dropped: not in /discovery, counted, and one WARN with the reason.
    assert!(!holds_outbound(&rig.discovery.report()));
    assert_eq!(rig.discovery.report().exchanges(), 0);
    assert_eq!(metric(&rig, DROPPED), 1);
    assert_eq!(metric(&rig, INBOUND_ONLY), 1);
    let lines = not_shipped(&logs).await;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["level"], "WARN");
    assert_eq!(lines[0]["reason"], "unknown_outcome");
    assert_eq!(lines[0]["status"], 400);
    // The inbound part is shipped once; the outbound exchanges are counted once overall: the
    // fake applied them in the first attempt and nothing posts them again.
    let applied = rig
        .api
        .reports(|s| s.status == 200 || s.status == 0 && outbound_scope_in(s));
    assert_eq!(
        applied.iter().map(Report::outbound_exchanges).sum::<u64>(),
        3
    );
    let inbound = rig
        .api
        .reports(|s| s.status == 200 && !outbound_scope_in(s));
    assert_eq!(inbound.len(), 1);
    assert_eq!(inbound[0].exchanges(), 2);
    for line in logs.text().lines() {
        assert!(!line.contains(TOKEN), "{line}");
    }
    // Later outbound traffic ships in version 2 again.
    let (stop, task) = rig.start();
    rig.outbound_call(7).await;
    wait(|| shipped_outbound(&rig) == 1).await;
    drop(stop);
    task.await.unwrap();
}

#[tokio::test]
async fn shr_16_500_502_504_and_malformed_answers_are_unknown_outcomes() {
    for first in [
        Reply::Status(500),
        Reply::Status(502),
        Reply::Status(504),
        Reply::Body("not json"),
        Reply::Body("{\"applied\":\"yes\"}"),
    ] {
        let (rig, logs) = first_then_400(first).await;
        assert!(!holds_outbound(&rig.discovery.report()), "{first:?}");
        assert_eq!(metric(&rig, DROPPED), 1, "{first:?}");
        let lines = not_shipped(&logs).await;
        assert_eq!(lines.len(), 1, "{first:?} {lines:?}");
        assert_eq!(lines[0]["reason"], "unknown_outcome", "{first:?}");
    }
}

#[tokio::test]
async fn shr_16_answered_attempts_before_the_400_keep_the_outbound_part() {
    for first in [
        Reply::Status(503),
        Reply::Status(429),
        Reply::Status(401),
        Reply::Status(403),
        Reply::Status(413),
    ] {
        let (rig, logs) = first_then_400(first).await;
        assert_eq!(metric(&rig, DROPPED), 0, "{first:?}");
        let lines = not_shipped(&logs).await;
        assert_eq!(lines.len(), 1, "{first:?} {lines:?}");
        assert!(lines[0].get("reason").is_none(), "{first:?} {lines:?}");
        assert_eq!(lines[0]["status"], 400);
    }
}

#[tokio::test]
async fn shr_16_the_unknown_flag_persists_across_answered_retries_and_resets_on_a_new_sequence() {
    let logs = capture_logs();
    // Sequence 1: no answer, then 503 (answered), then 400: the first attempt is still
    // unknown. The inbound-only post is applied. Sequence 2: its first attempt gets a 400.
    let script = vec![
        Reply::Reset,
        Reply::Status(503),
        Reply::Status(400),
        Reply::Ok,
        Reply::Status(400),
        Reply::Ok,
    ];
    let rig = Rig::new(script, INTERVAL, DiscoveryConfig::default()).await;
    traffic(&rig, 2, 3).await;
    let (stop, task) = rig.start();
    wait(|| rig.api.count() >= 1).await;
    // Recorded while sequence 1 is pending: it is frozen as sequence 2.
    traffic(&rig, 2, 2).await;
    wait(|| rig.api.count() >= 6).await;
    let posts = rig.api.versions();
    assert_eq!(posts[0], (1, 2, 0), "{posts:?}");
    assert_eq!(posts[1], (1, 2, 503), "{posts:?}");
    assert_eq!(posts[2], (1, 2, 400), "{posts:?}");
    assert_eq!(posts[3], (1, 1, 200), "{posts:?}");
    assert_eq!(posts[4], (2, 2, 400), "{posts:?}");
    assert_eq!(posts[5], (2, 1, 200), "{posts:?}");
    // Sequence 1 dropped its outbound part, sequence 2 (answered) kept it.
    assert_eq!(metric(&rig, DROPPED), 1);
    wait(|| metric(&rig, INBOUND_ONLY) >= 2).await;
    // Both cases apply, the unknown outcome first: its line wins and also warns the streak,
    // so the plain fallback of sequence 2 writes none.
    let lines = not_shipped(&logs).await;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["reason"], "unknown_outcome");
    // The outbound exchanges of sequence 2 are shipped once, in version 2.
    wait(|| shipped_outbound(&rig) == 2).await;
    drop(stop);
    task.await.unwrap();
}

#[tokio::test]
async fn shr_16_a_streak_line_and_a_sequence_line_are_each_written_once_per_event() {
    let logs = capture_logs();
    // Sequence 1: a plain fallback (the streak's line). Sequence 2: an unknown outcome in the
    // same streak (the sequence's line, which carries `status` too): one line per event.
    let script = vec![
        Reply::Status(400),
        Reply::Ok,
        Reply::Reset,
        Reply::Status(400),
    ];
    let rig = Rig::new(script, INTERVAL, DiscoveryConfig::default()).await;
    traffic(&rig, 2, 3).await;
    let (stop, task) = rig.start();
    wait(|| metric(&rig, DROPPED) == 1).await;
    let lines = not_shipped(&logs).await;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].get("reason").is_none());
    assert_eq!(lines[0]["status"], 400);
    assert_eq!(lines[1]["reason"], "unknown_outcome");
    assert_eq!(lines[1]["status"], 400);
    // The inbound part of sequence 2 covers no exchange: nothing was posted. A replica may
    // have applied sequence 2, so the next freeze does not use it again.
    assert_eq!(rig.api.count(), 4, "{:?}", rig.api.versions());
    rig.outbound_call(10).await;
    wait(|| rig.api.acknowledged() >= 3).await;
    let last = *rig.api.versions().last().unwrap();
    assert_eq!(last, (3, 2, 200));
    drop(stop);
    task.await.unwrap();
}

#[tokio::test]
async fn shr_16_the_sigterm_flush_falls_back_and_ships_the_inbound_part() {
    let logs = capture_logs();
    // The first report is pending (refused) when the signal comes; the flush gets a 400.
    let script = vec![Reply::Status(503), Reply::Status(400)];
    let rig = Rig::new(script, INTERVAL, DiscoveryConfig::default()).await;
    traffic(&rig, 2, 3).await;
    let (stop, task) = rig.start();
    wait(|| rig.api.count() >= 1).await;
    drop(stop);
    let mut shipper = task.await.unwrap();
    // The current report: one more inbound exchange and one more outbound.
    rig.call("orders", 1).await;
    rig.outbound_call(5).await;
    wait(|| rig.discovery.report().exchanges() >= 2).await;
    let begun = Instant::now();
    shipper.flush(Duration::from_secs(5)).await;
    assert!(
        begun.elapsed() < Duration::from_secs(3),
        "{:?}",
        begun.elapsed()
    );
    let posts = rig.api.versions();
    assert_eq!(posts[0], (1, 2, 503), "{posts:?}");
    assert_eq!(posts[1], (1, 2, 400), "{posts:?}");
    assert_eq!(posts[2], (1, 1, 200), "{posts:?}");
    // Then the current report, inbound only: the flush does not try version 2 again.
    assert_eq!(posts[3], (2, 1, 200), "{posts:?}");
    assert_eq!(posts.len(), 4, "{posts:?}");
    let inbound: u64 = rig
        .api
        .reports(|s| s.status == 200)
        .iter()
        .map(Report::exchanges)
        .sum();
    assert_eq!(inbound, 3);
    assert_eq!(metric(&rig, INBOUND_ONLY), 1);
    let lines = not_shipped(&logs).await;
    assert_eq!(lines.len(), 1, "{lines:?}");
}

// ---------------------------------------------------------------------------------------
// CTL-17 as the binary, in a network namespace where `127.0.0.9 -> 127.0.0.1:18080` is
// redirected to the sidecar the way the init container redirects a pod's traffic.

const REDIRECTED: &str = "127.0.0.1:18080";
const CLIENT_IP: &str = "127.0.0.9";

/// A test that cannot run here: skipped with a message, or failed under `MIMIC_TEST_REQUIRE_NETNS=1`
/// (the CI test job does not set it: a CI run on a GitHub-hosted `ubuntu-24.04` runner failed the probe,
/// `unshare -rn` with `ip` and `iptables` is not usable there, so the test is skipped in CI and runs locally).
fn skip(test: &str, why: &str) {
    assert!(
        std::env::var_os("MIMIC_TEST_REQUIRE_NETNS").is_none(),
        "{test} needs a user and network namespace with iptables, and MIMIC_TEST_REQUIRE_NETNS is set: {why}"
    );
    eprintln!("{test}: skipped, {why} (set MIMIC_TEST_REQUIRE_NETNS=1 to make this a failure)");
}

/// `true` inside the namespace (run the body). Outside, runs this test again in a user and network
/// namespace with the redirect, and returns `false`; skips (see [`skip`]) when the host has no
/// `unshare`, forbids user namespaces or lacks `ip` or `iptables`.
fn in_namespace(test: &str) -> bool {
    if std::env::var_os("MIMIC_TEST_NETNS").is_some() {
        return true;
    }
    let rule = format!(
        "iptables -t nat -A OUTPUT -p tcp -s {CLIENT_IP} -d 127.0.0.1 --dport 18080 -j REDIRECT --to-ports 15006"
    );
    let setup = format!("ip link set lo up && {rule}");
    // Can the namespace be set up at all?
    let probe = std::process::Command::new("unshare")
        .args(["-rn", "sh", "-c", &setup])
        .stderr(std::process::Stdio::null())
        .status();
    match probe {
        Err(e) => return skip_false(test, &format!("unshare is not available ({e})")),
        Ok(status) if !status.success() => {
            return skip_false(
                test,
                &format!("no user namespace with ip and iptables here ({status})"),
            );
        }
        Ok(_) => {}
    }
    let script = format!("{setup} && exec \"$0\" \"$@\"");
    let status = std::process::Command::new("unshare")
        .args(["-rn", "sh", "-c", &script])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env("MIMIC_TEST_NETNS", "1")
        .status()
        .expect("unshare starts");
    assert!(status.success(), "{test} failed in its namespace: {status}");
    false
}

fn skip_false(test: &str, why: &str) -> bool {
    skip(test, why);
    false
}

/// A request from the redirected source address; the connection is returned unread.
async fn redirected_get(path: &str) -> TcpStream {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket
        .bind(format!("{CLIENT_IP}:0").parse().unwrap())
        .unwrap();
    let mut c = socket.connect(REDIRECTED.parse().unwrap()).await.unwrap();
    c.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: svc.ns:8080\r\nAccept: application/json\r\n\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    c
}

async fn answered(path: &str) {
    let mut c = redirected_get(path).await;
    let mut head = [0u8; 12];
    c.read_exact(&mut head).await.unwrap();
    assert_eq!(&head, b"HTTP/1.1 200");
}

/// Starts the sidecar binary shipping to `api`, with `grace` as its shutdown grace.
async fn sidecar(api: &FakeApi, dir: &std::path::Path, grace: &str) -> Process {
    let token = dir.join("token");
    std::fs::write(&token, TOKEN).unwrap();
    let url = format!("https://{}", api.addr);
    let mut process = Process::spawn(
        BIN,
        &[
            ("MIMIC_ADMIN_ADDR", "127.0.0.1:0"),
            ("MIMIC_INBOUND_ADDRS", "127.0.0.1:15006"),
            ("MIMIC_OUTBOUND_ADDRS", "127.0.0.1:15001"),
            ("MIMIC_CONTROLLER_URL", &url),
            ("MIMIC_CONTROLLER_CA", &api.ca_pem),
            ("MIMIC_TLS_TOKEN_FILE", token.to_str().unwrap()),
            ("MIMIC_WORKLOAD", "pay"),
            ("MIMIC_DISCOVERY_PUSH_INTERVAL", "1s"),
            ("MIMIC_SHUTDOWN_GRACE", grace),
        ],
    );
    let admin = process.admin_addr();
    mimic_runtime::testing::wait_status(admin, "/readyz", 200, Duration::from_secs(10));
    process
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctl_17_sigterm_flushes_the_pending_then_the_current_report_despite_a_long_drain() {
    if !in_namespace(
        "ctl_17_sigterm_flushes_the_pending_then_the_current_report_despite_a_long_drain",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // The first two posts are refused: the first report is pending when the signal comes.
    let api = fake_api(vec![Reply::Status(503), Reply::Status(503)]).await;
    app_on(REDIRECTED).await;
    let mut process = sidecar(&api, dir.path(), "4s").await;
    answered("/users/1").await;
    wait(|| api.count() >= 1).await;
    answered("/users/2").await;
    // A request the app never answers keeps the drain going until its limit.
    let _hung = redirected_get("/hang/1").await;
    wait(|| api.count() >= 2).await;
    let begun = Instant::now();
    process.terminate();
    let status = process.wait_exit(Duration::from_secs(10));
    let took = begun.elapsed();
    assert!(status.success(), "{status}");
    assert!(took < Duration::from_secs(5), "{took:?}");
    let posts = api.posts();
    assert_eq!(api.acknowledged(), 2, "{posts:?}");
    let sequences: Vec<u64> = posts
        .iter()
        .filter(|(_, status, _)| *status == 200)
        .map(|(sequence, ..)| *sequence)
        .collect();
    assert_eq!(sequences, vec![1, 2], "{posts:?}");
    let lost = process
        .logs()
        .iter()
        .filter(|v| v["message"] == "report not shipped")
        .count();
    assert_eq!(lost, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctl_17_sigterm_with_a_silent_api_exits_within_the_grace_and_counts_the_loss() {
    if !in_namespace("ctl_17_sigterm_with_a_silent_api_exits_within_the_grace_and_counts_the_loss")
    {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let api = fake_api(vec![Reply::Silent; 50]).await;
    app_on(REDIRECTED).await;
    let mut process = sidecar(&api, dir.path(), "3s").await;
    answered("/users/1").await;
    wait(|| api.count() >= 1).await;
    answered("/users/2").await;
    let begun = Instant::now();
    process.terminate();
    let status = process.wait_exit(Duration::from_secs(10));
    assert!(status.success(), "{status}");
    assert!(
        begun.elapsed() < Duration::from_secs(4),
        "{:?}",
        begun.elapsed()
    );
    let lines: Vec<serde_json::Value> = process
        .logs()
        .into_iter()
        .filter(|v| v["message"] == "report not shipped")
        .collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["level"], "WARN");
    assert_eq!(lines[0]["exchanges"], 2);
}
