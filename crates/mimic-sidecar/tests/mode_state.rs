//! MCK-10: the mode state of the sidecar (spec §3, §7.7, §8), against a fake sidecar API
//! served over TLS in the test process and the real binary.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use mimic_runtime::testing::{Process, http_get};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

const BIN: &str = env!("CARGO_BIN_EXE_mimic-sidecar");
const EMPTY: &str = r#"{"authorities":{},"format":1}"#;

type Header = Option<&'static str>;

/// What the fake API does with one pull; the headers are `Mimic-Mode` and
/// `Mimic-Latency-Factor` (`None`: absent).
#[derive(Clone)]
enum Reply {
    /// `200` with the empty set.
    Ok(Header, Header),
    /// `200` with a body the sidecar refuses.
    Invalid(Header, Header),
    /// `304`.
    NotModified(Header, Header),
    Status(u16),
    Reset,
    /// No answer for an hour.
    Silent,
}

struct FakeApi {
    addr: SocketAddr,
    ca_pem: String,
    seen: Arc<Mutex<Vec<Instant>>>,
}

impl FakeApi {
    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

async fn fake_api(script: Vec<Reply>, fallback: Reply) -> FakeApi {
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
    let script: Arc<Mutex<VecDeque<Reply>>> = Arc::new(Mutex::new(script.into()));
    let seen: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let seen_in = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, script, seen, fallback) = (
                acceptor.clone(),
                script.clone(),
                seen_in.clone(),
                fallback.clone(),
            );
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let service = service_fn(move |_: Request<Incoming>| {
                    let (script, seen, fallback) = (script.clone(), seen.clone(), fallback.clone());
                    async move {
                        seen.lock().unwrap().push(Instant::now());
                        let reply = script.lock().unwrap().pop_front().unwrap_or(fallback);
                        let (status, body, mode, factor) = match reply {
                            Reply::Reset => return Err("reset"),
                            Reply::Silent => {
                                tokio::time::sleep(Duration::from_secs(3600)).await;
                                return Err("silent");
                            }
                            Reply::Ok(m, f) => (200, EMPTY, m, f),
                            Reply::Invalid(m, f) => (200, "{not a set", m, f),
                            Reply::NotModified(m, f) => (304, "", m, f),
                            Reply::Status(s) => (s, "", None, None),
                        };
                        let mut response = Response::builder().status(status);
                        if status == 200 || status == 304 {
                            response = response.header("etag", "\"1-aa\"");
                        }
                        if let Some(mode) = mode {
                            response = response.header("mimic-mode", mode);
                        }
                        if let Some(factor) = factor {
                            response = response.header("mimic-latency-factor", factor);
                        }
                        let body = Full::new(Bytes::from(body)).map_err(|e| match e {}).boxed();
                        Ok(response.body(body).unwrap())
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

/// The sidecar with its `/mimic` volume at `dir/mimic`.
fn sidecar(
    api: &FakeApi,
    dir: &Path,
    interval: &str,
    extra: &[(&str, &str)],
) -> (Process, SocketAddr) {
    let token = dir.join("token");
    std::fs::write(&token, "token").unwrap();
    let url = format!("https://{}", api.addr);
    let trust = dir.join("mimic");
    std::fs::create_dir_all(&trust).unwrap();
    let mut env = vec![
        ("MIMIC_ADMIN_ADDR", "127.0.0.1:0"),
        ("MIMIC_INBOUND_ADDRS", "127.0.0.1:0"),
        ("MIMIC_OUTBOUND_ADDRS", "127.0.0.1:0"),
        ("MIMIC_CONTROLLER_URL", &url),
        ("MIMIC_CONTROLLER_CA", &api.ca_pem),
        ("MIMIC_TLS_TOKEN_FILE", token.to_str().unwrap()),
        ("MIMIC_TRUST_DIR", trust.to_str().unwrap()),
        ("MIMIC_WORKLOAD", "pay"),
        ("MIMIC_EGRESS_PULL_INTERVAL", interval),
    ];
    env.extend_from_slice(extra);
    let mut process = Process::spawn(BIN, &env);
    let admin = process.admin_addr();
    (process, admin)
}

fn state_file(dir: &Path) -> std::path::PathBuf {
    dir.join("mimic/state/mode")
}

fn egress(admin: SocketAddr) -> Value {
    let (status, body) = http_get(admin, "/egress");
    assert_eq!(status, 200);
    serde_json::from_str(&body).unwrap()
}

fn mode_gauge(admin: SocketAddr) -> Option<String> {
    let (_, text) = http_get(admin, "/metrics");
    text.lines()
        .find(|l| l.starts_with("mimic_mock_mode "))
        .map(|l| l.rsplit(' ').next().unwrap().to_string())
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..1500 {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("{what} not reached in 30 s");
}

fn count(process: &mut Process, message: &str) -> usize {
    process
        .logs()
        .iter()
        .filter(|v| v["message"] == message)
        .count()
}

fn line(process: &mut Process, message: &str) -> Value {
    process
        .logs()
        .into_iter()
        .find(|v| v["message"] == message)
        .unwrap()
}

/// `(mode, latency_factor, source)` of each `mode changed` line.
fn changes(process: &mut Process) -> Vec<(String, String, String)> {
    process
        .logs()
        .iter()
        .filter(|v| v["message"] == "mode changed")
        .map(|v| {
            assert_eq!(v["level"], "INFO");
            assert_eq!(v["target"], "mock");
            (
                v["mode"].as_str().unwrap().to_string(),
                v["latency_factor"].as_f64().unwrap().to_string(),
                v["source"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn row(mode: &str, factor: &str, source: &str) -> (String, String, String) {
    (mode.into(), factor.into(), source.into())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mck_10_the_first_pull_arrives_at_start_and_the_env_mode_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let api = fake_api(vec![], Reply::NotModified(None, None)).await;
    let begun = Instant::now();
    let (mut process, admin) = sidecar(
        &api,
        dir.path(),
        "60s",
        &[("MIMIC_MODE", "mock"), ("MIMIC_MOCK_LATENCY_FACTOR", "0.5")],
    );
    wait_for("first pull", || api.count() >= 1);
    let first = api.seen.lock().unwrap()[0];
    assert!(first.duration_since(begun) < Duration::from_secs(1));
    assert_eq!(changes(&mut process), [row("mock", "0.5", "env")]);
    let view = egress(admin);
    assert_eq!(view["mode"], "mock");
    assert_eq!(view["latency_factor"].as_f64(), Some(0.5));
    assert_eq!(mode_gauge(admin).as_deref(), Some("1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mck_10_unset_variables_mean_discovery_factor_1() {
    let dir = tempfile::tempdir().unwrap();
    let api = fake_api(vec![], Reply::Silent).await;
    let (mut process, admin) = sidecar(&api, dir.path(), "60s", &[]);
    wait_for("first pull", || api.count() >= 1);
    assert_eq!(changes(&mut process), [row("discovery", "1", "env")]);
    let view = egress(admin);
    assert_eq!(view["latency_factor"], 1);
    assert_eq!(view["mode"], "discovery");
    assert_eq!(mode_gauge(admin).as_deref(), Some("0"));
    assert!(!state_file(dir.path()).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mck_10_answers_switch_the_mode_and_failures_keep_it() {
    let dir = tempfile::tempdir().unwrap();
    let api = fake_api(
        vec![
            Reply::Ok(Some("mock"), Some("0.5")),
            Reply::NotModified(None, None),
            Reply::NotModified(Some("MOCK"), None),
            Reply::NotModified(Some("MOCK"), None),
            Reply::Status(503),
            Reply::Reset,
            Reply::Status(404),
            Reply::NotModified(Some("mock"), Some("2")),
            Reply::Invalid(Some("discovery"), None),
        ],
        Reply::NotModified(Some("discovery"), None),
    )
    .await;
    let (mut process, admin) = sidecar(&api, dir.path(), "1s", &[]);
    wait_for("mock", || mode_gauge(admin).as_deref() == Some("1"));
    assert_eq!(egress(admin)["latency_factor"], 0.5);
    wait_for("the end of the script", || api.count() >= 10);
    wait_for("discovery", || mode_gauge(admin).as_deref() == Some("0"));
    assert_eq!(
        changes(&mut process),
        [
            row("discovery", "1", "env"),
            row("mock", "0.5", "pull"),
            row("mock", "2", "pull"),
            row("discovery", "2", "pull"),
        ]
    );
    // One streak of unknown values: one line, whatever the number of answers.
    assert_eq!(count(&mut process, "mode unknown"), 1);
    let unknown = line(&mut process, "mode unknown");
    assert_eq!(unknown["level"], "WARN");
    assert_eq!(unknown["value"], "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mck_10_a_controller_without_the_header_keeps_the_env_mode() {
    let dir = tempfile::tempdir().unwrap();
    let api = fake_api(vec![], Reply::Ok(None, None)).await;
    let (mut process, admin) = sidecar(&api, dir.path(), "1s", &[("MIMIC_MODE", "mock")]);
    wait_for("three pulls", || api.count() >= 3);
    assert_eq!(egress(admin)["mode"], "mock");
    assert_eq!(count(&mut process, "mode unknown"), 1);
    assert_eq!(changes(&mut process), [row("mock", "1", "env")]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mck_10_a_switch_is_saved_and_a_restart_resumes_it_while_the_controller_is_silent() {
    let dir = tempfile::tempdir().unwrap();
    let api = fake_api(vec![], Reply::Ok(Some("mock"), Some("0.5"))).await;
    let (process, admin) = sidecar(&api, dir.path(), "1s", &[]);
    wait_for("mock", || mode_gauge(admin).as_deref() == Some("1"));
    assert_eq!(
        std::fs::read_to_string(state_file(dir.path())).unwrap(),
        r#"{"latency_factor":0.5,"mode":"mock"}"#
    );
    drop(process);

    let silent = fake_api(vec![], Reply::Silent).await;
    let (mut again, admin) = sidecar(&silent, dir.path(), "60s", &[("MIMIC_MODE", "discovery")]);
    assert_eq!(changes(&mut again), [row("mock", "0.5", "state")]);
    let view = egress(admin);
    assert_eq!(view["mode"], "mock");
    assert_eq!(view["latency_factor"].as_f64(), Some(0.5));
    assert_eq!(mode_gauge(admin).as_deref(), Some("1"));
    wait_for("the silent pull", || silent.count() >= 1);
    assert_eq!(egress(admin)["mode"], "mock");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mck_10_an_invalid_state_file_is_ignored_with_one_warning() {
    for content in [
        "not json",
        r#"{"mode":"record","latency_factor":1}"#,
        r#"{"mode":"mock","latency_factor":101}"#,
        "",
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("mimic/state")).unwrap();
        std::fs::write(state_file(dir.path()), content).unwrap();
        let api = fake_api(vec![], Reply::Silent).await;
        let (mut process, admin) = sidecar(&api, dir.path(), "60s", &[("MIMIC_MODE", "discovery")]);
        assert_eq!(count(&mut process, "mode state ignored"), 1, "{content:?}");
        assert_eq!(line(&mut process, "mode state ignored")["level"], "WARN");
        assert_eq!(changes(&mut process), [row("discovery", "1", "env")]);
        assert_eq!(egress(admin)["mode"], "discovery");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mck_10_an_unwritable_state_directory_warns_once_and_the_switch_applies() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("mimic")).unwrap();
    // A file where the directory should be.
    std::fs::write(dir.path().join("mimic/state"), "x").unwrap();
    let api = fake_api(vec![], Reply::Ok(Some("mock"), Some("1"))).await;
    let (mut process, admin) = sidecar(&api, dir.path(), "1s", &[]);
    wait_for("mock", || mode_gauge(admin).as_deref() == Some("1"));
    wait_for("three pulls", || api.count() >= 3);
    assert_eq!(egress(admin)["mode"], "mock");
    assert_eq!(count(&mut process, "mode state not saved"), 1);
    assert_eq!(line(&mut process, "mode state not saved")["level"], "WARN");
}

#[test]
fn mck_10_invalid_mode_settings_exit_2_before_listening() {
    for (var, value) in [
        ("MIMIC_MODE", "record"),
        ("MIMIC_MODE", "MOCK"),
        ("MIMIC_MOCK_LATENCY_FACTOR", "-1"),
        ("MIMIC_MOCK_LATENCY_FACTOR", "101"),
        ("MIMIC_MOCK_LATENCY_FACTOR", "abc"),
        ("MIMIC_MOCK_PASSTHROUGH", "*"),
    ] {
        // Loopback and ephemeral ports: a value not rejected must not bind a real port.
        let mut child = std::process::Command::new(BIN)
            .env_clear()
            .env("MIMIC_ADMIN_ADDR", "127.0.0.1:0")
            .env("MIMIC_INBOUND_ADDRS", "127.0.0.1:0")
            .env("MIMIC_OUTBOUND_ADDRS", "127.0.0.1:0")
            .env(var, value)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let status = status.unwrap_or_else(|| panic!("{var}={value} was not rejected in 10 s"));
        assert_eq!(status.code(), Some(2), "{var}={value}");
        let mut stderr = String::new();
        std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
        assert!(stderr.contains(var), "{var}={value}: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mck_10_a_state_path_that_is_not_a_regular_file_is_ignored_without_blocking() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("mimic/state")).unwrap();
    let fifo = state_file(dir.path());
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    let api = fake_api(vec![], Reply::Silent).await;
    let (mut process, admin) = sidecar(&api, dir.path(), "60s", &[]);
    assert_eq!(count(&mut process, "mode state ignored"), 1);
    assert_eq!(egress(admin)["mode"], "discovery");
}

// MON-11: the mode switches and the latency factor as metrics (spec §9, #385).

/// The value of the series whose line starts with `series` (a name with its labels), if any.
fn series(admin: SocketAddr, series: &str) -> Option<f64> {
    let (_, text) = http_get(admin, "/metrics");
    text.lines()
        .find(|l| l.starts_with(series) && l[series.len()..].starts_with(' '))
        .and_then(|l| l.rsplit(' ').next()?.parse().ok())
}

fn switches(admin: SocketAddr, to: &str) -> f64 {
    let name = format!("mimic_mock_mode_switches_total{{to=\"{to}\"}}");
    series(admin, &name).unwrap_or(0.0)
}

fn factor(admin: SocketAddr) -> Option<f64> {
    series(admin, "mimic_mock_latency_factor")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mon_11_the_factor_at_start_is_exposed_and_the_start_mode_is_not_a_switch() {
    let dir = tempfile::tempdir().unwrap();
    let api = fake_api(vec![], Reply::Silent).await;
    let (_process, admin) = sidecar(
        &api,
        dir.path(),
        "60s",
        &[
            ("MIMIC_MODE", "discovery"),
            ("MIMIC_MOCK_LATENCY_FACTOR", "1.5"),
        ],
    );
    assert_eq!(factor(admin), Some(1.5));
    assert_eq!(switches(admin, "mock"), 0.0);
    assert_eq!(switches(admin, "discovery"), 0.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mon_11_each_applied_change_of_mode_counts_once_and_the_factor_follows() {
    let dir = tempfile::tempdir().unwrap();
    let api = fake_api(
        vec![
            Reply::Ok(Some("mock"), None),
            Reply::NotModified(Some("mock"), None),
            Reply::NotModified(Some("mock"), Some("2")),
        ],
        Reply::NotModified(Some("discovery"), None),
    )
    .await;
    let (_process, admin) = sidecar(
        &api,
        dir.path(),
        "1s",
        &[("MIMIC_MOCK_LATENCY_FACTOR", "1.5")],
    );
    wait_for("mock", || mode_gauge(admin).as_deref() == Some("1"));
    assert_eq!(switches(admin, "mock"), 1.0);
    // The third answer changes the factor only: no switch.
    wait_for("the factor 2", || factor(admin) == Some(2.0));
    assert_eq!(switches(admin, "mock"), 1.0);
    wait_for("discovery", || mode_gauge(admin).as_deref() == Some("0"));
    assert_eq!(switches(admin, "mock"), 1.0);
    assert_eq!(switches(admin, "discovery"), 1.0);
    assert_eq!(factor(admin), Some(2.0));
    wait_for("more pulls", || api.count() >= 6);
    assert_eq!(switches(admin, "discovery"), 1.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mon_11_a_restart_with_a_saved_mock_mode_counts_no_switch() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("mimic/state")).unwrap();
    std::fs::write(
        state_file(dir.path()),
        r#"{"latency_factor":0.5,"mode":"mock"}"#,
    )
    .unwrap();
    let api = fake_api(vec![], Reply::Silent).await;
    let (_process, admin) = sidecar(&api, dir.path(), "60s", &[("MIMIC_MODE", "discovery")]);
    assert_eq!(mode_gauge(admin).as_deref(), Some("1"));
    assert_eq!(factor(admin), Some(0.5));
    assert_eq!(switches(admin, "mock"), 0.0);
    assert_eq!(switches(admin, "discovery"), 0.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mon_11_a_switch_whose_state_file_cannot_be_written_is_applied_and_counted_once() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("mimic")).unwrap();
    // A file where the directory should be.
    std::fs::write(dir.path().join("mimic/state"), "x").unwrap();
    let api = fake_api(vec![], Reply::Ok(Some("mock"), None)).await;
    let (_process, admin) = sidecar(&api, dir.path(), "1s", &[]);
    wait_for("three pulls", || api.count() >= 3);
    assert_eq!(mode_gauge(admin).as_deref(), Some("1"));
    assert_eq!(switches(admin, "mock"), 1.0);
}

/// The `mimic_mode` series of `admin`, by mode.
fn mode_series(admin: SocketAddr) -> [Option<f64>; 3] {
    ["none", "discovery", "mock"]
        .map(|mode| series(admin, &format!("mimic_mode{{mode=\"{mode}\"}}")))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_02_the_env_mode_none_is_applied_and_logged() {
    let dir = tempfile::tempdir().unwrap();
    let api = fake_api(vec![], Reply::Silent).await;
    let (mut process, admin) = sidecar(&api, dir.path(), "60s", &[("MIMIC_MODE", "none")]);
    wait_for("first pull", || api.count() >= 1);
    assert_eq!(changes(&mut process), [row("none", "1", "env")]);
    assert_eq!(egress(admin)["mode"], "none");
    assert_eq!(mode_gauge(admin).as_deref(), Some("0"));
    assert_eq!(mode_series(admin), [Some(1.0), Some(0.0), Some(0.0)]);
    assert!(!state_file(dir.path()).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_02_the_state_file_accepts_none_and_a_restart_resumes_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("mimic/state")).unwrap();
    std::fs::write(
        state_file(dir.path()),
        r#"{"latency_factor":1,"mode":"none"}"#,
    )
    .unwrap();
    let api = fake_api(vec![], Reply::Silent).await;
    let (mut process, admin) = sidecar(&api, dir.path(), "60s", &[("MIMIC_MODE", "discovery")]);
    wait_for("first pull", || api.count() >= 1);
    assert_eq!(changes(&mut process), [row("none", "1", "state")]);
    assert_eq!(mode_series(admin), [Some(1.0), Some(0.0), Some(0.0)]);
    assert_eq!(count(&mut process, "mode state ignored"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_05_pulled_switches_between_none_discovery_and_mock_are_applied_and_counted() {
    let dir = tempfile::tempdir().unwrap();
    let api = fake_api(
        vec![
            Reply::Ok(Some("discovery"), None),
            Reply::NotModified(Some("mock"), None),
        ],
        Reply::NotModified(Some("none"), None),
    )
    .await;
    let (mut process, admin) = sidecar(&api, dir.path(), "1s", &[("MIMIC_MODE", "none")]);
    wait_for("the end of the script", || api.count() >= 4);
    wait_for("none again", || {
        mode_series(admin) == [Some(1.0), Some(0.0), Some(0.0)] && switches(admin, "none") == 1.0
    });
    assert_eq!(
        changes(&mut process)
            .into_iter()
            .map(|(mode, _, source)| (mode, source))
            .collect::<Vec<_>>(),
        [
            ("none".to_string(), "env".to_string()),
            ("discovery".to_string(), "pull".to_string()),
            ("mock".to_string(), "pull".to_string()),
            ("none".to_string(), "pull".to_string()),
        ]
    );
    assert_eq!(switches(admin, "discovery"), 1.0);
    assert_eq!(switches(admin, "mock"), 1.0);
    assert_eq!(mode_gauge(admin).as_deref(), Some("0"));
    assert_eq!(egress(admin)["mode"], "none");
    assert_eq!(
        std::fs::read_to_string(state_file(dir.path())).unwrap(),
        r#"{"latency_factor":1,"mode":"none"}"#
    );
    // The previous mode is in the log line.
    let last = process
        .logs()
        .into_iter()
        .rfind(|v| v["message"] == "mode changed")
        .unwrap();
    assert_eq!(last["previous"], "mock");
}
