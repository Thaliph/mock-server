//! RTE-60 to RTE-62: deadlines of `mesh::timeout`, `Timeout` in `mesh::retry`, compile and
//! `View::from_json` of `timeouts`.

use std::time::Duration;

use proptest::prelude::*;

use super::retry::{Budget, Decision, Failure, Outcome, Planner, classify, decide, delay};
use super::route::TimeoutsView;
use super::timeout::{Deadline, Kind, call_deadline, try_deadline};

const WORST: u64 = 1_439_985_600_000;

fn view(request: Option<u64>, backend: Option<u64>) -> TimeoutsView {
    TimeoutsView {
        request_ms: request,
        backend_request_ms: backend,
    }
}

fn at(at_ms: u64, kind: Kind) -> Option<Deadline> {
    Some(Deadline { at_ms, kind })
}

#[test]
fn rte_60_no_timeout_gives_no_deadline() {
    for t in [
        None,
        Some(view(None, None)),
        Some(view(Some(0), None)),
        Some(view(None, Some(0))),
        Some(view(Some(0), Some(0))),
    ] {
        let t = t.as_ref();
        assert_eq!(call_deadline(t), None, "{t:?}");
        for start in [0, 50, u64::MAX] {
            assert_eq!(try_deadline(t, start), None, "{t:?} at {start}");
        }
    }
}

#[test]
fn rte_60_the_worked_case() {
    let t = view(Some(1000), Some(400));
    let t = Some(&t);
    assert_eq!(call_deadline(t), at(1000, Kind::Request));
    assert_eq!(try_deadline(t, 0), at(400, Kind::BackendRequest));
    assert_eq!(try_deadline(t, 50), at(450, Kind::BackendRequest));
    assert_eq!(try_deadline(t, 700), at(1000, Kind::Request));
    assert_eq!(try_deadline(t, 600), at(1000, Kind::Request), "tie");
    assert_eq!(try_deadline(t, 599), at(999, Kind::BackendRequest));
    assert_eq!(try_deadline(t, 1000), at(1000, Kind::Request));
    assert_eq!(try_deadline(t, 5000), at(1000, Kind::Request));
}

#[test]
fn rte_60_a_backend_timeout_beyond_the_call_is_the_calls() {
    let t = view(Some(1000), Some(2000));
    assert_eq!(try_deadline(Some(&t), 0), at(1000, Kind::Request));
}

#[test]
fn rte_60_only_a_backend_timeout_is_per_try() {
    let t = view(None, Some(300));
    let t = Some(&t);
    assert_eq!(call_deadline(t), None);
    assert_eq!(try_deadline(t, 0), at(300, Kind::BackendRequest));
    assert_eq!(try_deadline(t, 1000), at(1300, Kind::BackendRequest));
}

#[test]
fn rte_60_only_a_request_timeout_is_the_deadline_of_every_try() {
    let t = view(Some(500), None);
    let t = Some(&t);
    assert_eq!(try_deadline(t, 0), at(500, Kind::Request));
    assert_eq!(try_deadline(t, 200), at(500, Kind::Request));
}

#[test]
fn rte_60_the_largest_values_and_u64_max_do_not_wrap() {
    for (request, backend) in [
        (Some(WORST), Some(WORST)),
        (Some(u64::MAX), Some(u64::MAX)),
        (None, Some(u64::MAX)),
        (Some(u64::MAX), None),
        (Some(1), Some(u64::MAX)),
    ] {
        let t = view(request, backend);
        for start in [0, 1, WORST, u64::MAX - 1, u64::MAX] {
            let _ = call_deadline(Some(&t));
            let _ = try_deadline(Some(&t), start);
        }
    }
    // Past u64::MAX is no deadline.
    let t = view(None, Some(u64::MAX));
    assert_eq!(try_deadline(Some(&t), 1), None);
    let t = view(None, Some(WORST));
    assert_eq!(
        try_deadline(Some(&t), u64::MAX - WORST + 1),
        None,
        "one past"
    );
    assert_eq!(
        try_deadline(Some(&t), u64::MAX - WORST),
        at(u64::MAX, Kind::BackendRequest)
    );
    // With a call deadline the sum that overflows leaves the call's.
    let t = view(Some(WORST), Some(u64::MAX));
    assert_eq!(try_deadline(Some(&t), 10), at(WORST, Kind::Request));
    let t = view(Some(u64::MAX), Some(u64::MAX));
    assert_eq!(try_deadline(Some(&t), 5), at(u64::MAX, Kind::Request));
}

proptest! {
    #[test]
    fn rte_60_a_try_deadline_is_within_its_start_and_the_calls(
        request in proptest::option::of(any::<u64>()),
        backend in proptest::option::of(any::<u64>()),
        start in any::<u64>(),
    ) {
        let t = view(request, backend);
        let call = call_deadline(Some(&t));
        if let Some(d) = try_deadline(Some(&t), start) {
            if let Some(call) = call {
                prop_assert!(d.at_ms <= call.at_ms);
                if start <= call.at_ms {
                    prop_assert!(d.at_ms >= start);
                }
            } else {
                prop_assert!(d.at_ms >= start);
                prop_assert_eq!(d.kind, Kind::BackendRequest);
            }
        } else {
            prop_assert!(call.is_none());
        }
    }
}

const METHODS: [&str; 10] = [
    "GET", "HEAD", "OPTIONS", "TRACE", "PUT", "DELETE", "POST", "PATCH", "CONNECT", "PROPFIND",
];

#[test]
fn rte_61_a_timeout_is_never_retried_whatever_the_request() {
    for method in METHODS.iter().chain(["get"].iter()) {
        for held in [true, false] {
            for written in [true, false] {
                let v = classify(Failure::Timeout, method, held, written);
                assert!(!v.retry, "{method} {held} {written}");
                assert!(v.skip.is_none(), "{method} {held} {written}");
                assert_eq!(v.outcome, Outcome::Timeout);
            }
        }
    }
    assert_eq!(Outcome::Timeout.label(), "timeout");
    assert_eq!(Outcome::Ok.label(), "ok");
}

#[test]
fn rte_61_a_timeout_asks_nothing_of_the_budget() {
    let now = Duration::from_secs(3);
    let mut budget = Budget::new(20, 10, Duration::from_secs(10));
    budget.deposit(now);
    let planner = Planner::new(3);
    let before = budget.counts(now);
    for method in METHODS {
        let v = classify(Failure::Timeout, method, true, true);
        assert_eq!(decide(v, &planner, &mut budget, now), Decision::Done);
        assert_eq!(budget.counts(now), before);
    }
    // Not even with a budget that would refuse.
    let mut empty = Budget::new(0, 0, Duration::from_secs(10));
    let v = classify(Failure::Timeout, "GET", true, false);
    assert_eq!(decide(v, &planner, &mut empty, now), Decision::Done);
    assert_eq!(empty.counts(now), (0, 0));
}

#[test]
fn rte_61_a_timeout_has_no_delay() {
    for unit in [0.0, 0.5, 1.0, f64::NAN] {
        assert_eq!(delay(Outcome::Timeout, unit), Duration::ZERO);
    }
}

#[test]
fn rte_61_a_timeout_ends_a_sequence_of_tries() {
    let mut planner = Planner::new(3);
    planner.failed(Outcome::Timeout);
    // The caller stops at `Done`; the planner keeps no state that allows the original
    // destination after a failure that is not a connect failure.
    assert_eq!(planner.tries(), 0);
}
