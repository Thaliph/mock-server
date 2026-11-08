//! LDT-11 on the real `mimic-sidecar` binary: `GET /load` exists only with
//! `MIMIC_LOAD_ENABLED=1` (spec §4.1, §7.8, §10.3).

use mimic_runtime::testing::{Process, http_get, wait_status};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_mimic-sidecar");

fn spawn(extra: &[(&str, &str)]) -> Process {
    let mut env = vec![
        ("MIMIC_ADMIN_ADDR", "127.0.0.1:0"),
        ("MIMIC_INBOUND_ADDRS", "127.0.0.1:0"),
        ("MIMIC_OUTBOUND_ADDRS", "127.0.0.1:0"),
        ("MIMIC_WORKLOAD", "payment"),
        ("MIMIC_POD_NAMESPACE", "shop"),
        ("MIMIC_POD_NAME", "payment-6d4c8f9b7-xxxxx"),
    ];
    env.extend_from_slice(extra);
    Process::spawn(BIN, &env)
}

#[test]
fn ldt_11_load_is_404_without_load_enabled() {
    let mut p = spawn(&[]);
    let admin = p.admin_addr();
    wait_status(admin, "/readyz", 200, Duration::from_secs(10));
    assert_eq!(http_get(admin, "/load").0, 404);
}

#[test]
fn ldt_11_load_answers_the_identity_with_load_enabled() {
    let mut p = spawn(&[("MIMIC_LOAD_ENABLED", "1")]);
    let admin = p.admin_addr();
    wait_status(admin, "/readyz", 200, Duration::from_secs(10));
    let (status, body) = http_get(admin, "/load");
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        r#"{"deployment":"payment","mode":"discovery","namespace":"shop","pod":"payment-6d4c8f9b7-xxxxx"}"#
    );
}

#[test]
fn ldt_11_an_invalid_load_enabled_exits_2() {
    let mut p = spawn(&[("MIMIC_LOAD_ENABLED", "yes")]);
    let status = p.wait_exit(Duration::from_secs(10));
    assert_eq!(status.code(), Some(2));
}
