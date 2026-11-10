//! LDT-13 on the real `mimic-load` binary (spec §7.8): stops, and FND-01 to FND-03.

mod common;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::*;
use mimic_runtime::testing::{http_get, run, wait_status};

/// No request reached any target later than `window` after `at`.
fn silent_after(rig: &Rig, at: Instant, window: Duration) {
    for target in &rig.targets {
        if let Some(last) = *target.state.last_request.lock().unwrap() {
            assert!(
                last <= at + window,
                "{} got a request {:?} after the refusal",
                target.pod,
                last.saturating_duration_since(at)
            );
        }
    }
}

fn refusal_stops_the_run(answer: Answer, reason: &str) {
    let rig = Rig::new(
        &simple_plan(),
        &Params::closed(8, 60_000),
        &[Probe::Good, Probe::Good],
    );
    for target in &rig.targets {
        target.set_delay(Duration::from_millis(20));
    }
    let mut generator = rig.spawn("1s", &[]);
    std::thread::sleep(Duration::from_secs(2));
    assert!(rig.targets.iter().all(|t| t.count() > 0));
    rig.targets[0].set_answer(answer);
    let status = generator.wait_exit(Duration::from_secs(20));
    assert_eq!(status.code(), Some(1));
    let refused = rig.targets[0]
        .state
        .first_refusal
        .lock()
        .unwrap()
        .expect("a refusal was sent");
    silent_after(&rig, refused, Duration::from_secs(2));
    assert_eq!(rig.state().final_post().unwrap()["reason"], reason);
}

#[test]
fn ldt_13_a_mode_refusal_stops_every_request_within_2_seconds() {
    refusal_stops_the_run(Answer::Refused("mode"), "ModeLeft");
}

#[test]
fn ldt_13_an_answer_without_the_marker_stops_the_run_unverified() {
    refusal_stops_the_run(Answer::NoMarker, "Unverified");
}

#[test]
fn ldt_13_stop_in_the_targets_answer_stops_within_one_interval() {
    let rig = Rig::new(&simple_plan(), &Params::closed(4, 60_000), &[Probe::Good]);
    rig.targets[0].set_delay(Duration::from_millis(20));
    let mut generator = rig.spawn("1s", &[]);
    std::thread::sleep(Duration::from_secs(2));
    assert!(rig.targets[0].count() > 0);
    let asked = Instant::now();
    rig.state().stop.store(true, Ordering::SeqCst);
    let status = generator.wait_exit(Duration::from_secs(20));
    assert_eq!(status.code(), Some(1));
    // One interval, and the time of the final post.
    silent_after(&rig, asked, Duration::from_millis(1_800));
    assert!(rig.state().final_post().is_some());
}

#[test]
fn ldt_13_sigterm_posts_the_final_results_within_the_grace_period() {
    let rig = Rig::new(&simple_plan(), &Params::closed(4, 60_000), &[Probe::Good]);
    rig.targets[0].set_delay(Duration::from_millis(20));
    let mut generator = rig.spawn("5s", &[("MIMIC_SHUTDOWN_GRACE", "3s")]);
    std::thread::sleep(Duration::from_secs(2));
    assert!(rig.targets[0].count() > 0);
    let sent = Instant::now();
    generator.terminate();
    generator.wait_exit(Duration::from_secs(4));
    assert!(sent.elapsed() <= Duration::from_secs(3) + Duration::from_millis(500));
    let post = rig.state().final_post().expect("a final post");
    assert!(
        post["results"]["endpoints"][0]["requests"]
            .as_u64()
            .unwrap()
            > 0
    );
    generator.wait_log(Duration::from_secs(1), |v| {
        v["message"] == "shutdown complete"
    });
}

#[test]
fn ldt_13_without_the_controller_for_three_intervals_the_generator_stops() {
    let rig = Rig::new(&simple_plan(), &Params::closed(4, 60_000), &[Probe::Good]);
    rig.targets[0].set_delay(Duration::from_millis(20));
    let mut generator = rig.spawn("1s", &[]);
    std::thread::sleep(Duration::from_secs(2));
    assert!(rig.targets[0].count() > 0);
    let down = Instant::now();
    rig.state().down.store(true, Ordering::SeqCst);
    let status = generator.wait_exit(Duration::from_secs(15));
    assert_eq!(status.code(), Some(1));
    // Three intervals since the last answer, which came up to one interval before the outage.
    assert!(
        down.elapsed() >= Duration::from_secs(2),
        "{:?}",
        down.elapsed()
    );
    silent_after(&rig, down, Duration::from_secs(6));
}

#[test]
fn ldt_10_a_404_on_targets_or_results_is_a_stop_without_further_posts() {
    let rig = Rig::new(&simple_plan(), &Params::closed(4, 60_000), &[Probe::Good]);
    rig.targets[0].set_delay(Duration::from_millis(20));
    let mut generator = rig.spawn("1s", &[]);
    std::thread::sleep(Duration::from_secs(2));
    assert!(rig.targets[0].count() > 0);
    let ended = Instant::now();
    rig.state().ended.store(true, Ordering::SeqCst);
    let status = generator.wait_exit(Duration::from_secs(15));
    assert_eq!(status.code(), Some(1));
    assert!(
        ended.elapsed() < Duration::from_secs(5),
        "{:?}",
        ended.elapsed()
    );
    // The run is gone: at most the post of the report that saw the 404 itself, no retries.
    assert!(rig.state().late.load(Ordering::SeqCst) <= 1);
    silent_after(&rig, ended, Duration::from_secs(3));
}

#[test]
fn ldt_13_fnd_01_invalid_values_exit_2_before_listening() {
    let rig = Rig::new(&simple_plan(), &Params::closed(1, 1_000), &[Probe::Good]);
    let cases = [
        ("MIMIC_LOAD_REPORT_INTERVAL", "0s"),
        ("MIMIC_LOAD_REPORT_INTERVAL", "2m"),
        ("MIMIC_LOAD_MAX_PLAN", "1Ki"),
        ("MIMIC_LOAD_MAX_PLAN", "lots"),
        ("MIMIC_LOAD_RUN", "payment-load"),
        ("MIMIC_LOAD_DEPLOYMENT", "Not A Name"),
        ("MIMIC_CONTROLLER_URL", "http://127.0.0.1:1"),
        ("MIMIC_CONTROLLER_CA", "not pem"),
        ("MIMIC_SHUTDOWN_GRACE", "soon"),
    ];
    for (var, value) in cases {
        let mut env = rig.vars("1s");
        env.retain(|(k, _)| k != var);
        env.push((var.to_string(), value.to_string()));
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let (status, stdout, stderr) = run(BIN, &[], &env);
        assert_eq!(status.code(), Some(2), "{var}={value}: {stderr}");
        assert!(stderr.contains(var), "{var}: {stderr}");
        assert!(!stdout.contains("admin listening"), "{stdout}");
    }
    for var in [
        "MIMIC_LOAD_RUN",
        "MIMIC_LOAD_DEPLOYMENT",
        "MIMIC_CONTROLLER_URL",
        "MIMIC_CONTROLLER_CA",
    ] {
        let mut env = rig.vars("1s");
        env.retain(|(k, _)| k != var);
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let (status, _, stderr) = run(BIN, &[], &env);
        assert_eq!(status.code(), Some(2), "missing {var}: {stderr}");
        assert!(stderr.contains(var), "{stderr}");
    }
    assert_eq!(rig.targets[0].count(), 0);
}

#[test]
fn ldt_13_fnd_02_and_03_health_readiness_and_metrics() {
    let rig = Rig::new(&simple_plan(), &Params::closed(1, 3_000), &[Probe::Good]);
    let mut generator = rig.spawn("1s", &[]);
    let admin = generator.admin_addr();
    assert_eq!(http_get(admin, "/healthz").0, 200);
    wait_status(admin, "/readyz", 200, Duration::from_secs(10));
    let (status, metrics) = http_get(admin, "/metrics");
    assert_eq!(status, 200);
    let build = format!(
        "mimic_build_info{{component=\"load\",version=\"{}\"}} 1",
        env!("CARGO_PKG_VERSION")
    );
    assert!(metrics.contains(&build), "{metrics}");
    assert!(generator.wait_exit(Duration::from_secs(20)).success());
}
