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
    assert_eq!(installed[0]["excluded_ports"], "8081,9090");
    if host_has_ipv6() {
        let v6 = fs::read_to_string(dir.path().join("ip6tables-restore.stdin")).unwrap();
        assert_eq!(v6, rules(&config, Family::V6));
        assert_eq!(installed[0]["families"], "ipv4,ipv6");
    } else {
        assert!(!dir.path().join("ip6tables-restore.stdin").exists());
        assert_eq!(installed[0]["families"], "ipv4");
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

/// A fake restore command that appends each run's stdin to `<name>.runs`, separated by a line
/// `=== run`, and exits with the code in `<name>.code` (default 0).
fn recording(dir: &Path, name: &str, code: i32) {
    let script = format!(
        "#!/bin/sh\necho '=== run' >> \"{d}/{name}.runs\"\n/bin/cat >> \"{d}/{name}.runs\"\nexit {code}\n",
        d = dir.display()
    );
    let path = dir.join(name);
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Jumps left in the built-in chains after replaying the recorded `--noflush` inputs in order:
/// `-F` empties a chain, `-A` appends to it. Keyed by `table chain`.
fn replay(runs: &str) -> std::collections::BTreeMap<String, usize> {
    let mut table = "";
    let mut rules: std::collections::BTreeMap<String, usize> = Default::default();
    for line in runs.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            [t] if t.starts_with('*') => table = &t[1..],
            ["-F", chain] => {
                rules.remove(&format!("{table} {chain}"));
            }
            ["-A", chain, .., "-j", target] if target.starts_with("MIMIC_") => {
                *rules.entry(format!("{table} {chain}")).or_default() += 1;
            }
            _ => {}
        }
    }
    rules.retain(|chain, _| !chain.contains("MIMIC_"));
    rules
}

fn run_init(dir: &Path) -> std::process::ExitStatus {
    run(BIN, &[], &[("PATH", &path_env(dir))]).0
}

fn expected_jumps() -> std::collections::BTreeMap<String, usize> {
    [
        ("nat PREROUTING", 1),
        ("nat OUTPUT", 1),
        ("filter OUTPUT", 1),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

#[test]
fn inj_22_running_twice_keeps_one_jump_per_built_in_chain() {
    let dir = tempfile::tempdir().unwrap();
    recording(dir.path(), "iptables-restore", 0);
    recording(dir.path(), "ip6tables-restore", 0);
    assert_eq!(run_init(dir.path()).code(), Some(0));
    assert_eq!(run_init(dir.path()).code(), Some(0));
    let families: &[&str] = if host_has_ipv6() {
        &["iptables-restore", "ip6tables-restore"]
    } else {
        &["iptables-restore"]
    };
    for name in families {
        let runs = fs::read_to_string(dir.path().join(format!("{name}.runs"))).unwrap();
        assert_eq!(runs.matches("=== run").count(), 2, "{name}");
        assert_eq!(replay(&runs), expected_jumps(), "{name}");
    }
}

#[test]
fn inj_22_a_rerun_after_a_failed_ipv6_restore_keeps_one_jump() {
    if !host_has_ipv6() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    recording(dir.path(), "iptables-restore", 0);
    recording(dir.path(), "ip6tables-restore", 1);
    assert_eq!(run_init(dir.path()).code(), Some(1));
    recording(dir.path(), "ip6tables-restore", 0);
    assert_eq!(run_init(dir.path()).code(), Some(0));
    let v4 = fs::read_to_string(dir.path().join("iptables-restore.runs")).unwrap();
    assert_eq!(v4.matches("=== run").count(), 2);
    assert_eq!(replay(&v4), expected_jumps());
    let v6 = fs::read_to_string(dir.path().join("ip6tables-restore.runs")).unwrap();
    assert_eq!(replay(&v6), expected_jumps());
}
