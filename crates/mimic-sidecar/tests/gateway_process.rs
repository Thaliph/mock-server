//! GW-18 and GW-20 on the gateway role of `mimic-sidecar` (spec §7.10, §10.3): the role switch,
//! its configuration, the bind failure, readiness and the drain at `SIGTERM`.

mod common;

use std::io::{ErrorKind, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use common::capture_logs;
use common::gateway_rig::{
    BIN, connect, free_port, gateway, get, local, ports_value, read_head, run_bounded,
};
use mimic_runtime::admin;
use mimic_runtime::metrics::Metrics;
use mimic_runtime::testing::{Process, http_get, wait_status};
use mimic_sidecar::gateway::config::GatewayConfig;
use mimic_sidecar::gateway::serve::Gateway;
use tower::ServiceExt;

const ADMIN: (&str, &str) = ("MIMIC_ADMIN_ADDR", "127.0.0.1:0");

#[test]
fn gw_18_role_absent_or_sidecar_runs_the_sidecar() {
    let _logs = capture_logs();
    for role in [None, Some("sidecar")] {
        let mut env = vec![
            ADMIN,
            ("MIMIC_INBOUND_ADDRS", "127.0.0.1:0"),
            ("MIMIC_OUTBOUND_ADDRS", "127.0.0.1:0"),
            ("MIMIC_SHUTDOWN_GRACE", "2s"),
        ];
        if let Some(role) = role {
            env.push(("MIMIC_ROLE", role));
        }
        let mut sidecar = Process::spawn(BIN, &env);
        let admin = sidecar.admin_addr();
        wait_status(admin, "/readyz", 200, Duration::from_secs(5));
        sidecar.wait_log(Duration::from_secs(5), |v| {
            v["message"] == "tunnel listening"
        });
        assert!(
            !sidecar
                .logs()
                .iter()
                .any(|v| v["message"] == "gateway listening"),
            "{role:?}: {:?}",
            sidecar.lines()
        );
    }
}

#[test]
fn gw_18_gateway_role_starts_without_the_sidecar_variables() {
    let _logs = capture_logs();
    let decoy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    decoy.set_nonblocking(true).unwrap();
    let url = format!("https://{}", decoy.local_addr().unwrap());
    let trust = tempfile::tempdir().unwrap();
    let ports = [free_port(), free_port()];
    let (_gateway, _admin) = gateway(
        &ports,
        &[
            ("MIMIC_INBOUND_ADDRS", "x"),
            ("MIMIC_MODE", "x"),
            ("MIMIC_CONTROLLER_URL", &url),
            ("MIMIC_TLS_CA", "x"),
            ("MIMIC_TRUST_DIR", trust.path().to_str().unwrap()),
        ],
    );
    for port in ports {
        get(&mut connect(port), "/").assert_no_route(&[]);
    }
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        decoy.accept().map(|_| ()).map_err(|e| e.kind()),
        Err(ErrorKind::WouldBlock),
        "the decoy controller was connected to"
    );
    let written: Vec<_> = std::fs::read_dir(trust.path()).unwrap().collect();
    assert!(written.is_empty(), "{written:?}");
}

#[test]
fn gw_18_invalid_gateway_variables_exit_2_naming_the_variable() {
    let port = free_port().to_string();
    let many = ports_value(&(1..=65).collect::<Vec<u16>>());
    let ports = "MIMIC_GATEWAY_PORTS";
    let mut cases: Vec<(&str, Option<&str>)> = vec![
        ("MIMIC_ROLE", Some("proxy")),
        (ports, None),
        (ports, Some("")),
        (ports, Some("0")),
        (ports, Some("65536")),
        (ports, Some("15100")),
        (ports, Some("80,80")),
        (ports, Some(&many)),
        (ports, Some("http")),
    ];
    for value in ["0s", "500ms", "6m"] {
        cases.push(("MIMIC_GATEWAY_HEADER_TIMEOUT", Some(value)));
    }
    for value in ["0s", "2h"] {
        cases.push(("MIMIC_GATEWAY_IDLE_TIMEOUT", Some(value)));
    }
    for (var, value) in cases {
        let mut env = vec![ADMIN, ("MIMIC_ROLE", "gateway")];
        if var != ports {
            env.push((ports, &port));
        }
        if var == "MIMIC_ROLE" {
            env.retain(|(k, _)| *k != "MIMIC_ROLE");
        }
        if let Some(value) = value {
            env.push((var, value));
        }
        let (code, stdout, stderr) = run_bounded(&env);
        assert_eq!(code, Some(2), "{var}={value:?}: {stderr}");
        assert!(stderr.contains(var), "{var}={value:?}: {stderr}");
        assert!(!stdout.contains("admin listening"), "{var}={value:?}");
    }
    // The bounds themselves are accepted.
    for (var, value) in [
        ("MIMIC_GATEWAY_HEADER_TIMEOUT", "1s"),
        ("MIMIC_GATEWAY_HEADER_TIMEOUT", "5m"),
        ("MIMIC_GATEWAY_IDLE_TIMEOUT", "1s"),
        ("MIMIC_GATEWAY_IDLE_TIMEOUT", "1h"),
    ] {
        let (process, _) = gateway(&[free_port()], &[(var, value)]);
        drop(process);
    }
}

#[test]
fn gw_18_a_bound_port_exits_1_with_one_error() {
    let _logs = capture_logs();
    let held = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let free = free_port();
    let value = ports_value(&[free, port]);
    let mut process = Process::spawn(
        BIN,
        &[
            ADMIN,
            ("MIMIC_ROLE", "gateway"),
            ("MIMIC_GATEWAY_PORTS", &value),
        ],
    );
    let status = process.wait_exit(Duration::from_secs(5));
    assert_eq!(status.code(), Some(1), "{:?}", process.lines());
    let errors: Vec<_> = process
        .logs()
        .into_iter()
        .filter(|v| v["level"] == "ERROR")
        .collect();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0]["message"], "gateway listener failed");
    assert_eq!(errors[0]["port"], u64::from(port));
    drop(held);
}

async fn readyz(router: &axum::Router) -> u16 {
    let request = axum::http::Request::get("/readyz")
        .body(Body::empty())
        .unwrap();
    router
        .clone()
        .oneshot(request)
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn gw_20_readyz_is_503_until_every_listener_is_bound() {
    let _logs = capture_logs();
    let gateway = Gateway::new(GatewayConfig {
        ports: vec![free_port(), free_port()],
        header_timeout: Duration::from_secs(10),
        idle_timeout: Duration::from_secs(60),
        max_connections: 16,
    });
    let router = admin::router(
        Arc::new(Metrics::new("gateway", "test")),
        gateway.readiness(),
    );
    assert_eq!(readyz(&router).await, 503);
    let listeners = gateway
        .bind(false)
        .unwrap_or_else(|e| panic!("{}", e.source));
    assert_eq!(listeners.len(), 2);
    assert_eq!(readyz(&router).await, 200);
    gateway.shutdown();
    assert_eq!(readyz(&router).await, 503);
}

#[test]
fn gw_20_sigterm_drains_and_exits_0() {
    let _logs = capture_logs();
    let port = free_port();
    let (mut process, admin) = gateway(&[port], &[("MIMIC_SHUTDOWN_GRACE", "3s")]);
    let mut a = connect(port);
    a.write_all(b"GET / HTTP/1.1\r\nHost: a\r\n").unwrap();
    common::gateway_rig::server_read_all(port, &a);

    process.terminate();
    let signalled = Instant::now();
    wait_status(admin, "/readyz", 503, Duration::from_secs(1));
    let refused = loop {
        match TcpStream::connect_timeout(&local(port), Duration::from_millis(200)) {
            Err(e) if e.kind() == ErrorKind::ConnectionRefused => break true,
            _ if signalled.elapsed() > Duration::from_secs(1) => break false,
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    assert!(refused, "the listener still accepts after SIGTERM");

    a.write_all(b"\r\n").unwrap();
    read_head(&mut a)
        .expect("the request in flight is answered")
        .assert_no_route(&[("connection", "close")]);
    let status = process.wait_exit(Duration::from_secs(4).saturating_sub(signalled.elapsed()));
    assert_eq!(status.code(), Some(0), "{:?}", process.lines());
}

#[test]
fn gw_20_admin_paths_answer_on_the_admin_port_only() {
    let _logs = capture_logs();
    let port = free_port();
    let (_process, admin) = gateway(&[port], &[]);
    assert_eq!(http_get(admin, "/healthz").0, 200);
    let (status, metrics) = http_get(admin, "/metrics");
    assert_eq!(status, 200);
    assert!(
        metrics.contains("mimic_build_info{component=\"gateway\""),
        "{metrics}"
    );
    for path in ["/healthz", "/metrics", "/readyz"] {
        get(&mut connect(port), path).assert_no_route(&[]);
    }
}
