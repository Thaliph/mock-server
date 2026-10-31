//! TLS-03 (sidecar part), TLS-15, TLS-16 on the real `mimic-sidecar` binary, with a fake
//! sidecar API (spec §7.2) served over TLS in the test process.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use mimic_runtime::testing::{Process, http_get, run, wait_status};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType, SubjectPublicKeyInfo,
};
use time::OffsetDateTime;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

const BIN: &str = env!("CARGO_BIN_EXE_mimic-sidecar");
const CANARY: &str = "canary-token-7f3a91";

fn base_env() -> Vec<(&'static str, String)> {
    vec![
        ("MIMIC_ADMIN_ADDR", "127.0.0.1:0".into()),
        ("MIMIC_INBOUND_ADDRS", "127.0.0.1:0".into()),
        ("MIMIC_OUTBOUND_ADDRS", "127.0.0.1:0".into()),
    ]
}

fn spawn(extra: &[(&'static str, String)]) -> Process {
    let mut env = base_env();
    env.extend_from_slice(extra);
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    Process::spawn(BIN, &env)
}

type Root = Issuer<'static, KeyPair>;

fn ca(name: &str) -> (Root, String) {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, name.to_string());
    let key = KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    (Issuer::new(params, key), cert.pem())
}

#[derive(Clone)]
enum Behaviour {
    /// `503` for every request.
    Fail,
    /// Certify the posted key under this root, valid for this many seconds from now.
    Sign(Arc<Root>, u64),
}

struct Seen {
    at: Instant,
    authorization: String,
    path: String,
    body: serde_json::Value,
}

struct FakeApi {
    addr: SocketAddr,
    ca_pem: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn at(secs: u64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(secs as i64).unwrap()
}

fn sign(root: &Root, body: &serde_json::Value, secs: u64) -> String {
    let der = STANDARD
        .decode(body["public_key"].as_str().unwrap())
        .unwrap();
    let spki = SubjectPublicKeyInfo::from_der(&der).unwrap();
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "mimic intermediate".to_string());
    params.not_before = at(now());
    params.not_after = at(now() + secs);
    params.signed_by(&spki, root).unwrap().pem()
}

async fn fake_api(behaviour: Arc<Mutex<Behaviour>>) -> FakeApi {
    let (ca_issuer, ca_pem) = ca("controller ca");
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::default();
    params.subject_alt_names = vec![SanType::IpAddress("127.0.0.1".parse().unwrap())];
    let cert = params.signed_by(&key, &ca_issuer).unwrap();
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
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, behaviour, log) = (acceptor.clone(), behaviour.clone(), log.clone());
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let service = service_fn(move |req: Request<Incoming>| {
                    let (behaviour, log) = (behaviour.clone(), log.clone());
                    async move {
                        let authorization = req
                            .headers()
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        let path = req.uri().path().to_string();
                        let bytes = req.into_body().collect().await.unwrap().to_bytes();
                        let body = serde_json::from_slice(&bytes).unwrap_or_default();
                        let current = behaviour.lock().unwrap().clone();
                        let (status, out) = match current {
                            Behaviour::Fail => (503, String::new()),
                            Behaviour::Sign(root, secs) => (
                                200,
                                serde_json::json!({"certificate": sign(&root, &body, secs)})
                                    .to_string(),
                            ),
                        };
                        log.lock().unwrap().push(Seen {
                            at: Instant::now(),
                            authorization,
                            path,
                            body,
                        });
                        Ok::<_, std::convert::Infallible>(
                            Response::builder()
                                .status(status)
                                .body(Full::new(Bytes::from(out)))
                                .unwrap(),
                        )
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

struct Setup {
    api: FakeApi,
    root_pem: String,
    behaviour: Arc<Mutex<Behaviour>>,
    dir: tempfile::TempDir,
}

impl Setup {
    async fn new(behaviour: Behaviour) -> Setup {
        let (_, root_pem) = ca("deployment root");
        Setup::with_root(behaviour, root_pem).await
    }

    /// The fake API signs with `root` when the behaviour says so; `root_pem` is what the
    /// sidecar is told to trust.
    async fn with_root(behaviour: Behaviour, root_pem: String) -> Setup {
        let behaviour = Arc::new(Mutex::new(behaviour));
        let api = fake_api(behaviour.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("token"), CANARY).unwrap();
        Setup {
            api,
            root_pem,
            behaviour,
            dir,
        }
    }

    fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("MIMIC_TLS_CA", self.root_pem.clone()),
            ("MIMIC_CONTROLLER_URL", format!("https://{}", self.api.addr)),
            ("MIMIC_CONTROLLER_CA", self.api.ca_pem.clone()),
            (
                "MIMIC_TLS_TOKEN_FILE",
                self.dir.path().join("token").display().to_string(),
            ),
            (
                "MIMIC_TRUST_DIR",
                self.dir.path().join("trust").display().to_string(),
            ),
            ("MIMIC_WORKLOAD", "pay".into()),
            ("MIMIC_LOG", "debug".into()),
        ]
    }

    fn requests(&self) -> usize {
        self.api.seen.lock().unwrap().len()
    }
}

/// A root, and a setup whose fake API signs with it for `secs` seconds.
async fn signing(secs: u64) -> Setup {
    let (root, root_pem) = ca("deployment root");
    Setup::with_root(Behaviour::Sign(Arc::new(root), secs), root_pem).await
}

/// Value of the sample `name` (with its labels, if any) in the OpenMetrics text.
fn sample(metrics: &str, name: &str) -> Option<f64> {
    metrics
        .lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.trim().parse().ok())
}

async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

const EXPIRY: &str = "mimic_tls_intermediate_expiry_timestamp_seconds";
const OK: &str = "mimic_tls_intermediate_requests_total{result=\"ok\"}";
const ERR: &str = "mimic_tls_intermediate_requests_total{result=\"error\"}";
const MISMATCH: &str = "mimic_tls_intermediate_requests_total{result=\"mismatch\"}";

#[tokio::test(flavor = "multi_thread")]
async fn tls_15_ready_before_an_intermediate_and_retries_with_backoff() {
    let setup = Setup::new(Behaviour::Fail).await;
    let mut sidecar = spawn(&setup.env());
    let admin = sidecar.admin_addr();
    wait_status(admin, "/readyz", 200, Duration::from_secs(5));
    // Failures at about 0, 1 and 3 s: the TLS-05 backoff, not a busy loop.
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let n = setup.requests();
    assert!((3..=4).contains(&n), "{n} requests");
    let metrics = http_get(admin, "/metrics").1;
    assert!(sample(&metrics, ERR).unwrap() >= 3.0, "{metrics}");
    assert_eq!(sample(&metrics, EXPIRY), Some(0.0), "{metrics}");
}

#[tokio::test(flavor = "multi_thread")]
async fn tls_15_accepts_an_intermediate_of_its_root_and_sends_token_and_key() {
    let setup = signing(3600).await;
    let mut sidecar = spawn(&setup.env());
    let admin = sidecar.admin_addr();
    eventually("intermediate", || {
        sample(&http_get(admin, "/metrics").1, OK) == Some(1.0)
    })
    .await;
    let metrics = http_get(admin, "/metrics").1;
    let expiry = sample(&metrics, EXPIRY).unwrap() as u64;
    assert!(expiry > now() + 3000 && expiry <= now() + 3600, "{expiry}");
    {
        let seen = setup.api.seen.lock().unwrap();
        assert_eq!(seen[0].path, "/v1/tls/intermediate");
        assert_eq!(seen[0].authorization, format!("Bearer {CANARY}"));
        assert_eq!(seen[0].body["deployment"], "pay");
        let key = STANDARD
            .decode(seen[0].body["public_key"].as_str().unwrap())
            .unwrap();
        assert!(!key.is_empty());
    }
    sidecar.terminate();
    sidecar.wait_exit(Duration::from_secs(10));
    for line in sidecar.lines() {
        assert!(!line.contains(CANARY), "{line}");
        assert!(!line.contains("PRIVATE KEY"), "{line}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tls_15_renews_at_half_life_with_the_current_token() {
    let setup = signing(4).await;
    let _sidecar = spawn(&setup.env());
    eventually("first request", || setup.requests() >= 1).await;
    std::fs::write(setup.dir.path().join("token"), "second-token").unwrap();
    eventually("renewal", || setup.requests() >= 2).await;
    let seen = setup.api.seen.lock().unwrap();
    let gap = seen[1].at - seen[0].at;
    assert!(
        gap >= Duration::from_millis(1500) && gap <= Duration::from_millis(3800),
        "{gap:?}"
    );
    assert_eq!(seen[1].authorization, "Bearer second-token");
}

#[tokio::test(flavor = "multi_thread")]
async fn tls_15_refuses_an_intermediate_of_another_root() {
    // The API signs with one root while the sidecar is told to trust another.
    let (other, _) = ca("other root");
    let (_, trusted) = ca("deployment root");
    let setup = Setup::with_root(Behaviour::Sign(Arc::new(other), 3600), trusted).await;
    let mut sidecar = spawn(&setup.env());
    let admin = sidecar.admin_addr();
    let line = sidecar.wait_log(Duration::from_secs(10), |v| {
        v["message"] == "tls ca mismatch"
    });
    assert_eq!(line["level"], "ERROR");
    eventually("mismatch metric", || {
        sample(&http_get(admin, "/metrics").1, MISMATCH) >= Some(1.0)
    })
    .await;
    let metrics = http_get(admin, "/metrics").1;
    assert_eq!(sample(&metrics, EXPIRY), Some(0.0), "{metrics}");
    let _ = &setup.behaviour;
}

#[test]
fn tls_15_missing_controller_url_exits_2() {
    let (status, _, stderr) = run(
        BIN,
        &[],
        &[
            ("MIMIC_TLS_CA", "-----BEGIN CERTIFICATE-----"),
            ("MIMIC_CONTROLLER_CA", "x"),
        ],
    );
    assert_eq!(status.code(), Some(2));
    assert!(stderr.contains("MIMIC_CONTROLLER_URL"), "{stderr}");
}

#[test]
fn tls_03_invalid_exclusion_exits_2() {
    for bad in ["a.*.com", "*", ",x.test", "-bad-"] {
        let (status, _, stderr) = run(BIN, &[], &[("MIMIC_TLS_EXCLUDE", bad)]);
        assert_eq!(status.code(), Some(2), "{bad}");
        assert!(stderr.contains("MIMIC_TLS_EXCLUDE"), "{stderr}");
    }
}

#[test]
fn tls_16_core_dumps_are_off_and_the_process_is_not_dumpable() {
    let mut sidecar = spawn(&[]);
    let admin = sidecar.admin_addr();
    wait_status(admin, "/readyz", 200, Duration::from_secs(5));
    let pid = sidecar.pid();
    let limits = std::fs::read_to_string(format!("/proc/{pid}/limits")).unwrap();
    let core = limits
        .lines()
        .find(|l| l.starts_with("Max core file size"))
        .unwrap();
    let values: Vec<&str> = core.split_whitespace().skip(4).collect();
    assert_eq!(values[..2], ["0", "0"], "{core}");
    use std::os::unix::fs::MetadataExt;
    let owner = std::fs::metadata(format!("/proc/{pid}/environ"))
        .unwrap()
        .uid();
    assert_eq!(owner, 0);
}

#[test]
fn trust_dir_receives_the_binary_and_the_fallback_store() {
    let src = tempfile::tempdir().unwrap();
    let bin = src.path().join("mimic-trust");
    let store = src.path().join("ca.crt");
    std::fs::write(&bin, b"\x7fELF binary").unwrap();
    std::fs::write(&store, "store").unwrap();
    let out = tempfile::tempdir().unwrap();
    mimic_sidecar::tls::publish_trust(out.path(), &bin, &[src.path().join("missing"), store])
        .unwrap();
    use std::os::unix::fs::PermissionsExt;
    let copied = out.path().join("bin/mimic-trust");
    assert_eq!(std::fs::read(&copied).unwrap(), b"\x7fELF binary");
    assert_eq!(
        std::fs::metadata(&copied).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        std::fs::read_to_string(out.path().join("bin/fallback-ca.pem")).unwrap(),
        "store"
    );
}
