//! FND-01, FND-04, FND-05, FND-07, INJ-07, PRX-10, PRX-12 on the real `mimic-sidecar` binary.

use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use mimic_runtime::testing::{Process, http_get, run, wait_status};

const BIN: &str = env!("CARGO_BIN_EXE_mimic-sidecar");
const ADMIN: (&str, &str) = ("MIMIC_ADMIN_ADDR", "127.0.0.1:0");
// Ephemeral ports: the default 15006 and 15001 would collide between parallel tests.
const INBOUND: (&str, &str) = ("MIMIC_INBOUND_ADDRS", "127.0.0.1:0");
const OUTBOUND: (&str, &str) = ("MIMIC_OUTBOUND_ADDRS", "127.0.0.1:0");

#[test]
fn fnd_01_invalid_value_exits_2_before_listening() {
    let (status, stdout, stderr) = run(BIN, &[], &[("MIMIC_SHUTDOWN_GRACE", "soon")]);
    assert_eq!(status.code(), Some(2));
    assert!(
        stderr.contains("MIMIC_SHUTDOWN_GRACE") && stderr.contains("soon"),
        "{stderr}"
    );
    assert!(!stdout.contains("admin listening"), "{stdout}");
}

#[test]
fn fnd_04_every_stdout_line_is_a_json_object_with_the_base_fields() {
    let mut sidecar = Process::spawn(BIN, &[ADMIN, INBOUND, OUTBOUND, ("MIMIC_LOG", "debug")]);
    let addr = sidecar.admin_addr();
    assert_eq!(http_get(addr, "/healthz").0, 200);
    sidecar.terminate();
    sidecar.wait_exit(Duration::from_secs(10));

    let lines = sidecar.lines().to_vec();
    assert!(!lines.is_empty());
    for line in lines {
        let v: serde_json::Value =
            serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"));
        for field in ["timestamp", "level", "target", "message"] {
            assert!(v.get(field).is_some(), "missing {field}: {line}");
        }
    }
}

#[test]
fn fnd_04_mimic_log_filters_the_output() {
    let mut sidecar = Process::spawn(BIN, &[ADMIN, INBOUND, OUTBOUND, ("MIMIC_LOG", "warn")]);
    // Nothing at info level reaches stdout, so wait on the process itself.
    std::thread::sleep(Duration::from_millis(500));
    sidecar.terminate();
    sidecar.wait_exit(Duration::from_secs(10));
    assert!(
        sidecar
            .logs()
            .iter()
            .all(|v| v["level"] == "WARN" || v["level"] == "ERROR"),
        "{:?}",
        sidecar.lines()
    );
}

#[test]
fn fnd_05_sigterm_exits_0_within_the_grace_period() {
    let mut sidecar = Process::spawn(
        BIN,
        &[ADMIN, INBOUND, OUTBOUND, ("MIMIC_SHUTDOWN_GRACE", "2s")],
    );
    let addr = sidecar.admin_addr();
    assert_eq!(http_get(addr, "/readyz").0, 200);

    sidecar.terminate();
    let status = sidecar.wait_exit(Duration::from_secs(3));
    assert!(status.success(), "{status}");
    sidecar.wait_log(Duration::from_secs(1), |v| {
        v["message"] == "shutdown started"
    });
    sidecar.wait_log(Duration::from_secs(1), |v| {
        v["message"] == "shutdown complete"
    });
}

#[test]
fn fnd_07_version_flag_prints_the_crate_version() {
    let (status, stdout, _) = run(BIN, &["--version"], &[]);
    assert!(status.success());
    assert_eq!(
        stdout.trim(),
        format!("mimic-sidecar {}", env!("CARGO_PKG_VERSION"))
    );
}

fn tunnel_addrs(sidecar: &mut Process) -> (SocketAddr, SocketAddr) {
    let mut find = |direction: &str| -> SocketAddr {
        sidecar.wait_log(Duration::from_secs(10), |v| {
            v["message"] == "tunnel listening" && v["direction"] == direction
        })["addr"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    };
    (find("inbound"), find("outbound"))
}

const LISTENERS: [(&str, &str); 3] = [ADMIN, INBOUND, OUTBOUND];

#[test]
fn inj_07_listeners_are_bound_when_readyz_answers_200() {
    let mut sidecar = Process::spawn(BIN, &LISTENERS);
    let admin = sidecar.admin_addr();
    wait_status(admin, "/readyz", 200, Duration::from_secs(10));
    let (inbound, outbound) = tunnel_addrs(&mut sidecar);
    for addr in [inbound, outbound] {
        TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("listener accepts");
    }
}

#[test]
fn prx_10_connection_without_original_destination_is_closed_and_counted() {
    let mut sidecar = Process::spawn(BIN, &LISTENERS);
    let admin = sidecar.admin_addr();
    let (inbound, outbound) = tunnel_addrs(&mut sidecar);
    for addr in [inbound, outbound] {
        let mut c = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 1];
        assert!(matches!(c.read(&mut buf), Ok(0) | Err(_)));
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (_, metrics) = http_get(admin, "/metrics");
        // Without a redirection the kernel either fails (`no_original_dst`) or reports the
        // listener itself (`loop`), depending on conntrack: both close the connection.
        if ["inbound", "outbound"].iter().all(|d| {
            ["no_original_dst", "loop"].iter().any(|r| {
                metrics.contains(&format!(
                    r#"mimic_tcp_connections_total{{direction="{d}",result="{r}"}} 1"#
                ))
            })
        }) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{metrics}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn inj_07_outbound_listener_off_loopback_exits_2() {
    let (status, _, stderr) = run(BIN, &[], &[("MIMIC_OUTBOUND_ADDRS", "0.0.0.0:15001")]);
    assert_eq!(status.code(), Some(2));
    assert!(stderr.contains("MIMIC_OUTBOUND_ADDRS"), "{stderr}");
}

#[test]
fn inj_07_invalid_listener_address_exits_2() {
    let (status, _, stderr) = run(BIN, &[], &[("MIMIC_INBOUND_ADDRS", "nope")]);
    assert_eq!(status.code(), Some(2));
    assert!(stderr.contains("MIMIC_INBOUND_ADDRS"), "{stderr}");
}

#[test]
fn inj_18_invalid_app_probes_exit_2_without_echoing_the_value() {
    let (status, stdout, stderr) = run(BIN, &[], &[("MIMIC_APP_PROBES", "{\"secret-token\":")]);
    assert_eq!(status.code(), Some(2));
    assert!(stderr.contains("MIMIC_APP_PROBES"), "{stderr}");
    assert!(!stderr.contains("secret-token") && !stdout.contains("secret-token"));
}

#[test]
fn prx_12_open_file_soft_limit_equals_the_hard_limit() {
    let mut sidecar = Process::spawn(BIN, &LISTENERS);
    let admin = sidecar.admin_addr();
    wait_status(admin, "/readyz", 200, Duration::from_secs(10));
    let limits = std::fs::read_to_string(format!("/proc/{}/limits", sidecar.pid())).unwrap();
    let line = limits
        .lines()
        .find(|l| l.starts_with("Max open files"))
        .unwrap();
    let cols: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(cols[3], cols[4], "{line}");
}
