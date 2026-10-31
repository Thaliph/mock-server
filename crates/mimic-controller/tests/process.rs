//! FND-01, FND-04, FND-05, FND-06, FND-07 on the real `mimic-controller` binary.
//!
//! Valkey is replaced by a minimal RESP server that answers `PING` with `PONG` and anything
//! else with `OK`, and records the commands it receives.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use mimic_runtime::testing::{Process, http_get, run, wait_status};

const BIN: &str = env!("CARGO_BIN_EXE_mimic-controller");
const ADMIN: (&str, &str) = ("MIMIC_ADMIN_ADDR", "127.0.0.1:0");
/// Required by the webhook (spec §10.2); the listener takes any free port.
const WEBHOOK: [(&str, &str); 8] = [
    ("MIMIC_WEBHOOK_ADDR", "127.0.0.1:0"),
    ("MIMIC_NAMESPACE", "mimic-system"),
    ("MIMIC_WEBHOOK_SERVICE", "mimic"),
    ("MIMIC_WEBHOOK_SECRET", "mimic-webhook-tls"),
    ("MIMIC_WEBHOOK_CONFIGURATION", "mimic-injector"),
    ("MIMIC_TLS_CA_SECRET", "mimic-tls-ca"),
    ("MIMIC_INJECT_SIDECAR_IMAGE", "mimic-sidecar:test"),
    ("MIMIC_INJECT_INIT_IMAGE", "mimic-init:test"),
];

/// The admin address, the webhook settings and `extra`.
fn env<'a>(extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    [&[ADMIN][..], &WEBHOOK, extra].concat()
}

struct FakeValkey {
    addr: SocketAddr,
    commands: Arc<Mutex<Vec<Vec<String>>>>,
    stop: Arc<AtomicBool>,
    clients: Arc<Mutex<Vec<TcpStream>>>,
}

impl FakeValkey {
    fn start(addr: SocketAddr) -> FakeValkey {
        let listener = TcpListener::bind(addr).unwrap();
        listener.set_nonblocking(true).unwrap();
        let fake = FakeValkey {
            addr: listener.local_addr().unwrap(),
            commands: Arc::default(),
            stop: Arc::default(),
            clients: Arc::default(),
        };
        let (commands, stop, clients) = (
            fake.commands.clone(),
            fake.stop.clone(),
            fake.clients.clone(),
        );
        thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        clients.lock().unwrap().push(stream.try_clone().unwrap());
                        let commands = commands.clone();
                        thread::spawn(move || serve(stream, commands));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        fake
    }

    fn stop(self) -> SocketAddr {
        self.stop.store(true, Ordering::SeqCst);
        for client in self.clients.lock().unwrap().iter() {
            let _ = client.shutdown(Shutdown::Both);
        }
        // Let the accept loop notice the flag and drop the listener.
        thread::sleep(Duration::from_millis(100));
        self.addr
    }
}

fn serve(stream: TcpStream, commands: Arc<Mutex<Vec<Vec<String>>>>) {
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    while let Some(command) = read_command(&mut reader) {
        let reply: &[u8] = if command
            .first()
            .is_some_and(|c| c.eq_ignore_ascii_case("PING"))
        {
            b"+PONG\r\n"
        } else {
            b"+OK\r\n"
        };
        commands.lock().unwrap().push(command);
        if writer.write_all(reply).is_err() {
            return;
        }
    }
}

fn read_command(reader: &mut BufReader<TcpStream>) -> Option<Vec<String>> {
    let mut line = String::new();
    reader.read_line(&mut line).ok().filter(|n| *n > 0)?;
    let count: usize = line.trim().strip_prefix('*')?.parse().ok()?;
    let mut args = Vec::with_capacity(count);
    for _ in 0..count {
        line.clear();
        reader.read_line(&mut line).ok()?;
        let len: usize = line.trim().strip_prefix('$')?.parse().ok()?;
        let mut buf = vec![0; len + 2];
        reader.read_exact(&mut buf).ok()?;
        buf.truncate(len);
        args.push(String::from_utf8_lossy(&buf).into_owned());
    }
    Some(args)
}

fn free_addr() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

#[test]
fn fnd_01_missing_valkey_url_exits_2() {
    let (status, _, stderr) = run(BIN, &[], &env(&[]));
    assert_eq!(status.code(), Some(2));
    assert!(stderr.contains("MIMIC_VALKEY_URL"), "{stderr}");
}

#[test]
fn fnd_01_invalid_valkey_url_exits_2() {
    let (status, _, stderr) = run(BIN, &[], &env(&[("MIMIC_VALKEY_URL", "http://nope")]));
    assert_eq!(status.code(), Some(2));
    assert!(
        stderr.contains("MIMIC_VALKEY_URL") && stderr.contains("http://nope"),
        "{stderr}"
    );
}

#[test]
fn fnd_01_credentials_in_the_url_are_refused() {
    let (status, _, stderr) = run(
        BIN,
        &[],
        &env(&[("MIMIC_VALKEY_URL", "redis://:hunter2@127.0.0.1:6379")]),
    );
    assert_eq!(status.code(), Some(2));
    assert!(stderr.contains("MIMIC_VALKEY_PASSWORD"), "{stderr}");
    assert!(!stderr.contains("hunter2"), "{stderr}");
}

#[test]
fn fnd_06_ready_only_while_valkey_answers() {
    let kube = FakeKube::start("fnd06a");
    let valkey_addr = free_addr();
    let url = format!("redis://{valkey_addr}");
    let mut controller = Process::spawn(
        BIN,
        &env(&[("MIMIC_VALKEY_URL", &url), ("KUBECONFIG", &kube.kubeconfig)]),
    );
    let addr = controller.admin_addr();

    // Valkey down at start: alive but not ready, and still running.
    assert_eq!(http_get(addr, "/healthz").0, 200);
    wait_status(addr, "/readyz", 503, Duration::from_secs(5));

    let valkey = FakeValkey::start(valkey_addr);
    wait_status(addr, "/readyz", 200, Duration::from_secs(10));

    let valkey_addr = valkey.stop();
    wait_status(addr, "/readyz", 503, Duration::from_secs(10));
    assert_eq!(http_get(addr, "/healthz").0, 200);

    let _valkey = FakeValkey::start(valkey_addr);
    wait_status(addr, "/readyz", 200, Duration::from_secs(10));

    controller.wait_log(Duration::from_secs(1), |v| {
        v["level"] == "WARN" && v["message"] == "valkey unreachable"
    });
    controller.wait_log(Duration::from_secs(1), |v| {
        v["message"] == "valkey reachable"
    });
}

#[test]
fn fnd_06_the_password_is_sent_and_never_logged() {
    let valkey = FakeValkey::start("127.0.0.1:0".parse().unwrap());
    let url = format!("redis://{}", valkey.addr);
    let kube = FakeKube::start("fnd06b");
    let mut controller = Process::spawn(
        BIN,
        &env(&[
            ("MIMIC_VALKEY_URL", &url),
            ("MIMIC_VALKEY_PASSWORD", "hunter2"),
            ("KUBECONFIG", &kube.kubeconfig),
            ("MIMIC_LOG", "trace"),
        ]),
    );
    let addr = controller.admin_addr();
    wait_status(addr, "/readyz", 200, Duration::from_secs(10));

    let commands = valkey.commands.lock().unwrap().clone();
    assert!(
        commands.iter().any(
            |c| c.first().is_some_and(|n| n.eq_ignore_ascii_case("AUTH"))
                && c.last().is_some_and(|p| p == "hunter2")
        ),
        "{commands:?}"
    );
    controller.terminate();
    controller.wait_exit(Duration::from_secs(10));
    assert!(controller.lines().iter().all(|l| !l.contains("hunter2")));
}

#[test]
fn fnd_05_sigterm_exits_0() {
    let kube = FakeKube::start("fnd05");
    let valkey = FakeValkey::start("127.0.0.1:0".parse().unwrap());
    let url = format!("redis://{}", valkey.addr);
    let mut controller = Process::spawn(
        BIN,
        &env(&[
            ("MIMIC_VALKEY_URL", &url),
            ("MIMIC_SHUTDOWN_GRACE", "2s"),
            ("KUBECONFIG", &kube.kubeconfig),
        ]),
    );
    let addr = controller.admin_addr();
    wait_status(addr, "/readyz", 200, Duration::from_secs(10));
    controller.terminate();
    assert!(controller.wait_exit(Duration::from_secs(3)).success());
    controller.wait_log(Duration::from_secs(1), |v| {
        v["message"] == "shutdown complete"
    });
}

#[test]
fn fnd_04_controller_logs_are_json() {
    let mut controller = Process::spawn(BIN, &env(&[("MIMIC_VALKEY_URL", "redis://127.0.0.1:1")]));
    controller.admin_addr();
    controller.terminate();
    controller.wait_exit(Duration::from_secs(10));
    for line in controller.lines() {
        let v: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"));
        assert!(
            v.get("timestamp").is_some() && v.get("message").is_some(),
            "{line}"
        );
    }
}

#[test]
fn fnd_07_version_flag_prints_the_crate_version() {
    let (status, stdout, _) = run(BIN, &["--version"], &[]);
    assert!(status.success());
    assert_eq!(
        stdout.trim(),
        format!("mimic-controller {}", env!("CARGO_PKG_VERSION"))
    );
}

/// A minimal Kubernetes API: the webhook Secret and configuration, and an empty list of
/// `MimicWorkload`s. Requests it does not know are answered 404 and recorded.
struct FakeKube {
    kubeconfig: String,
    state: Arc<Mutex<KubeState>>,
}

struct KubeState {
    secret_data: serde_json::Value,
    secret_version: u64,
    /// The TLS seed Secret (empty at first) and its version.
    seed_data: serde_json::Value,
    seed_version: u64,
    ca_bundles: Vec<String>,
    config_version: u64,
    unknown: Vec<String>,
}

const SECRET_PATH: &str = "/api/v1/namespaces/mimic-system/secrets/mimic-webhook-tls";
const SEED_PATH: &str = "/api/v1/namespaces/mimic-system/secrets/mimic-tls-ca";
const CONFIGS_PATH: &str = "/apis/admissionregistration.k8s.io/v1/mutatingwebhookconfigurations";
const CONFIG_PATH: &str =
    "/apis/admissionregistration.k8s.io/v1/mutatingwebhookconfigurations/mimic-injector";

impl FakeKube {
    fn start(name: &str) -> FakeKube {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let dir =
            std::env::temp_dir().join(format!("mimic-fake-kube-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let kubeconfig = dir.join("config");
        std::fs::write(
            &kubeconfig,
            format!(
                "apiVersion: v1\nkind: Config\nclusters:\n- name: fake\n  cluster:\n    server: http://{addr}\n\
                 users:\n- name: fake\n  user:\n    token: fake\ncontexts:\n- name: fake\n  context:\n    cluster: fake\n    user: fake\n\
                 current-context: fake\n"
            ),
        )
        .unwrap();
        let state = Arc::new(Mutex::new(KubeState {
            secret_data: serde_json::json!({}),
            secret_version: 1,
            seed_data: serde_json::json!({}),
            seed_version: 1,
            ca_bundles: vec![String::new(); 3],
            config_version: 1,
            unknown: Vec::new(),
        }));
        let shared = state.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = shared.clone();
                thread::spawn(move || kube_connection(stream, state));
            }
        });
        FakeKube {
            kubeconfig: kubeconfig.to_string_lossy().into_owned(),
            state,
        }
    }

    fn secret(&self) -> serde_json::Value {
        self.state.lock().unwrap().secret_data.clone()
    }

    fn seed(&self) -> serde_json::Value {
        self.state.lock().unwrap().seed_data.clone()
    }

    fn ca_bundles(&self) -> Vec<String> {
        self.state.lock().unwrap().ca_bundles.clone()
    }

    /// Another client changing the `caBundle` of webhook `index`.
    fn set_ca_bundle(&self, index: usize, value: &str) {
        let mut state = self.state.lock().unwrap();
        state.ca_bundles[index] = value.to_string();
        state.config_version += 1;
    }
}

fn kube_connection(stream: TcpStream, state: Arc<Mutex<KubeState>>) {
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    loop {
        let mut request = String::new();
        if reader.read_line(&mut request).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = request.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let target = parts.next().unwrap_or_default().to_string();
        let mut length = 0;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            if header.trim().is_empty() {
                break;
            }
            if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        let shared = state.clone();
        let mut state = state.lock().unwrap();
        let (status, reply) = match (method.as_str(), path) {
            ("GET", SECRET_PATH) => (200, secret_json(&state)),
            ("PUT", SECRET_PATH) => {
                let version = body["metadata"]["resourceVersion"].as_str().unwrap_or("");
                if version == state.secret_version.to_string() {
                    state.secret_data = body["data"].clone();
                    state.secret_version += 1;
                    (200, secret_json(&state))
                } else {
                    let conflict = serde_json::json!({"apiVersion": "v1", "kind": "Status",
                        "status": "Failure", "reason": "Conflict", "code": 409,
                        "message": "the object has been modified"});
                    (409, conflict)
                }
            }
            ("GET", SEED_PATH) => (200, seed_json(&state)),
            ("PUT", SEED_PATH) => {
                let version = body["metadata"]["resourceVersion"].as_str().unwrap_or("");
                if version == state.seed_version.to_string() {
                    state.seed_data = body["data"].clone();
                    state.seed_version += 1;
                    (200, seed_json(&state))
                } else {
                    let conflict = serde_json::json!({"apiVersion": "v1", "kind": "Status",
                        "status": "Failure", "reason": "Conflict", "code": 409,
                        "message": "the object has been modified"});
                    (409, conflict)
                }
            }
            ("GET", CONFIG_PATH) => (200, config_json(&state)),
            ("PATCH", CONFIG_PATH) => {
                for op in body.as_array().cloned().unwrap_or_default() {
                    let path = op["path"].as_str().unwrap_or("");
                    if let Some(index) = path
                        .strip_prefix("/webhooks/")
                        .and_then(|p| p.strip_suffix("/clientConfig/caBundle"))
                        .and_then(|i| i.parse::<usize>().ok())
                    {
                        state.ca_bundles[index] = op["value"].as_str().unwrap_or("").to_string();
                    } else {
                        state.unknown.push(format!("patch {path}"));
                    }
                }
                state.config_version += 1;
                (200, config_json(&state))
            }
            ("GET", CONFIGS_PATH)
                if !query.contains("fieldSelector=metadata.name%3Dmimic-injector") =>
            {
                // RBAC only allows list and watch by resourceNames (spec §11).
                state
                    .unknown
                    .push(format!("unscoped list or watch: {query}"));
                (
                    403,
                    serde_json::json!({"apiVersion": "v1", "kind": "Status",
                    "status": "Failure", "reason": "Forbidden", "code": 403, "message": "no"}),
                )
            }
            ("GET", CONFIGS_PATH) => {
                if query.contains("watch=true") {
                    drop(state);
                    watch_configuration(&mut writer, &shared);
                    return;
                }
                let list = serde_json::json!({
                    "apiVersion": "admissionregistration.k8s.io/v1",
                    "kind": "MutatingWebhookConfigurationList",
                    "metadata": {"resourceVersion": state.config_version.to_string()},
                    "items": [config_json(&state)]});
                (200, list)
            }
            ("GET", "/apis/mimic.platform.io/v1alpha1/mimicworkloads") => {
                if query.contains("watch=true") {
                    drop(state);
                    let _ =
                        writer.write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n");
                    thread::sleep(Duration::from_secs(120));
                    return;
                }
                let list = serde_json::json!({"apiVersion": "mimic.platform.io/v1alpha1",
                    "kind": "MimicWorkloadList", "metadata": {"resourceVersion": "1"}, "items": []});
                (200, list)
            }
            _ => {
                state.unknown.push(format!("{method} {target}"));
                (
                    404,
                    serde_json::json!({"apiVersion": "v1", "kind": "Status",
                    "status": "Failure", "reason": "NotFound", "code": 404, "message": "no"}),
                )
            }
        };
        let reply = reply.to_string();
        let head = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
            reply.len()
        );
        if writer.write_all(head.as_bytes()).is_err() || writer.write_all(reply.as_bytes()).is_err()
        {
            return;
        }
    }
}

/// Streams a `MODIFIED` event each time the configuration changes, until the client leaves.
fn watch_configuration(writer: &mut TcpStream, state: &Mutex<KubeState>) {
    if writer
        .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n")
        .is_err()
    {
        return;
    }
    let mut sent = state.lock().unwrap().config_version;
    loop {
        thread::sleep(Duration::from_millis(100));
        let event = {
            let state = state.lock().unwrap();
            (state.config_version > sent).then(|| {
                sent = state.config_version;
                serde_json::json!({"type": "MODIFIED", "object": config_json(&state)})
            })
        };
        if let Some(event) = event {
            let line = format!("{event}\n");
            if write!(writer, "{:x}\r\n{line}\r\n", line.len()).is_err() {
                return;
            }
        }
    }
}

fn secret_json(state: &KubeState) -> serde_json::Value {
    serde_json::json!({"apiVersion": "v1", "kind": "Secret",
        "metadata": {"name": "mimic-webhook-tls", "namespace": "mimic-system",
            "resourceVersion": state.secret_version.to_string()},
        "data": state.secret_data})
}

fn seed_json(state: &KubeState) -> serde_json::Value {
    serde_json::json!({"apiVersion": "v1", "kind": "Secret",
        "metadata": {"name": "mimic-tls-ca", "namespace": "mimic-system",
            "resourceVersion": state.seed_version.to_string()},
        "data": state.seed_data})
}

fn config_json(state: &KubeState) -> serde_json::Value {
    let webhooks: Vec<_> = state
        .ca_bundles
        .iter()
        .enumerate()
        .map(|(i, bundle)| {
            serde_json::json!({"name": format!("w{i}.mimic.platform.io"),
                "clientConfig": {"caBundle": bundle}})
        })
        .collect();
    serde_json::json!({"apiVersion": "admissionregistration.k8s.io/v1",
        "kind": "MutatingWebhookConfiguration",
        "metadata": {"name": "mimic-injector",
            "resourceVersion": state.config_version.to_string()}, "webhooks": webhooks})
}

#[test]
fn inj_11_unready_until_the_certificate_is_loaded_and_the_cache_synced() {
    let valkey = FakeValkey::start("127.0.0.1:0".parse().unwrap());
    let url = format!("redis://{}", valkey.addr);

    // No cluster: Valkey is fine, the controller stays alive but unready.
    let mut lonely = Process::spawn(BIN, &env(&[("MIMIC_VALKEY_URL", &url)]));
    let addr = lonely.admin_addr();
    thread::sleep(Duration::from_millis(1500));
    let (status, body) = http_get(addr, "/readyz");
    assert_eq!(status, 503);
    assert!(body.contains("certificate"), "{body}");
    assert_eq!(http_get(addr, "/healthz").0, 200);

    let kube = FakeKube::start("ready");
    let mut controller = Process::spawn(
        BIN,
        &env(&[("MIMIC_VALKEY_URL", &url), ("KUBECONFIG", &kube.kubeconfig)]),
    );
    let addr = controller.admin_addr();
    wait_status(addr, "/readyz", 200, Duration::from_secs(15));
    assert!(kube.state.lock().unwrap().unknown.is_empty());
}

#[test]
fn inj_11_certificates_are_stored_and_ca_bundle_patched_once_for_all_replicas() {
    let valkey = FakeValkey::start("127.0.0.1:0".parse().unwrap());
    let url = format!("redis://{}", valkey.addr);
    let kube = FakeKube::start("replicas");
    let vars = env(&[("MIMIC_VALKEY_URL", &url), ("KUBECONFIG", &kube.kubeconfig)]);
    let mut first = Process::spawn(BIN, &vars);
    let mut second = Process::spawn(BIN, &vars);
    let (a, b) = (first.admin_addr(), second.admin_addr());
    wait_status(a, "/readyz", 200, Duration::from_secs(15));
    wait_status(b, "/readyz", 200, Duration::from_secs(15));

    let data = kube.secret();
    for key in ["ca.crt", "ca.key", "tls.crt", "tls.key"] {
        assert!(data[key].as_str().is_some_and(|v| !v.is_empty()), "{key}");
    }
    let bundles = kube.ca_bundles();
    assert_eq!(bundles.len(), 3);
    assert!(
        bundles
            .iter()
            .all(|b| *b == data["ca.crt"].as_str().unwrap())
    );
    for admin in [a, b] {
        let (_, metrics) = http_get(admin, "/metrics");
        let line = metrics
            .lines()
            .find(|l| l.starts_with("mimic_webhook_certificate_expiry_timestamp_seconds"))
            .unwrap_or_else(|| panic!("{metrics}"));
        let expiry: f64 = line.split_whitespace().last().unwrap().parse().unwrap();
        assert!(expiry > 1.7e9, "{line}");
    }

    // A restart keeps what is stored.
    first.terminate();
    first.wait_exit(Duration::from_secs(10));
    let mut restarted = Process::spawn(BIN, &vars);
    wait_status(
        restarted.admin_addr(),
        "/readyz",
        200,
        Duration::from_secs(15),
    );
    assert_eq!(kube.secret(), data);
    assert_eq!(kube.ca_bundles(), bundles);
}

#[test]
fn tls_08_the_seed_is_written_once_gates_readiness_and_is_never_logged() {
    use base64::Engine;
    let valkey = FakeValkey::start("127.0.0.1:0".parse().unwrap());
    let url = format!("redis://{}", valkey.addr);
    let kube = FakeKube::start("seed");
    assert_eq!(kube.seed(), serde_json::json!({}));
    let vars = env(&[("MIMIC_VALKEY_URL", &url), ("KUBECONFIG", &kube.kubeconfig)]);
    let mut first = Process::spawn(BIN, &vars);
    let mut second = Process::spawn(BIN, &vars);
    let (a, b) = (first.admin_addr(), second.admin_addr());
    wait_status(a, "/readyz", 200, Duration::from_secs(15));
    wait_status(b, "/readyz", 200, Duration::from_secs(15));

    // One seed for both replicas: 32 bytes and its creation time.
    let data = kube.seed();
    let seed = base64::engine::general_purpose::STANDARD
        .decode(data["seed"].as_str().unwrap())
        .unwrap();
    assert_eq!(seed.len(), 32);
    let created = base64::engine::general_purpose::STANDARD
        .decode(data["created"].as_str().unwrap())
        .unwrap();
    assert!(String::from_utf8(created).unwrap().parse::<u64>().unwrap() > 1_700_000_000);
    for admin in [a, b] {
        let (_, metrics) = http_get(admin, "/metrics");
        let line = metrics.lines().find(|l| l.starts_with("mimic_tls_seed_loaded"));
        assert_eq!(line.map(|l| l.split_whitespace().last().unwrap()), Some("1"));
    }

    // A restart keeps it.
    first.terminate();
    first.wait_exit(Duration::from_secs(10));
    let mut restarted = Process::spawn(BIN, &vars);
    wait_status(restarted.admin_addr(), "/readyz", 200, Duration::from_secs(15));
    assert_eq!(kube.seed(), data);

    // The value, in either encoding, is in no log line.
    let encoded = data["seed"].as_str().unwrap().to_string();
    let hex: String = seed.iter().map(|byte| format!("{byte:02x}")).collect();
    for process in [&mut second, &mut restarted] {
        for line in process.lines() {
            assert!(!line.contains(&encoded) && !line.contains(&hex), "{line}");
        }
    }
    assert!(kube.state.lock().unwrap().unknown.is_empty());
}

#[test]
fn tls_08_missing_seed_secret_name_exits_2() {
    let valkey = FakeValkey::start("127.0.0.1:0".parse().unwrap());
    let url = format!("redis://{}", valkey.addr);
    let kube = FakeKube::start("noseed");
    let mut vars = env(&[("MIMIC_VALKEY_URL", &url), ("KUBECONFIG", &kube.kubeconfig)]);
    vars.retain(|(k, _)| *k != "MIMIC_TLS_CA_SECRET");
    let (status, _, stderr) = run(BIN, &[], &vars);
    assert_eq!(status.code(), Some(2));
    assert!(stderr.contains("MIMIC_TLS_CA_SECRET"), "{stderr}");
}

#[test]
fn inj_19_invalid_listener_limits_exit_2() {
    for (var, value) in [
        ("MIMIC_WEBHOOK_MAX_CONNECTIONS", "0"),
        ("MIMIC_WEBHOOK_MAX_CONNECTIONS", "65536"),
        ("MIMIC_WEBHOOK_MAX_CONNECTIONS", "many"),
        ("MIMIC_WEBHOOK_REQUEST_TIMEOUT", "0s"),
        ("MIMIC_WEBHOOK_REQUEST_TIMEOUT", "fast"),
    ] {
        let (status, _, stderr) = run(
            BIN,
            &[],
            &env(&[("MIMIC_VALKEY_URL", "redis://127.0.0.1:1"), (var, value)]),
        );
        assert_eq!(status.code(), Some(2), "{var}={value}");
        assert!(stderr.contains(var), "{var}: {stderr}");
    }
}

#[test]
fn inj_20_a_changed_or_emptied_ca_bundle_is_restored_within_10s() {
    let valkey = FakeValkey::start("127.0.0.1:0".parse().unwrap());
    let url = format!("redis://{}", valkey.addr);
    let kube = FakeKube::start("restore");
    let mut controller = Process::spawn(
        BIN,
        &env(&[("MIMIC_VALKEY_URL", &url), ("KUBECONFIG", &kube.kubeconfig)]),
    );
    wait_status(
        controller.admin_addr(),
        "/readyz",
        200,
        Duration::from_secs(15),
    );
    let data = kube.secret();
    // The Secret holds base64 already, as does the `caBundle`.
    let encoded = data["ca.crt"].as_str().unwrap().to_string();
    wait_until(|| kube.ca_bundles().iter().all(|b| *b == encoded));

    kube.set_ca_bundle(0, "");
    kube.set_ca_bundle(2, "Zm9vYmFy");
    wait_until(|| kube.ca_bundles().iter().all(|b| *b == encoded));
    assert_eq!(kube.secret(), data, "no new certificate");
    assert!(kube.state.lock().unwrap().unknown.is_empty());
}

fn wait_until(condition: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(std::time::Instant::now() < deadline, "not restored in 10 s");
        thread::sleep(Duration::from_millis(100));
    }
}
