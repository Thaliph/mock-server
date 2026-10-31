//! INJ-06 on the real `mimic-init` binary, with fake `iptables-restore` commands on `PATH`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use mimic_core::redirect::{Family, RedirectConfig, rules};
use mimic_runtime::testing::run;
use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_mimic-init");

/// A fake restore command that records its arguments and stdin, then exits with `code`.
fn fake(dir: &Path, name: &str, code: i32) {
    let script = format!(
        "#!/bin/sh\necho \"$@\" > \"{d}/{name}.args\"\n/bin/cat > \"{d}/{name}.stdin\"\nexit {code}\n",
        d = dir.display()
    );
    let path = dir.join(name);
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn host_has_ipv6() -> bool {
    fs::read_to_string("/proc/net/if_inet6").is_ok_and(|s| !s.trim().is_empty())
}

fn logs(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .map(|l| serde_json::from_str(l).expect("every stdout line is JSON"))
        .collect()
}

fn path_env(dir: &Path) -> String {
    format!("{}:/usr/bin:/bin", dir.display())
}

#[test]
fn inj_06_pipes_the_rules_to_both_restore_commands_and_logs_once() {
    let dir = tempfile::tempdir().unwrap();
    fake(dir.path(), "iptables-restore", 0);
    fake(dir.path(), "ip6tables-restore", 0);
    let (status, stdout, _) = run(
        BIN,
        &[],
        &[
            ("PATH", &path_env(dir.path())),
            ("MIMIC_EXCLUDE_INBOUND_PORTS", "9090, 8081"),
            ("MIMIC_PROXY_UID", "2000"),
        ],
    );
    assert_eq!(status.code(), Some(0), "{stdout}");

    let config = RedirectConfig {
        inbound_port: 15006,
        outbound_port: 15001,
        proxy_uid: 2000,
        exclude_inbound_ports: [8081, 9090].into(),
    };
    let v4 = fs::read_to_string(dir.path().join("iptables-restore.stdin")).unwrap();
    assert_eq!(v4, rules(&config, Family::V4));
    let args = fs::read_to_string(dir.path().join("iptables-restore.args")).unwrap();
    assert_eq!(args.trim(), "--noflush");

    let entries = logs(&stdout);
    let installed: Vec<&Value> = entries
        .iter()
        .filter(|v| v["message"] == "redirection installed")
        .collect();
    assert_eq!(installed.len(), 1, "{stdout}");
    assert_eq!(
        installed[0]["excluded_ports"],
        serde_json::json!([8081, 9090])
    );
    if host_has_ipv6() {
        let v6 = fs::read_to_string(dir.path().join("ip6tables-restore.stdin")).unwrap();
        assert_eq!(v6, rules(&config, Family::V6));
        assert_eq!(
            installed[0]["families"],
            serde_json::json!(["ipv4", "ipv6"])
        );
    } else {
        assert!(!dir.path().join("ip6tables-restore.stdin").exists());
        assert_eq!(installed[0]["families"], serde_json::json!(["ipv4"]));
        assert!(
            entries
                .iter()
                .any(|v| v["level"] == "INFO" && v["message"].as_str().unwrap().contains("IPv6")),
            "{stdout}"
        );
    }
}

#[test]
fn inj_06_a_failing_restore_exits_1_naming_the_family() {
    let dir = tempfile::tempdir().unwrap();
    fake(dir.path(), "iptables-restore", 1);
    fake(dir.path(), "ip6tables-restore", 0);
    let (status, stdout, _) = run(BIN, &[], &[("PATH", &path_env(dir.path()))]);
    assert_eq!(status.code(), Some(1), "{stdout}");
    assert!(
        logs(&stdout)
            .iter()
            .any(|v| v["level"] == "ERROR" && v["family"] == "ipv4"),
        "{stdout}"
    );
    assert!(!stdout.contains("redirection installed"));
}

#[test]
fn inj_06_a_missing_restore_command_exits_1_naming_the_family() {
    let dir = tempfile::tempdir().unwrap();
    let (status, stdout, _) = run(BIN, &[], &[("PATH", &dir.path().display().to_string())]);
    assert_eq!(status.code(), Some(1), "{stdout}");
    assert!(
        logs(&stdout)
            .iter()
            .any(|v| v["level"] == "ERROR" && v["family"] == "ipv4"),
        "{stdout}"
    );
}

#[test]
fn inj_06_an_invalid_variable_exits_2() {
    for (var, value) in [
        ("MIMIC_PROXY_UID", "root"),
        ("MIMIC_INBOUND_PORT", "70000"),
        ("MIMIC_OUTBOUND_PORT", "0"),
        ("MIMIC_EXCLUDE_INBOUND_PORTS", "80,0"),
        ("MIMIC_EXCLUDE_INBOUND_PORTS", "http"),
        ("MIMIC_LOG", "[["),
    ] {
        let dir = tempfile::tempdir().unwrap();
        fake(dir.path(), "iptables-restore", 0);
        fake(dir.path(), "ip6tables-restore", 0);
        let (status, stdout, stderr) =
            run(BIN, &[], &[("PATH", &path_env(dir.path())), (var, value)]);
        assert_eq!(status.code(), Some(2), "{var}={value}");
        assert!(stderr.contains(var), "{stderr}");
        assert!(!dir.path().join("iptables-restore.stdin").exists());
        assert!(stdout.is_empty(), "{stdout}");
    }
}

#[test]
fn inj_06_never_logs_the_rest_of_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    fake(dir.path(), "iptables-restore", 0);
    fake(dir.path(), "ip6tables-restore", 0);
    let (status, stdout, stderr) = run(
        BIN,
        &[],
        &[
            ("PATH", &path_env(dir.path())),
            ("MIMIC_API_TOKEN", "canary-7f3a"),
            ("DATABASE_PASSWORD", "canary-91bc"),
        ],
    );
    assert_eq!(status.code(), Some(0));
    for text in [&stdout, &stderr] {
        assert!(!text.contains("canary-"), "{text}");
    }
}
