//! FND-01, FND-04, FND-05, FND-07 on the real `mimic-sidecar` binary.

use std::time::Duration;

use mimic_runtime::testing::{Process, http_get, run};

const BIN: &str = env!("CARGO_BIN_EXE_mimic-sidecar");
const ADMIN: (&str, &str) = ("MIMIC_ADMIN_ADDR", "127.0.0.1:0");

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
    let mut sidecar = Process::spawn(BIN, &[ADMIN, ("MIMIC_LOG", "debug")]);
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
    let mut sidecar = Process::spawn(BIN, &[ADMIN, ("MIMIC_LOG", "warn")]);
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
    let mut sidecar = Process::spawn(BIN, &[ADMIN, ("MIMIC_SHUTDOWN_GRACE", "2s")]);
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
