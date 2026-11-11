//! A fake load API (served over TLS) and fake targets (probe on `:15100` and a request port)
//! around the real `mimic-load` binary. The fakes run on a runtime owned by the rig, so the
//! tests themselves are plain synchronous functions.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use mimic_core::load::plan::{Body, CredentialField, Endpoint, Plan, Sample};
use mimic_runtime::testing::Process;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

pub const BIN: &str = env!("CARGO_BIN_EXE_mimic-load");
pub const TOKEN: &str = "load-token-7a1c";
pub const TICKET: &str = "v1.ticket-payload-3f9b.ticket-signature-c2d8";
pub const NAMESPACE: &str = "shop";
pub const NAME: &str = "payment-load";
pub const UID: &str = "3f2b8c1e-0000-4000-8000-000000000001";
pub const DEPLOYMENT: &str = "payment";
pub const REQUEST_PORT: u16 = 18080;

/// Loopback addresses no other test (or test process) uses: the probe port is fixed.
pub fn ips(n: usize) -> Vec<IpAddr> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let pid = std::process::id();
    let second = 16 + (pid % 200);
    let third = NEXT.fetch_add(1, Ordering::Relaxed) % 250;
    (0..n)
        .map(|i| format!("127.{second}.{third}.{}", i + 1).parse().unwrap())
        .collect()
}

pub fn pod_name(ip: IpAddr) -> String {
    format!("payment-{}", ip.to_string().replace('.', "-"))
}

// --- the plan ---------------------------------------------------------------------------

pub fn sample(path: &str, headers: &[(&str, &str)], body: Option<Body>) -> Sample {
    Sample {
        hash: 1,
        path: path.to_string(),
        query: vec![],
        headers: headers
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect(),
        body,
    }
}

pub fn endpoint(method: &str, template: &str, weight: u64, samples: Vec<Sample>) -> Endpoint {
    Endpoint {
        method: method.to_string(),
        port: REQUEST_PORT,
        template: template.to_string(),
        host: None,
        weight,
        volatile: vec![],
        samples,
    }
}

pub fn simple_plan() -> Plan {
    Plan {
        credentials: vec![],
        endpoints: vec![endpoint(
            "GET",
            "/items",
            1,
            vec![sample("/items", &[], None)],
        )],
    }
}

#[derive(Clone, Debug)]
pub struct Params {
    pub concurrency: u64,
    pub rate: Option<u64>,
    pub duration_ms: u64,
    pub ramp_up_ms: u64,
    pub timeout_ms: u64,
    pub start_timeout_ms: u64,
}

impl Params {
    pub fn closed(concurrency: u64, duration_ms: u64) -> Params {
        Params {
            concurrency,
            rate: None,
            duration_ms,
            ramp_up_ms: 0,
            timeout_ms: 5_000,
            start_timeout_ms: 30_000,
        }
    }

    pub fn open(concurrency: u64, rate: u64, duration_ms: u64) -> Params {
        Params {
            rate: Some(rate),
            ..Params::closed(concurrency, duration_ms)
        }
    }
}

/// The body of `GET …/plan`.
pub fn plan_answer(plan: &Plan, params: &Params) -> String {
    json!({
        "plan": plan,
        "ticket": TICKET,
        "worker": 0,
        "workers": 1,
        "params": {
            "concurrency": params.concurrency,
            "rate": params.rate,
            "durationMs": params.duration_ms,
            "rampUpMs": params.ramp_up_ms,
            "timeoutMs": params.timeout_ms,
            "startTimeoutMs": params.start_timeout_ms,
        },
    })
    .to_string()
}

pub fn credential(field: &str, key: &str) -> CredentialField {
    CredentialField {
        field: field.to_string(),
        key: key.to_string(),
    }
}

// --- the fake load API ----------------------------------------------------------------------

pub struct Post {
    pub at: Instant,
    pub raw: String,
    pub body: Value,
}

#[derive(Default)]
pub struct ApiState {
    pub plan: Mutex<String>,
    pub targets: Mutex<Vec<(IpAddr, String)>>,
    /// `stop` in the targets and results answers.
    pub stop: AtomicBool,
    /// Every route answers `503`.
    pub down: AtomicBool,
    /// The run ended: `targets` and `results` answer `404`.
    pub ended: AtomicBool,
    /// Results posts received after the end.
    pub late: std::sync::atomic::AtomicUsize,
    pub posts: Mutex<Vec<Post>>,
    pub authorizations: Mutex<Vec<String>>,
}

impl ApiState {
    pub fn posts(&self) -> Vec<Value> {
        self.posts
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.body.clone())
            .collect()
    }

    pub fn raw_posts(&self) -> Vec<String> {
        self.posts
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.raw.clone())
            .collect()
    }

    pub fn final_post(&self) -> Option<Value> {
        self.posts().into_iter().rev().find(|p| p["final"] == true)
    }

    pub fn set_targets(&self, targets: &[Target]) {
        *self.targets.lock().unwrap() = targets.iter().map(|t| (t.ip, t.pod.clone())).collect();
    }
}

pub struct Api {
    pub addr: SocketAddr,
    pub ca_pem: String,
    pub state: Arc<ApiState>,
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

async fn start_api() -> Api {
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
    let state = Arc::new(ApiState::default());
    let shared = state.clone();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, state) = (acceptor.clone(), shared.clone());
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let service = service_fn(move |req| api_route(state.clone(), req));
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), service)
                    .await;
            });
        }
    });
    Api {
        addr,
        ca_pem,
        state,
    }
}

fn reply(status: u16, body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

async fn api_route(
    state: Arc<ApiState>,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let authorization = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    state
        .authorizations
        .lock()
        .unwrap()
        .push(authorization.clone());
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    let raw = req.into_body().collect().await.map(|b| b.to_bytes());
    if state.down.load(Ordering::SeqCst) {
        return Ok(reply(503, "{}".into()));
    }
    if authorization != format!("Bearer {TOKEN}") {
        return Ok(reply(401, "{}".into()));
    }
    let prefix = format!("/v1/load/{NAME}/{UID}/");
    let Some(route) = path.strip_prefix(&prefix) else {
        return Ok(reply(404, "{}".into()));
    };
    let stop = state.stop.load(Ordering::SeqCst);
    if state.ended.load(Ordering::SeqCst) && route != "plan" {
        if route == "results" {
            state.late.fetch_add(1, Ordering::SeqCst);
        }
        return Ok(reply(404, "{}".into()));
    }
    Ok(match (method.as_str(), route) {
        ("GET", "plan") => reply(200, state.plan.lock().unwrap().clone()),
        ("GET", "targets") => {
            let targets: Vec<Value> = state
                .targets
                .lock()
                .unwrap()
                .iter()
                .map(|(ip, pod)| json!({"ip": ip.to_string(), "pod": pod}))
                .collect();
            reply(200, json!({"stop": stop, "targets": targets}).to_string())
        }
        ("POST", "results") => {
            let raw = String::from_utf8_lossy(&raw.unwrap_or_default()).into_owned();
            match serde_json::from_str::<Value>(&raw) {
                Ok(body) => {
                    state.posts.lock().unwrap().push(Post {
                        at: Instant::now(),
                        raw,
                        body,
                    });
                    reply(200, json!({"stop": stop}).to_string())
                }
                Err(_) => reply(400, "{}".into()),
            }
        }
        _ => reply(404, "{}".into()),
    })
}

// --- the fake targets -----------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    Good,
    OtherDeployment,
    OtherPod,
    Discovery,
    NotFound,
    Silent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Ok,
    Refused(&'static str),
    /// `200` without `x-mimic-load`.
    NoMarker,
}

pub struct Seen {
    pub at: Instant,
    pub method: String,
    pub target: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

pub struct TargetState {
    pub probe: Mutex<Probe>,
    pub answer: Mutex<Answer>,
    pub delay: Mutex<Duration>,
    /// Put this text in the answers' headers and body (a canary).
    pub echo: Mutex<Option<String>>,
    pub keep_bodies: AtomicBool,
    /// Connections accepted on the request port.
    pub accepted: AtomicUsize,
    pub probes: AtomicUsize,
    pub in_flight: AtomicUsize,
    pub max_in_flight: AtomicUsize,
    pub count: AtomicUsize,
    pub seen: Mutex<Vec<Seen>>,
    /// When the first non-`ok` answer was sent.
    pub first_refusal: Mutex<Option<Instant>>,
    pub last_request: Mutex<Option<Instant>>,
}

#[derive(Clone)]
pub struct Target {
    pub ip: IpAddr,
    pub pod: String,
    pub state: Arc<TargetState>,
}

impl Target {
    pub fn set_probe(&self, probe: Probe) {
        *self.state.probe.lock().unwrap() = probe;
    }

    pub fn set_answer(&self, answer: Answer) {
        *self.state.answer.lock().unwrap() = answer;
    }

    pub fn set_delay(&self, delay: Duration) {
        *self.state.delay.lock().unwrap() = delay;
    }

    pub fn count(&self) -> usize {
        self.state.count.load(Ordering::SeqCst)
    }

    pub fn accepted(&self) -> usize {
        self.state.accepted.load(Ordering::SeqCst)
    }

    pub fn max_in_flight(&self) -> usize {
        self.state.max_in_flight.load(Ordering::SeqCst)
    }
}

async fn start_target(ip: IpAddr, probe: Probe) -> Target {
    let pod = pod_name(ip);
    let state = Arc::new(TargetState {
        probe: Mutex::new(probe),
        answer: Mutex::new(Answer::Ok),
        delay: Mutex::new(Duration::ZERO),
        echo: Mutex::new(None),
        keep_bodies: AtomicBool::new(false),
        accepted: AtomicUsize::new(0),
        probes: AtomicUsize::new(0),
        in_flight: AtomicUsize::new(0),
        max_in_flight: AtomicUsize::new(0),
        count: AtomicUsize::new(0),
        seen: Mutex::new(Vec::new()),
        first_refusal: Mutex::new(None),
        last_request: Mutex::new(None),
    });
    let probe_listener = TcpListener::bind(SocketAddr::new(ip, 15100)).await.unwrap();
    let request_listener = TcpListener::bind(SocketAddr::new(ip, REQUEST_PORT))
        .await
        .unwrap();
    let (st, name) = (state.clone(), pod.clone());
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = probe_listener.accept().await else {
                return;
            };
            let (st, name) = (st.clone(), name.clone());
            tokio::spawn(async move {
                let service = service_fn(move |_req: Request<Incoming>| {
                    let (st, name) = (st.clone(), name.clone());
                    async move { Ok::<_, Infallible>(probe_answer(&st, &name).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tcp), service)
                    .await;
            });
        }
    });
    let st = state.clone();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = request_listener.accept().await else {
                return;
            };
            st.accepted.fetch_add(1, Ordering::SeqCst);
            let st = st.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let st = st.clone();
                    async move { Ok::<_, Infallible>(request_answer(&st, req).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tcp), service)
                    .await;
            });
        }
    });
    Target { ip, pod, state }
}

async fn probe_answer(state: &TargetState, pod: &str) -> Response<Full<Bytes>> {
    state.probes.fetch_add(1, Ordering::SeqCst);
    let probe = *state.probe.lock().unwrap();
    let body = |deployment: &str, pod: &str, mode: &str| {
        json!({"deployment": deployment, "mode": mode, "namespace": NAMESPACE, "pod": pod})
            .to_string()
    };
    match probe {
        Probe::Good => reply(200, body(DEPLOYMENT, pod, "mock")),
        Probe::OtherDeployment => reply(200, body("billing", pod, "mock")),
        Probe::OtherPod => reply(200, body(DEPLOYMENT, "payment-somewhere-else", "mock")),
        Probe::Discovery => reply(200, body(DEPLOYMENT, pod, "discovery")),
        Probe::NotFound => reply(404, "{}".into()),
        Probe::Silent => {
            tokio::time::sleep(Duration::from_secs(30)).await;
            reply(200, body(DEPLOYMENT, pod, "mock"))
        }
    }
}

async fn request_answer(state: &TargetState, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let now = Instant::now();
    let current = state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    state.max_in_flight.fetch_max(current, Ordering::SeqCst);
    let (parts, body) = req.into_parts();
    let body = body
        .collect()
        .await
        .map(|b| b.to_bytes())
        .unwrap_or_default();
    let keep = state.keep_bodies.load(Ordering::SeqCst);
    state.seen.lock().unwrap().push(Seen {
        at: now,
        method: parts.method.to_string(),
        target: parts.uri.to_string(),
        headers: parts
            .headers
            .iter()
            .map(|(n, v)| {
                (
                    n.as_str().to_string(),
                    v.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect(),
        body: if keep { body.to_vec() } else { Vec::new() },
    });
    drop(body);
    state.count.fetch_add(1, Ordering::SeqCst);
    *state.last_request.lock().unwrap() = Some(now);
    let delay = *state.delay.lock().unwrap();
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    state.in_flight.fetch_sub(1, Ordering::SeqCst);
    let answer = *state.answer.lock().unwrap();
    if answer != Answer::Ok {
        state.first_refusal.lock().unwrap().get_or_insert(now);
    }
    let echo = state.echo.lock().unwrap().clone();
    let mut response = Response::builder();
    let text = echo.clone().unwrap_or_else(|| "ok".to_string());
    if let Some(echo) = &echo {
        response = response.header("x-echo", echo.as_str());
    }
    match answer {
        Answer::Ok => response
            .status(200)
            .header("x-mimic-load", "ok")
            .body(Full::new(Bytes::from(text))),
        Answer::Refused(reason) => response
            .status(599)
            .header("x-mimic-load", format!("refused; reason={reason}"))
            .body(Full::new(Bytes::new())),
        Answer::NoMarker => response.status(200).body(Full::new(Bytes::from(text))),
    }
    .unwrap()
}

// --- the rig --------------------------------------------------------------------------------

pub struct Rig {
    rt: tokio::runtime::Runtime,
    pub api: Api,
    pub targets: Vec<Target>,
    pub dir: tempfile::TempDir,
}

impl Rig {
    /// An API serving `plan`, and one target per probe behaviour, all announced by the API.
    pub fn new(plan: &Plan, params: &Params, probes: &[Probe]) -> Rig {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let api = rt.block_on(start_api());
        *api.state.plan.lock().unwrap() = plan_answer(plan, params);
        let addresses = ips(probes.len());
        let targets: Vec<Target> = addresses
            .iter()
            .zip(probes)
            .map(|(ip, probe)| rt.block_on(start_target(*ip, *probe)))
            .collect();
        api.state.set_targets(&targets);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("token"), format!("{TOKEN}\n")).unwrap();
        Rig {
            rt,
            api,
            targets,
            dir,
        }
    }

    /// Runs a future on the fakes' runtime.
    pub fn block_on<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        self.rt.block_on(future)
    }

    pub fn state(&self) -> &Arc<ApiState> {
        &self.api.state
    }

    pub fn credentials_dir(&self) -> std::path::PathBuf {
        let dir = self.dir.path().join("credentials");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The variables of the Job (spec §10.6): literal values only, so no `MIMIC_POD_NAME`; the
    /// generator names itself by the host name, as in the Job.
    pub fn vars(&self, interval: &str) -> Vec<(String, String)> {
        let token = self.dir.path().join("token");
        [
            ("MIMIC_LOAD_RUN", format!("{NAMESPACE}/{NAME}/{UID}")),
            ("MIMIC_LOAD_DEPLOYMENT", DEPLOYMENT.to_string()),
            (
                "MIMIC_CONTROLLER_URL",
                format!("https://127.0.0.1:{}", self.api.addr.port()),
            ),
            ("MIMIC_CONTROLLER_CA", self.api.ca_pem.clone()),
            ("MIMIC_LOAD_TOKEN_FILE", token.to_str().unwrap().to_string()),
            ("MIMIC_LOAD_REPORT_INTERVAL", interval.to_string()),
            ("MIMIC_ADMIN_ADDR", "127.0.0.1:0".to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    /// The generator, with the variables of the Job and `extra` (which wins).
    pub fn spawn(&self, interval: &str, extra: &[(&str, &str)]) -> Process {
        let mut env = self.vars(interval);
        env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        Process::spawn(BIN, &env)
    }
}

/// The number that follows `field:` in the process status (`VmHWM`), in kB.
pub fn status_kb(pid: u32, field: &str) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|l| l.starts_with(field))?;
    line.split_whitespace().nth(1)?.parse().ok()
}
