//! RTY-01 to RTY-04: classification, trust and planner of `mesh::retry`.

use std::net::SocketAddr;
use std::time::Duration;

use proptest::prelude::*;

use super::lb::Endpoints;
use super::retry::{Failure, Next, Outcome, Planner, Refusal, Skip, classify, delay, trusted};

const METHODS: [&str; 10] = [
    "GET", "HEAD", "OPTIONS", "TRACE", "PUT", "DELETE", "POST", "PATCH", "CONNECT", "PROPFIND",
];
const IDEMPOTENT: [&str; 6] = ["GET", "HEAD", "OPTIONS", "TRACE", "PUT", "DELETE"];

fn addr(n: u8) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, n], 8080))
}

fn endpoints(n: u8) -> Endpoints {
    let mut set = Endpoints::new(Duration::ZERO);
    let addrs: Vec<_> = (1..=n).map(addr).collect();
    set.update(&addrs, Duration::ZERO);
    set
}

#[test]
fn rty_01_connect_failures_and_refused_streams_are_retried_for_every_method() {
    for method in METHODS.iter().chain(["get"].iter()) {
        for held in [true, false] {
            let v = classify(Failure::Connect, method, held, false);
            assert!(v.retry && v.skip.is_none(), "connect {method} held={held}");
            assert_eq!(v.outcome, Outcome::ConnectFailed);
            let v = classify(Failure::RefusedStream, method, true, true);
            assert!(v.retry && v.skip.is_none(), "refused {method}");
            assert_eq!(v.outcome, Outcome::RefusedStream);
        }
    }
}

#[test]
fn rty_01_trusted_refusals_are_retried_for_every_method() {
    for method in METHODS.iter().chain(["get"].iter()) {
        for (refusal, outcome) in [
            (Refusal::Overloaded, Outcome::Overloaded),
            (Refusal::QueueTimeout, Outcome::QueueTimeout),
        ] {
            let v = classify(Failure::Refusal(refusal), method, true, true);
            assert!(v.retry && v.skip.is_none(), "{method}");
            assert_eq!(v.outcome, outcome);
        }
    }
}

#[test]
fn rty_01_reset_after_a_write_is_retried_for_idempotent_methods_only() {
    for method in METHODS.iter().chain(["get"].iter()) {
        let v = classify(Failure::Reset, method, true, true);
        assert_eq!(v.outcome, Outcome::Reset);
        if IDEMPOTENT.contains(method) {
            assert!(v.retry && v.skip.is_none(), "{method}");
        } else {
            assert!(!v.retry, "{method}");
            assert_eq!(v.skip, Some(Skip::Method), "{method}");
        }
    }
}

#[test]
fn rty_01_a_body_not_held_stops_every_retry_once_something_was_written() {
    for method in METHODS {
        for failure in [
            Failure::RefusedStream,
            Failure::Refusal(Refusal::Overloaded),
            Failure::Reset,
        ] {
            let v = classify(failure, method, false, true);
            assert!(!v.retry, "{failure:?} {method}");
            let expected = if failure == Failure::Reset && !IDEMPOTENT.contains(&method) {
                Skip::Method
            } else {
                Skip::Buffer
            };
            assert_eq!(v.skip, Some(expected), "{failure:?} {method}");
        }
    }
}

#[test]
fn rty_01_a_reset_before_any_write_is_retried_for_every_method() {
    let v = classify(Failure::Reset, "POST", false, false);
    assert!(v.retry);
}

#[test]
fn rty_01_other_responses_and_failures_after_the_head_are_never_retried() {
    for method in METHODS {
        for failure in [Failure::Response, Failure::AfterHead] {
            let v = classify(failure, method, true, true);
            assert!(!v.retry && v.skip.is_none(), "{failure:?} {method}");
        }
        assert_eq!(
            classify(Failure::Response, method, true, true).outcome,
            Outcome::Ok
        );
    }
}

#[test]
fn rty_01_the_labels_are_those_of_the_table() {
    let labels: Vec<_> = [
        Outcome::Ok,
        Outcome::ConnectFailed,
        Outcome::RefusedStream,
        Outcome::Reset,
        Outcome::Overloaded,
        Outcome::QueueTimeout,
    ]
    .iter()
    .map(|o| o.label())
    .collect();
    assert_eq!(
        labels,
        [
            "ok",
            "connect_failed",
            "refused_stream",
            "reset",
            "overloaded",
            "queue_timeout"
        ]
    );
    assert_eq!(Skip::Method.label(), "method");
    assert_eq!(Skip::Buffer.label(), "buffer");
}

fn fields<'a>(error: &'a [&'a str], endpoint: &'a [&'a str]) -> Vec<(&'a [u8], &'a [u8])> {
    let mut out: Vec<(&[u8], &[u8])> = Vec::new();
    out.push((b"content-length", b"0"));
    for v in error {
        out.push((b"x-mimic-error", v.as_bytes()));
    }
    for v in endpoint {
        out.push((b"X-Mimic-Endpoint", v.as_bytes()));
    }
    out
}

#[test]
fn rty_02_a_503_bound_to_the_pod_of_the_try_is_trusted() {
    let pod = Some(addr(1));
    assert_eq!(
        trusted(503, &fields(&["overloaded"], &["10.0.0.1:8080"]), pod),
        Some(Refusal::Overloaded)
    );
    assert_eq!(
        trusted(503, &fields(&["queue-timeout"], &["10.0.0.1:8080"]), pod),
        Some(Refusal::QueueTimeout)
    );
    let v6: SocketAddr = "[::1]:8080".parse().unwrap();
    assert_eq!(
        trusted(503, &fields(&["overloaded"], &["[::1]:8080"]), Some(v6)),
        Some(Refusal::Overloaded)
    );
    let upper: Vec<(&[u8], &[u8])> = vec![
        (b"X-MIMIC-ERROR", b"overloaded"),
        (b"x-Mimic-endpoint", b"10.0.0.1:8080"),
    ];
    assert_eq!(trusted(503, &upper, pod), Some(Refusal::Overloaded));
}

#[test]
fn rty_02_anything_else_is_not_trusted() {
    let pod = Some(addr(1));
    let good = fields(&["overloaded"], &["10.0.0.1:8080"]);
    for status in [200, 429, 500, 502, 504] {
        assert_eq!(trusted(status, &good, pod), None, "{status}");
    }
    for value in [
        "connect-failed",
        "no-endpoint",
        "Overloaded",
        "overloaded ",
        "",
    ] {
        let values = [value];
        let f = fields(&values, &["10.0.0.1:8080"]);
        assert_eq!(trusted(503, &f, pod), None, "{value:?}");
    }
    let two_errors = fields(&["overloaded", "overloaded"], &["10.0.0.1:8080"]);
    assert_eq!(trusted(503, &two_errors, pod), None);
    let two_endpoints = fields(&["overloaded"], &["10.0.0.1:8080", "10.0.0.1:8080"]);
    assert_eq!(trusted(503, &two_endpoints, pod), None);
    assert_eq!(trusted(503, &fields(&["overloaded"], &[]), pod), None);
    for value in [
        "10.0.0.2:8080",
        "10.0.0.1:8081",
        "10.0.0.1",
        "::1:8080",
        "[::1]",
        "10.0.0.1:8080 ",
        " 10.0.0.1:8080",
        "pod:8080",
    ] {
        let values = [value];
        let f = fields(&["overloaded"], &values);
        assert_eq!(trusted(503, &f, pod), None, "{value:?}");
    }
    let v6: SocketAddr = "[::1]:8080".parse().unwrap();
    let unbracketed = fields(&["overloaded"], &["::1:8080"]);
    assert_eq!(trusted(503, &unbracketed, Some(v6)), None);
    let bad_utf8: Vec<(&[u8], &[u8])> = vec![
        (b"x-mimic-error", b"overloaded"),
        (b"x-mimic-endpoint", b"\xff"),
    ];
    assert_eq!(trusted(503, &bad_utf8, pod), None);
    assert_eq!(trusted(503, &good, None), None, "original destination");
}

/// Runs a planner whose tries all end with `outcome`; the draws are always 0.
fn run(set: &Endpoints, retries: u32, outcome: Outcome) -> Vec<Next> {
    let mut planner = Planner::new(retries);
    let mut out = Vec::new();
    loop {
        let next = planner.next(set, Duration::ZERO, |_| 0, &mut |_| 0);
        out.push(next);
        if next == Next::Stop {
            return out;
        }
        planner.failed(outcome);
    }
}

fn pods(plan: &[Next]) -> Vec<SocketAddr> {
    plan.iter()
        .filter_map(|n| if let Next::Pod(a) = n { Some(*a) } else { None })
        .collect()
}

#[test]
fn rty_03_connect_failures_try_three_pods_then_the_original_destination() {
    let plan = run(&endpoints(5), 3, Outcome::ConnectFailed);
    assert_eq!(plan.len(), 5, "{plan:?}");
    assert_eq!(plan[3], Next::Original);
    assert_eq!(plan[4], Next::Stop);
    let tried = pods(&plan);
    assert_eq!(tried.len(), 3);
    let distinct: std::collections::BTreeSet<_> = tried.iter().collect();
    assert_eq!(distinct.len(), 3, "distinct pods");
}

#[test]
fn rty_03_one_or_two_endpoints_then_the_original_destination() {
    let plan = run(&endpoints(1), 3, Outcome::ConnectFailed);
    assert_eq!(plan, [Next::Pod(addr(1)), Next::Original, Next::Stop]);
    let plan = run(&endpoints(2), 3, Outcome::ConnectFailed);
    assert_eq!(pods(&plan).len(), 2);
    assert_eq!(plan[2], Next::Original);
    assert_eq!(plan[3], Next::Stop);
}

#[test]
fn rty_03_after_a_refusal_an_untried_pod_then_any_pod_never_the_original() {
    let plan = run(&endpoints(2), 3, Outcome::Overloaded);
    assert_eq!(plan.len(), 5, "{plan:?}");
    assert!(!plan.contains(&Next::Original));
    let tried = pods(&plan);
    assert_ne!(tried[0], tried[1], "the second try is the untried pod");
    let one = run(&endpoints(1), 3, Outcome::Overloaded);
    assert_eq!(one.len(), 5);
    assert!(!one.contains(&Next::Original));
}

#[test]
fn rty_03_a_mixed_sequence_never_goes_to_the_original_destination() {
    let set = endpoints(1);
    let mut planner = Planner::new(3);
    let next = |p: &mut Planner| p.next(&set, Duration::ZERO, |_| 0, &mut |_| 0);
    assert_eq!(next(&mut planner), Next::Pod(addr(1)));
    planner.failed(Outcome::Overloaded);
    assert_eq!(next(&mut planner), Next::Pod(addr(1)));
    planner.failed(Outcome::ConnectFailed);
    assert_eq!(next(&mut planner), Next::Pod(addr(1)));
    planner.failed(Outcome::ConnectFailed);
    assert_eq!(
        next(&mut planner),
        Next::Pod(addr(1)),
        "the last try is a pod too"
    );
    planner.failed(Outcome::ConnectFailed);
    assert_eq!(next(&mut planner), Next::Stop);
}

#[test]
fn rty_03_retries_zero_is_one_try() {
    let plan = run(&endpoints(3), 0, Outcome::ConnectFailed);
    assert_eq!(plan.len(), 2);
    assert!(matches!(plan[0], Next::Pod(_)));
    assert_eq!(plan[1], Next::Stop);
}

#[test]
fn rty_03_nothing_follows_the_original_destination() {
    let mut planner = Planner::new(5);
    let set = endpoints(1);
    planner.next(&set, Duration::ZERO, |_| 0, &mut |_| 0);
    planner.failed(Outcome::ConnectFailed);
    assert_eq!(
        planner.next(&set, Duration::ZERO, |_| 0, &mut |_| 0),
        Next::Original
    );
    planner.failed(Outcome::ConnectFailed);
    assert_eq!(planner.tries(), 2);
    assert_eq!(
        planner.next(&set, Duration::ZERO, |_| 0, &mut |_| 0),
        Next::Stop
    );
}

#[test]
fn rty_03_no_endpoint_is_no_try() {
    let mut planner = Planner::new(3);
    let set = Endpoints::new(Duration::ZERO);
    assert_eq!(
        planner.next(&set, Duration::ZERO, |_| 0, &mut |_| 0),
        Next::Stop
    );
}

#[test]
fn rty_03_draws_are_over_the_candidates_and_in_range() {
    let set = endpoints(5);
    let mut planner = Planner::new(3);
    let mut asked = Vec::new();
    for _ in 0..3 {
        planner.next(&set, Duration::ZERO, |_| 0, &mut |n| {
            asked.push(n);
            n + 7 // out of range on purpose: the planner clamps
        });
        planner.failed(Outcome::ConnectFailed);
    }
    // 5 untried, then 4, then 3 candidates: each pick draws n then n - 1.
    assert_eq!(asked, [5, 4, 4, 3, 3, 2]);
}

#[test]
fn rty_03_delays() {
    for outcome in [
        Outcome::ConnectFailed,
        Outcome::RefusedStream,
        Outcome::Reset,
    ] {
        assert_eq!(delay(outcome, 0.7), Duration::ZERO, "{outcome:?}");
    }
    for outcome in [Outcome::Overloaded, Outcome::QueueTimeout] {
        assert_eq!(delay(outcome, 0.0), Duration::from_micros(12_500));
        assert_eq!(delay(outcome, 1.0), Duration::from_micros(37_500));
        assert_eq!(delay(outcome, 0.5), Duration::from_millis(25));
    }
}

fn outcome_of(n: u8) -> Outcome {
    match n % 6 {
        0 => Outcome::ConnectFailed,
        1 => Outcome::ConnectFailed,
        2 => Outcome::RefusedStream,
        3 => Outcome::Reset,
        4 => Outcome::Overloaded,
        _ => Outcome::QueueTimeout,
    }
}

proptest! {
    #[test]
    fn rty_04_the_planner_keeps_its_bounds(
        n in 0u8..=20,
        retries in 0u32..=5,
        outcomes in proptest::collection::vec(any::<u8>(), 12),
        draws in proptest::collection::vec(any::<usize>(), 64),
    ) {
        let set = endpoints(n);
        let mut planner = Planner::new(retries);
        let mut tried = Vec::<SocketAddr>::new();
        let mut all_connect = true;
        let mut original_seen = false;
        let mut cursor = 0usize;
        let mut count = 0u32;
        for outcome in outcomes {
            let before = tried.clone();
            let next = planner.next(&set, Duration::ZERO, |_| 0, &mut |m| {
                prop_assert_draw(m);
                let d = draws[cursor % draws.len()];
                cursor += 1;
                d % (m + 3)
            });
            match next {
                Next::Stop => break,
                Next::Pod(a) => {
                    prop_assert!(!original_seen, "a pod after the original destination");
                    let untried_left = (1..=n).map(addr).any(|e| !before.contains(&e));
                    if untried_left {
                        prop_assert!(!before.contains(&a), "a tried pod while one is untried");
                    }
                    tried.push(a);
                }
                Next::Original => {
                    prop_assert!(!original_seen);
                    prop_assert!(all_connect, "original destination after another failure");
                    original_seen = true;
                }
            }
            count += 1;
            prop_assert!(count <= 1 + retries);
            let outcome = outcome_of(outcome);
            all_connect &= outcome == Outcome::ConnectFailed;
            planner.failed(outcome);
        }
        prop_assert_eq!(planner.tries(), count);
    }

    #[test]
    fn rty_04_classification_is_a_pure_function(
        kind in 0u8..6,
        method in 0usize..10,
        held in any::<bool>(),
        written in any::<bool>(),
    ) {
        let failure = [
            Failure::Connect,
            Failure::RefusedStream,
            Failure::Refusal(Refusal::Overloaded),
            Failure::Reset,
            Failure::Response,
            Failure::AfterHead,
        ][kind as usize];
        let a = classify(failure, METHODS[method], held, written);
        let b = classify(failure, METHODS[method], held, written);
        prop_assert_eq!(a, b);
    }
}

fn prop_assert_draw(n: usize) {
    assert!(n >= 1, "a draw over no candidate");
}

fn app_fields<'a>(origins: &'a [&'a str], endpoints: &'a [&'a str]) -> Vec<(&'a [u8], &'a [u8])> {
    let mut out: Vec<(&[u8], &[u8])> = vec![(b"x-mimic-error", b"connect-failed")];
    for v in origins {
        out.push((b"X-Mimic-Origin", v.as_bytes()));
    }
    for v in endpoints {
        out.push((b"x-mimic-endpoint", v.as_bytes()));
    }
    out
}

#[test]
fn rty_26_a_connect_failed_with_a_valid_origin_bound_to_the_pod_is_trusted() {
    let pod = Some(addr(1));
    for origin in [
        "inbound",
        "inbound; pod=cart-7d9f-x2k",
        "inbound; pod=a.b-c",
    ] {
        let origins = [origin];
        let f = app_fields(&origins, &["10.0.0.1:8080"]);
        assert_eq!(trusted(503, &f, pod), Some(Refusal::AppRefused), "{origin}");
    }
    let v6: SocketAddr = "[::1]:8080".parse().unwrap();
    let f = app_fields(&["inbound; pod=a"], &["[::1]:8080"]);
    assert_eq!(trusted(503, &f, Some(v6)), Some(Refusal::AppRefused));
    let long = format!("inbound; pod={}", "a".repeat(253));
    let f = app_fields(&[long.as_str()], &["10.0.0.1:8080"]);
    assert_eq!(trusted(503, &f, pod), Some(Refusal::AppRefused));
}

#[test]
fn rty_26_a_connect_failed_without_a_valid_origin_is_not_trusted() {
    let pod = Some(addr(1));
    let long = format!("inbound; pod={}", "a".repeat(254));
    for origin in [
        "Inbound",
        "inbound;pod=a",
        "inbound; pod=",
        "inbound; pod=A",
        "inbound; pod=a b",
        "inbound; pod=a; x=1",
        "inbound ",
        "outbound",
        "",
        long.as_str(),
    ] {
        let origins = [origin];
        let f = app_fields(&origins, &["10.0.0.1:8080"]);
        assert_eq!(trusted(503, &f, pod), None, "{origin:?}");
    }
    let good = app_fields(&["inbound"], &["10.0.0.1:8080"]);
    assert_eq!(
        trusted(503, &app_fields(&[], &["10.0.0.1:8080"]), pod),
        None
    );
    assert_eq!(
        trusted(
            503,
            &app_fields(&["inbound", "inbound"], &["10.0.0.1:8080"]),
            pod
        ),
        None
    );
    for status in [200, 500, 502] {
        assert_eq!(trusted(status, &good, pod), None, "{status}");
    }
    assert_eq!(trusted(503, &good, None), None, "no pod");
    assert_eq!(trusted(503, &good, Some(addr(2))), None, "another pod");
    let two = app_fields(&["inbound"], &["10.0.0.1:8080", "10.0.0.1:8080"]);
    assert_eq!(trusted(503, &two, pod), None);
    let mut errors = app_fields(&["inbound"], &["10.0.0.1:8080"]);
    errors.push((b"x-mimic-error", b"connect-failed"));
    assert_eq!(trusted(503, &errors, pod), None);
}

#[test]
fn rty_26_the_origin_does_not_change_overloaded_and_queue_timeout() {
    let pod = Some(addr(1));
    for origin in ["inbound", "outbound", "x", "inbound; pod=A"] {
        let mut f = fields(&["overloaded"], &["10.0.0.1:8080"]);
        f.push((b"x-mimic-origin", origin.as_bytes()));
        assert_eq!(trusted(503, &f, pod), Some(Refusal::Overloaded), "{origin}");
    }
}

#[test]
fn rty_26_an_app_refusal_is_retried_for_every_method_at_once() {
    for method in METHODS.iter().chain(["get"].iter()) {
        let v = classify(Failure::Refusal(Refusal::AppRefused), method, true, true);
        assert!(v.retry && v.skip.is_none(), "{method}");
        assert_eq!(v.outcome, Outcome::AppRefused);
        let v = classify(Failure::Refusal(Refusal::AppRefused), method, false, true);
        assert_eq!(v.skip, Some(Skip::Buffer), "{method}");
    }
    assert_eq!(Outcome::AppRefused.label(), "app_refused");
    assert_eq!(delay(Outcome::AppRefused, 0.9), Duration::ZERO);
}

#[test]
fn rty_26_after_app_refusals_the_planner_never_ends_on_the_original_destination() {
    let set = endpoints(2);
    let mut planner = Planner::new(3);
    let mut draw = |n: usize| n / 2;
    let mut tried = Vec::new();
    for _ in 0..4 {
        let next = planner.next(&set, Duration::from_secs(100), |_| 0, &mut draw);
        assert!(!matches!(next, Next::Original), "{next:?}");
        if let Next::Pod(pod) = next {
            tried.push(pod);
            planner.failed(Outcome::AppRefused);
        }
    }
    assert_eq!(tried.len(), 4);
    assert_eq!(
        planner.next(&set, Duration::from_secs(100), |_| 0, &mut draw),
        Next::Stop
    );
}
