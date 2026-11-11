//! MON-04: `process_*` metrics of the admin listener (spec §9, #385).

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use mimic_runtime::metrics::Metrics;

const CHILD_ENV: &str = "MIMIC_TEST_PROCESS_CHILD_CANARY";

fn encode() -> String {
    Metrics::new("sidecar", "1.2.3").encode()
}

fn value(text: &str, name: &str) -> f64 {
    let prefix = format!("{name} ");
    text.lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no `{name}` series in:\n{text}"))
        .parse()
        .unwrap()
}

fn soft_nofile() -> f64 {
    let limits = std::fs::read_to_string("/proc/self/limits").unwrap();
    let line = limits
        .lines()
        .find(|l| l.starts_with("Max open files"))
        .unwrap();
    match line.split_whitespace().nth(3).unwrap() {
        "unlimited" => f64::INFINITY,
        soft => soft.parse().unwrap(),
    }
}

#[test]
fn mon_04_process_families_have_type_help_and_no_label() {
    let text = encode();
    for (name, kind, help) in [
        (
            "process_cpu_seconds_total",
            "counter",
            "Total user and system CPU time spent in seconds",
        ),
        (
            "process_resident_memory_bytes",
            "gauge",
            "Resident memory size in bytes",
        ),
        (
            "process_open_fds",
            "gauge",
            "Number of open file descriptors",
        ),
        (
            "process_max_fds",
            "gauge",
            "Maximum number of open file descriptors",
        ),
        (
            "process_start_time_seconds",
            "gauge",
            "Start time of the process since unix epoch in seconds",
        ),
    ] {
        let family = name.strip_suffix("_total").unwrap_or(name);
        assert!(
            text.lines().any(|l| l == format!("# TYPE {family} {kind}")),
            "{name}: {text}"
        );
        assert!(
            text.lines().any(|l| l == format!("# HELP {family} {help}")),
            "{name}: {text}"
        );
        assert!(
            text.lines().any(|l| l.starts_with(&format!("{name} "))),
            "{name} has a label or is missing: {text}"
        );
    }
    assert!(!text.contains("mimic_process_"), "{text}");
    assert!(text.ends_with("# EOF\n") && text.matches("# EOF").count() == 1);
}

#[test]
fn mon_04_values_are_plausible() {
    let text = encode();
    assert!(value(&text, "process_resident_memory_bytes") > 0.0);
    assert_eq!(value(&text, "process_max_fds"), soft_nofile());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let start = value(&text, "process_start_time_seconds");
    assert!(start > now - 60.0 && start <= now, "{start}");
}

#[test]
fn mon_04_open_fds_grows_with_one_more_open_file() {
    // Other tests of this binary open files too: compare inside one test
    // with the fd count read right before and after.
    let mut grew = false;
    for _ in 0..20 {
        let before = value(&encode(), "process_open_fds");
        let file = std::fs::File::open("/proc/self/limits").unwrap();
        let during = value(&encode(), "process_open_fds");
        drop(file);
        if during == before + 1.0 {
            grew = true;
            break;
        }
    }
    assert!(grew, "process_open_fds never grew by exactly 1");
}

#[test]
fn mon_04_cpu_seconds_grow_around_a_busy_loop() {
    let before = value(&encode(), "process_cpu_seconds_total");
    let started = std::time::Instant::now();
    let mut x = 1u64;
    while started.elapsed().as_millis() < 300 {
        x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
    }
    let after = value(&encode(), "process_cpu_seconds_total");
    // 300 ms of busy loop is 30 ticks of 10 ms: the counter must have moved.
    assert!(after > before, "{before} -> {after}");
}

#[test]
fn mon_04_canary_in_env_and_args_is_not_exposed() {
    if let Ok(canary) = std::env::var(CHILD_ENV) {
        // Child: print the exposition between markers.
        println!("BEGIN\n{}\nEND {canary}", encode());
        return;
    }
    let canary = "canary-7f3a9c1e5b2d4086";
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "mon_04_canary_in_env_and_args_is_not_exposed",
            "--nocapture",
            "--test-threads=1",
            canary,
        ])
        .env(CHILD_ENV, canary)
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let begin = stdout.find("BEGIN\n").expect(&stdout);
    let end = stdout.find("\nEND ").expect(&stdout);
    let exposition = &stdout[begin..end];
    assert!(exposition.contains("process_open_fds "), "{exposition}");
    assert!(!exposition.contains(canary));
}
