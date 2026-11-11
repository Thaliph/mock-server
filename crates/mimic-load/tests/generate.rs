//! LDT-12, LDT-14 (generator part) and LDT-15 on the real `mimic-load` binary (spec §7.8),
//! against a fake load API and fake targets.

mod common;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use common::*;
use mimic_core::load::plan::{Body, Plan};
use mimic_runtime::testing::http_get;

const CANARY: &str = "secret-canary-41d7";
const ECHO: &str = "answer-canary-9be2";

fn total_requests(post: &serde_json::Value) -> u64 {
    post["results"]["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["requests"].as_u64().unwrap())
        .sum()
}

#[test]
fn ldt_12_open_model_sends_the_rate_over_both_targets() {
    let mut plan = simple_plan();
    plan.endpoints[0].volatile = vec!["request.header/idempotency-key".to_string()];
    plan.endpoints[0].samples[0].headers = vec![
        (
            "idempotency-key".to_string(),
            "550e8400-e29b-41d4-a716-446655440000".to_string(),
        ),
        ("x-keep".to_string(), "same".to_string()),
    ];
    let params = Params::open(50, 200, 10_000);
    let rig = Rig::new(&plan, &params, &[Probe::Good, Probe::Good]);
    let mut generator = rig.spawn("1s", &[]);
    let status = generator.wait_exit(Duration::from_secs(40));
    assert!(status.success(), "{status}");

    let counts: Vec<usize> = rig.targets.iter().map(|t| t.count()).collect();
    let total: usize = counts.iter().sum();
    assert!((1900..=2100).contains(&total), "sent {total}: {counts:?}");
    assert!(counts.iter().all(|c| *c > 700), "{counts:?}");

    let mut keys = BTreeSet::new();
    for target in &rig.targets {
        for seen in target.state.seen.lock().unwrap().iter() {
            assert_eq!(seen.headers["mimic-load-run"], TICKET);
            assert_eq!(seen.headers["x-keep"], "same");
            assert_eq!(
                seen.headers["host"],
                format!("{}:{REQUEST_PORT}", target.ip)
            );
            let key = &seen.headers["idempotency-key"];
            assert_eq!(key.len(), 36, "{key}");
            for (i, c) in key.chars().enumerate() {
                if [8, 13, 18, 23].contains(&i) {
                    assert_eq!(c, '-', "{key}");
                } else {
                    assert!(c.is_ascii_hexdigit() && !c.is_ascii_uppercase(), "{key}");
                }
            }
            keys.insert(key.clone());
        }
        // Keep-alive: far fewer connections than requests, each one probed first.
        assert!(target.accepted() <= 60, "{} connections", target.accepted());
        assert!(
            target
                .state
                .probes
                .load(std::sync::atomic::Ordering::SeqCst)
                >= target.accepted()
        );
    }
    assert!(
        keys.len() * 100 >= total * 99,
        "{} distinct of {total}",
        keys.len()
    );

    let posts = rig.state().posts();
    assert!(posts.len() >= 8, "{} posts", posts.len());
    let mut last = 0;
    for post in &posts {
        let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
        assert_eq!(post["pod"], hostname.trim());
        let seen = total_requests(post);
        assert!(seen >= last, "results must be cumulative");
        last = seen;
    }
    let (final_post, others) = posts.split_last().unwrap();
    assert_eq!(final_post["final"], true);
    assert_eq!(final_post["reason"], "Completed");
    assert!(others.iter().all(|p| p["final"] == false));
    assert_eq!(total_requests(final_post), total as u64);
    // The projected token goes in every call.
    let expected = format!("Bearer {TOKEN}");
    assert!(
        rig.state()
            .authorizations
            .lock()
            .unwrap()
            .iter()
            .all(|a| *a == expected)
    );
}

#[test]
fn ldt_12_closed_model_never_has_more_requests_in_flight_than_the_concurrency() {
    let params = Params::closed(8, 3_000);
    let rig = Rig::new(&simple_plan(), &params, &[Probe::Good]);
    rig.targets[0].set_delay(Duration::from_millis(50));
    let mut generator = rig.spawn("1s", &[]);
    assert!(generator.wait_exit(Duration::from_secs(20)).success());
    let max = rig.targets[0].max_in_flight();
    assert!((6..=8).contains(&max), "max in flight {max}");
    // 8 workers, 50 ms each, 3 s: about 480 requests.
    assert!(rig.targets[0].count() > 200, "{}", rig.targets[0].count());
}

fn credentials_plan() -> Plan {
    let mut plan = simple_plan();
    plan.credentials = vec![credential("header/authorization", "token")];
    plan.endpoints[0].samples[0].headers =
        vec![("authorization".to_string(), "Bearer <redacted>".to_string())];
    plan
}

#[test]
fn ldt_12_credentials_come_from_the_credentials_directory() {
    let rig = Rig::new(
        &credentials_plan(),
        &Params::closed(2, 2_000),
        &[Probe::Good],
    );
    let dir = rig.credentials_dir();
    std::fs::write(dir.join("token"), format!("{CANARY}\n")).unwrap();
    let mut generator = rig.spawn(
        "1s",
        &[("MIMIC_LOAD_CREDENTIALS_DIR", dir.to_str().unwrap())],
    );
    assert!(generator.wait_exit(Duration::from_secs(20)).success());
    let seen = rig.targets[0].state.seen.lock().unwrap();
    assert!(!seen.is_empty());
    assert!(seen.iter().all(|s| s.headers["authorization"] == CANARY));
}

#[test]
fn ldt_12_unverified_targets_receive_no_connection() {
    let probes = [
        Probe::OtherDeployment,
        Probe::OtherPod,
        Probe::Discovery,
        Probe::NotFound,
        Probe::Silent,
        Probe::Good,
    ];
    let rig = Rig::new(&simple_plan(), &Params::closed(4, 3_000), &probes);
    let mut generator = rig.spawn("1s", &[]);
    assert!(generator.wait_exit(Duration::from_secs(30)).success());
    for bad in &rig.targets[..5] {
        assert_eq!(bad.accepted(), 0, "{}", bad.pod);
        assert_eq!(bad.count(), 0, "{}", bad.pod);
        assert!(bad.state.probes.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    }
    assert!(rig.targets[5].count() > 0);
    generator.wait_log(Duration::from_secs(1), |v| {
        v["message"] == "target not verified"
    });
}

#[test]
fn ldt_12_without_a_verified_target_the_run_ends_with_no_targets() {
    let mut params = Params::closed(4, 30_000);
    params.start_timeout_ms = 2_000;
    let rig = Rig::new(
        &simple_plan(),
        &params,
        &[Probe::Discovery, Probe::NotFound],
    );
    let mut generator = rig.spawn("1s", &[]);
    let status = generator.wait_exit(Duration::from_secs(20));
    assert_eq!(status.code(), Some(1));
    assert_eq!(rig.state().final_post().unwrap()["reason"], "NoTargets");
    assert!(rig.targets.iter().all(|t| t.accepted() == 0));
}

#[test]
fn ldt_12_three_probe_rounds_without_a_verified_target_end_the_run_early() {
    let mut params = Params::closed(4, 60_000);
    params.start_timeout_ms = 300_000;
    let rig = Rig::new(&simple_plan(), &params, &[Probe::NotFound, Probe::NotFound]);
    let mut generator = rig.spawn("1s", &[]);
    let status = generator.wait_exit(Duration::from_secs(12));
    assert_eq!(status.code(), Some(1));
    assert_eq!(rig.state().final_post().unwrap()["reason"], "NoTargets");
    let finals = rig
        .state()
        .posts()
        .iter()
        .filter(|p| p["final"] == true)
        .count();
    assert_eq!(finals, 1);
    assert!(rig.targets.iter().all(|t| t.accepted() == 0));
}

#[test]
fn ldt_12_an_empty_targets_list_ends_the_run_at_the_start_timeout() {
    let mut params = Params::closed(4, 60_000);
    params.start_timeout_ms = 5_000;
    let rig = Rig::new(&simple_plan(), &params, &[]);
    let begun = Instant::now();
    let mut generator = rig.spawn("1s", &[]);
    let status = generator.wait_exit(Duration::from_secs(20));
    // The 3-round rule (about 3 s) does not apply: no target was ever listed.
    assert!(begun.elapsed() >= Duration::from_millis(4_900));
    assert_eq!(status.code(), Some(1));
    assert_eq!(rig.state().final_post().unwrap()["reason"], "NoTargets");
}

#[test]
fn ldt_12_a_connection_that_landed_on_another_pod_stops_the_run_unverified() {
    for reason in ["workload", "ticket"] {
        let rig = Rig::new(
            &simple_plan(),
            &Params::closed(4, 30_000),
            &[Probe::Good, Probe::Good],
        );
        rig.targets[0].set_answer(Answer::Refused(reason));
        let mut generator = rig.spawn("1s", &[]);
        let status = generator.wait_exit(Duration::from_secs(20));
        assert_eq!(status.code(), Some(1), "{reason}");
        let post = rig.state().final_post().expect("final post");
        assert_eq!(post["reason"], "Unverified", "{reason}");
    }
}

#[test]
fn ldt_12_metrics_expose_the_generator_series() {
    let rig = Rig::new(
        &simple_plan(),
        &Params::closed(2, 4_000),
        &[Probe::Good, Probe::Good],
    );
    let mut generator = rig.spawn("1s", &[]);
    let admin = generator.admin_addr();
    std::thread::sleep(Duration::from_millis(2_000));
    let (status, metrics) = http_get(admin, "/metrics");
    assert_eq!(status, 200);
    for series in [
        "mimic_build_info{component=\"load\"",
        "mimic_load_requests_total{endpoint=\"GET /items\",result=\"2xx\"}",
        "mimic_load_request_duration_seconds_bucket{",
        "endpoint=\"GET /items\"",
        "mimic_load_in_flight ",
        "mimic_load_missed_total ",
        "mimic_load_targets 2",
    ] {
        assert!(metrics.contains(series), "{series} missing:\n{metrics}");
    }
    assert!(generator.wait_exit(Duration::from_secs(20)).success());
}

#[test]
fn mon_12_the_planned_rate_is_exposed_in_the_open_model_only() {
    let open = Rig::new(
        &simple_plan(),
        &Params::open(20, 100, 6_000),
        &[Probe::Good],
    );
    let mut generator = open.spawn("1s", &[]);
    let admin = generator.admin_addr();
    std::thread::sleep(Duration::from_millis(2_500));
    let (status, metrics) = http_get(admin, "/metrics");
    assert_eq!(status, 200);
    assert!(
        metrics.contains("# TYPE mimic_load_planned_rate gauge"),
        "{metrics}"
    );
    let planned: f64 = metrics
        .lines()
        .find_map(|l| l.strip_prefix("mimic_load_planned_rate "))
        .and_then(|v| v.parse().ok())
        .expect("a value");
    assert!(planned > 0.0 && planned <= 100.0, "{planned}");
    assert!(generator.wait_exit(Duration::from_secs(20)).success());

    let closed = Rig::new(&simple_plan(), &Params::closed(2, 3_000), &[Probe::Good]);
    let mut generator = closed.spawn("1s", &[]);
    let admin = generator.admin_addr();
    std::thread::sleep(Duration::from_millis(1_500));
    let (_, metrics) = http_get(admin, "/metrics");
    assert!(metrics.contains("mimic_load_targets 1"), "{metrics}");
    assert!(!metrics.contains("mimic_load_planned_rate"), "{metrics}");
    assert!(generator.wait_exit(Duration::from_secs(20)).success());
}

#[test]
fn ldt_14_no_canary_and_no_ticket_in_the_generator_outputs() {
    let rig = Rig::new(
        &credentials_plan(),
        &Params::closed(2, 4_000),
        &[Probe::Good],
    );
    *rig.targets[0].state.echo.lock().unwrap() = Some(ECHO.to_string());
    let dir = rig.credentials_dir();
    std::fs::write(dir.join("token"), format!("{CANARY}\n")).unwrap();
    let mut generator = rig.spawn(
        "1s",
        &[
            ("MIMIC_LOAD_CREDENTIALS_DIR", dir.to_str().unwrap()),
            ("MIMIC_LOG", "debug"),
        ],
    );
    let admin = generator.admin_addr();
    std::thread::sleep(Duration::from_millis(2_000));
    let metrics = http_get(admin, "/metrics").1;
    assert!(generator.wait_exit(Duration::from_secs(20)).success());

    // The credential reached the target in its field, nowhere else.
    let seen = rig.targets[0].state.seen.lock().unwrap();
    assert!(!seen.is_empty());
    assert!(seen.iter().all(|s| s.headers["authorization"] == CANARY));
    drop(seen);

    let mut outputs = generator.lines().to_vec();
    outputs.push(metrics);
    outputs.extend(rig.state().raw_posts());
    // The assert messages name only indexes: printing a canary would itself log it.
    let secrets = [CANARY, ECHO, TICKET, "ticket-payload-3f9b", TOKEN];
    for (o, output) in outputs.iter().enumerate() {
        for (s, secret) in secrets.iter().enumerate() {
            assert!(
                !output.contains(secret),
                "secret #{s} leaked in output #{o}"
            );
        }
    }
}

#[test]
fn ldt_15_a_plan_over_the_limit_is_refused_before_any_request() {
    let mut plan = simple_plan();
    plan.endpoints[0].samples[0].body = Some(Body::Json {
        media: "application/json".to_string(),
        text: format!("{{\"data\":\"{}\"}}", "x".repeat(100 << 10)),
    });
    let rig = Rig::new(&plan, &Params::closed(2, 5_000), &[Probe::Good]);
    let mut generator = rig.spawn("1s", &[("MIMIC_LOAD_MAX_PLAN", "64Ki")]);
    let status = generator.wait_exit(Duration::from_secs(20));
    assert_eq!(status.code(), Some(1));
    assert_eq!(rig.targets[0].accepted(), 0);
    assert_eq!(rig.targets[0].count(), 0);
}

#[test]
fn ldt_15_peak_memory_stays_under_200_mib_with_large_bodies_and_slow_targets() {
    let endpoints = (0..64)
        .map(|i| {
            let body = Body::Json {
                media: "application/json".to_string(),
                text: format!("{{\"data\":\"{}\"}}", "x".repeat(256 << 10)),
            };
            endpoint(
                "POST",
                &format!("/bulk/{i}"),
                1,
                vec![sample(&format!("/bulk/{i}"), &[], Some(body))],
            )
        })
        .collect();
    let plan = Plan {
        credentials: vec![],
        endpoints,
    };
    let rig = Rig::new(
        &plan,
        &Params::closed(128, 8_000),
        &[Probe::Good, Probe::Good],
    );
    for target in &rig.targets {
        target.set_delay(Duration::from_secs(2));
    }
    let mut generator = rig.spawn("1s", &[("MIMIC_LOAD_MAX_PLAN", "32Mi")]);
    let pid = generator.pid();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut peak = 0;
    loop {
        if let Some(kb) = status_kb(pid, "VmHWM:") {
            peak = peak.max(kb);
        }
        if Instant::now() > deadline {
            panic!("generator still running");
        }
        if let Some(status) = generator_exited(&mut generator) {
            assert!(status, "generator failed");
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let sent: usize = rig.targets.iter().map(|t| t.count()).sum();
    assert!(sent > 100, "only {sent} requests: the load did not run");
    assert!(peak > 0, "VmHWM never read");
    assert!(peak < 200 * 1024, "peak RSS {} MiB", peak / 1024);
}

/// `Some(success)` once the process has exited.
fn generator_exited(generator: &mut mimic_runtime::testing::Process) -> Option<bool> {
    if std::path::Path::new(&format!("/proc/{}", generator.pid())).exists() {
        // A zombie keeps its /proc entry: look at its state.
        let status = std::fs::read_to_string(format!("/proc/{}/status", generator.pid()))
            .unwrap_or_default();
        if !status.contains("State:\tZ") {
            return None;
        }
    }
    Some(generator.wait_exit(Duration::from_secs(5)).success())
}

/// A plan whose only sample carries the placeholders the recorder writes.
fn placeholder_plan() -> Plan {
    let mut plan = simple_plan();
    let sample = &mut plan.endpoints[0].samples[0];
    sample.headers = vec![(
        "authorization".to_string(),
        "Bearer mimic-token".to_string(),
    )];
    sample.query = vec![("api_key".to_string(), "mimic-api-key".to_string())];
    plan
}

#[test]
fn plh_09_a_run_without_credentials_sends_the_placeholders() {
    let rig = Rig::new(
        &placeholder_plan(),
        &Params::closed(2, 2_000),
        &[Probe::Good],
    );
    let mut generator = rig.spawn("1s", &[]);
    assert!(generator.wait_exit(Duration::from_secs(20)).success());
    let seen = rig.targets[0].state.seen.lock().unwrap();
    assert!(!seen.is_empty());
    assert!(seen.iter().all(|s| {
        s.headers["authorization"] == "Bearer mimic-token"
            && s.target.contains("api_key=mimic-api-key")
    }));
}

#[test]
fn plh_09_credentials_replace_only_the_supplied_field() {
    let mut plan = placeholder_plan();
    plan.credentials = vec![credential("header/authorization", "token")];
    let rig = Rig::new(&plan, &Params::closed(2, 4_000), &[Probe::Good]);
    let dir = rig.credentials_dir();
    std::fs::write(dir.join("token"), format!("Bearer {CANARY}\n")).unwrap();
    let mut generator = rig.spawn(
        "1s",
        &[
            ("MIMIC_LOAD_CREDENTIALS_DIR", dir.to_str().unwrap()),
            ("MIMIC_LOG", "debug"),
        ],
    );
    let admin = generator.admin_addr();
    std::thread::sleep(Duration::from_millis(2_000));
    let metrics = http_get(admin, "/metrics").1;
    assert!(generator.wait_exit(Duration::from_secs(20)).success());
    let seen = rig.targets[0].state.seen.lock().unwrap();
    assert!(!seen.is_empty());
    assert!(seen.iter().all(|s| {
        s.headers["authorization"] == format!("Bearer {CANARY}")
            && s.target.contains("api_key=mimic-api-key")
    }));
    drop(seen);
    let mut outputs = generator.lines().to_vec();
    outputs.push(metrics);
    assert!(outputs.iter().all(|o| !o.contains(CANARY)));
}
