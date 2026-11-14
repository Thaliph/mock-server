//! RTY-40 to RTY-43: the retry budget of `mesh::retry` and the decision that asks it.

use std::net::SocketAddr;
use std::time::Duration;

use proptest::prelude::*;

use super::lb::Endpoints;
use super::retry::{
    BUDGET_SLOTS, Budget, Decision, Failure, Next, Outcome, Planner, Skip, classify, decide,
};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// How many retries are granted in a row at `now`.
fn grants(budget: &mut Budget, now: Duration, cap: u32) -> u32 {
    let mut n = 0;
    while n < cap && budget.withdraw(now) {
        n += 1;
    }
    n
}

#[test]
fn rty_40_the_floor_grants_100_retries_in_a_window_of_10_s() {
    let mut budget = Budget::new(20, 10, secs(10));
    assert_eq!(grants(&mut budget, Duration::ZERO, 1000), 100);
    assert!(!budget.withdraw(Duration::ZERO));
}

#[test]
fn rty_40_a_thousand_requests_bring_the_grants_to_300() {
    let mut budget = Budget::new(20, 10, secs(10));
    for _ in 0..1000 {
        budget.deposit(ms(100));
    }
    assert_eq!(grants(&mut budget, ms(100), 1000), 300);
}

#[test]
fn rty_40_zero_percent_and_zero_floor_grant_none() {
    let mut budget = Budget::new(0, 0, secs(10));
    for _ in 0..1000 {
        budget.deposit(Duration::ZERO);
    }
    assert!(!budget.withdraw(Duration::ZERO));
}

#[test]
fn rty_40_one_hundred_percent_grants_one_retry_per_request() {
    let mut budget = Budget::new(100, 0, secs(10));
    assert!(!budget.withdraw(Duration::ZERO));
    for _ in 0..7 {
        budget.deposit(Duration::ZERO);
    }
    assert_eq!(grants(&mut budget, Duration::ZERO, 100), 7);
}

#[test]
fn rty_40_a_refusal_counts_nothing_and_a_grant_counts_at_once() {
    let mut budget = Budget::new(0, 1, secs(10));
    assert_eq!(budget.counts(Duration::ZERO), (0, 0));
    assert_eq!(grants(&mut budget, Duration::ZERO, 100), 10);
    assert_eq!(budget.counts(Duration::ZERO), (0, 10));
    for _ in 0..5 {
        assert!(!budget.withdraw(Duration::ZERO));
        assert_eq!(budget.counts(Duration::ZERO), (0, 10));
    }
    budget.deposit(Duration::ZERO);
    assert_eq!(budget.counts(Duration::ZERO), (1, 10));
}

#[test]
fn rty_40_counts_saturate() {
    let mut budget = Budget::new(100, 0, secs(10));
    for _ in 0..3 {
        // The slot holds u32::MAX once saturated; adding one by one would take too long.
        budget.deposit_many_for_test(Duration::ZERO, u32::MAX);
    }
    assert_eq!(budget.counts(Duration::ZERO).0, u64::from(u32::MAX));
    assert!(budget.withdraw(Duration::ZERO));
    let mut budget = Budget::new(100, 0, secs(10));
    budget.deposit_many_for_test(Duration::ZERO, u32::MAX);
    for _ in 0..3 {
        budget.deposit_many_for_test(ms(1000), u32::MAX);
    }
    assert!(budget.withdraw(ms(1000)));
}

#[test]
fn rty_40_the_formula_is_in_integers() {
    // 33 % of 3 requests is 0.99 of a retry: none; of 4 requests 1.32: one.
    let mut budget = Budget::new(33, 0, secs(10));
    for _ in 0..3 {
        budget.deposit(Duration::ZERO);
    }
    assert!(!budget.withdraw(Duration::ZERO));
    budget.deposit(Duration::ZERO);
    assert_eq!(grants(&mut budget, Duration::ZERO, 10), 1);
}

#[test]
fn rty_41_events_count_until_the_end_of_the_tenth_slot() {
    // A retry granted at 0.5 s is held at 9.999 s and gone at 10.0 s.
    let mut budget = Budget::new(0, 1, secs(10));
    assert_eq!(grants(&mut budget, ms(500), 100), 10);
    assert_eq!(budget.counts(ms(9999)), (0, 10));
    assert!(!budget.withdraw(ms(9999)));
    assert_eq!(budget.counts(ms(10_000)), (0, 0));
    // The same for a request.
    let mut budget = Budget::new(100, 0, secs(10));
    budget.deposit(ms(500));
    assert_eq!(budget.counts(ms(9999)), (1, 0));
    assert_eq!(budget.counts(ms(10_000)), (0, 0));
}

#[test]
fn rty_41_a_window_of_1_s_has_slots_of_100_ms() {
    let mut budget = Budget::new(0, 1, secs(1));
    assert_eq!(grants(&mut budget, ms(50), 100), 1);
    assert_eq!(budget.counts(ms(999)), (0, 1));
    assert_eq!(budget.counts(ms(1000)), (0, 0));
    let mut budget = Budget::new(100, 0, secs(1));
    budget.deposit(ms(50));
    assert_eq!(budget.counts(ms(999)), (1, 0));
    assert_eq!(budget.counts(ms(1000)), (0, 0));
}

#[test]
fn rty_41_a_silence_longer_than_the_window_clears_everything_in_one_step() {
    for gap in [10_000, 10_001, 3_600_000, u64::MAX] {
        let mut budget = Budget::new(100, 1, secs(10));
        budget.deposit(ms(100));
        assert!(budget.withdraw(ms(100)));
        assert_eq!(budget.counts(ms(100).saturating_add(ms(gap))), (0, 0));
    }
    let mut budget = Budget::new(100, 1, secs(10));
    budget.deposit(ms(100));
    assert_eq!(budget.counts(Duration::from_millis(u64::MAX)), (0, 0));
    budget.deposit(Duration::MAX);
    assert_eq!(budget.counts(Duration::MAX), (1, 0));
}

#[test]
fn rty_41_a_clock_that_goes_back_counts_in_the_latest_slot_and_frees_nothing() {
    let mut budget = Budget::new(0, 1, secs(10));
    assert_eq!(grants(&mut budget, ms(5000), 100), 10);
    // Back to 0: nothing is freed, the grants stay.
    assert!(!budget.withdraw(Duration::ZERO));
    assert_eq!(budget.counts(Duration::ZERO), (0, 10));
    // Events of the past land in the latest slot (5): they expire with it, not earlier.
    budget.deposit(ms(100));
    assert_eq!(budget.counts(ms(14_999)), (1, 10));
    assert_eq!(budget.counts(ms(15_000)), (0, 0));
}

#[test]
fn rty_41_the_state_is_a_fixed_array_of_10_slots() {
    assert_eq!(BUDGET_SLOTS, 10);
    let mut budget = Budget::new(20, 10, secs(10));
    let before = std::mem::size_of_val(&budget);
    for i in 0..10_000 {
        budget.deposit(ms(i));
        let _ = budget.withdraw(ms(i));
    }
    assert_eq!(std::mem::size_of_val(&budget), before);
    assert!(before <= 512, "{before}");
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Request,
    Ask,
}

fn ops() -> impl Strategy<Value = Vec<(u64, Op)>> {
    proptest::collection::vec(
        (
            0u64..3000,
            prop_oneof![Just(Op::Request), Just(Op::Ask), Just(Op::Ask)],
        ),
        0..300,
    )
}

/// Runs the sequence; returns the answers and the per-slot requests and grants.
fn replay(
    percent: u32,
    min: u32,
    window: u64,
    seq: &[(u64, Op)],
) -> (Vec<bool>, Vec<(u64, u64, u64)>) {
    let mut budget = Budget::new(percent, min, secs(window));
    let slot_ms = window * 100;
    let mut now = 0;
    let mut answers = Vec::new();
    let mut slots: Vec<(u64, u64, u64)> = Vec::new();
    for (dt, op) in seq {
        now += dt;
        let slot = now / slot_ms;
        if slots.last().is_none_or(|s| s.0 != slot) {
            slots.push((slot, 0, 0));
        }
        let entry = slots.last_mut().unwrap();
        match op {
            Op::Request => {
                budget.deposit(ms(now));
                entry.1 += 1;
            }
            Op::Ask => {
                let granted = budget.withdraw(ms(now));
                answers.push(granted);
                entry.2 += u64::from(granted);
            }
        }
    }
    (answers, slots)
}

proptest! {
    #[test]
    fn rty_42_a_span_of_10_slots_never_exceeds_the_formula(
        percent in 0u32..=100,
        min in 0u32..=1000,
        window in 1u64..=60,
        seq in ops(),
    ) {
        let (_, slots) = replay(percent, min, window, &seq);
        // Every span of 10 slots that ends at a slot where a retry was granted (the decision
        // looks at the window ending at its own slot).
        for end in slots.iter().filter(|s| s.2 > 0).map(|s| s.0) {
            let (req, got) = slots
                .iter()
                .filter(|s| s.0 + 10 > end && s.0 <= end)
                .fold((0, 0), |a, s| (a.0 + s.1, a.1 + s.2));
            prop_assert!(
                100 * got <= u64::from(percent) * req + 100 * u64::from(min) * window,
                "{got} granted over {req} requests"
            );
        }
    }

    #[test]
    fn rty_42_the_answers_are_a_pure_function_of_the_sequence(
        percent in 0u32..=100,
        min in 0u32..=1000,
        window in 1u64..=60,
        seq in ops(),
    ) {
        prop_assert_eq!(
            replay(percent, min, window, &seq).0,
            replay(percent, min, window, &seq).0
        );
    }
}

fn addr(n: u8) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, n], 8080))
}

fn endpoints(n: u8) -> Endpoints {
    let mut set = Endpoints::new(Duration::ZERO);
    let addrs: Vec<_> = (1..=n).map(addr).collect();
    set.update(&addrs, Duration::ZERO);
    set
}

fn next(planner: &mut Planner, set: &Endpoints) -> Next {
    planner.next(set, Duration::ZERO, |_| 0, &mut |_| 0)
}

#[test]
fn rty_43_a_failure_that_is_not_retried_leaves_the_counts_unchanged() {
    let set = endpoints(3);
    let mut budget = Budget::new(20, 10, secs(10));
    let mut planner = Planner::new(3);
    assert!(matches!(next(&mut planner, &set), Next::Pod(_)));
    let held = [
        (
            Failure::Reset,
            "POST",
            true,
            true,
            Decision::Skipped(Skip::Method),
        ),
        (
            Failure::Connect,
            "GET",
            false,
            true,
            Decision::Skipped(Skip::Buffer),
        ),
        (Failure::Response, "GET", true, true, Decision::Done),
        (Failure::AfterHead, "GET", true, true, Decision::Done),
    ];
    for (failure, method, body_held, written, want) in held {
        let verdict = classify(failure, method, body_held, written);
        let got = decide(verdict, &planner, &mut budget, Duration::ZERO);
        assert_eq!(got, want, "{failure:?}");
        assert_eq!(budget.counts(Duration::ZERO), (0, 0), "{failure:?}");
    }
}

#[test]
fn rty_43_no_try_left_asks_nothing() {
    let set = endpoints(3);
    for retries in [0, 2] {
        let mut budget = Budget::new(20, 10, secs(10));
        let mut planner = Planner::new(retries);
        while next(&mut planner, &set) != Next::Stop {
            planner.failed(Outcome::Reset);
        }
        let verdict = classify(Failure::Reset, "GET", true, false);
        assert!(verdict.retry);
        assert_eq!(
            decide(verdict, &planner, &mut budget, Duration::ZERO),
            Decision::Exhausted
        );
        assert_eq!(budget.counts(Duration::ZERO), (0, 0));
    }
    // retries 0: one try, no ask.
    let mut budget = Budget::new(20, 10, secs(10));
    let mut planner = Planner::new(0);
    assert!(matches!(next(&mut planner, &set), Next::Pod(_)));
    let verdict = classify(Failure::Connect, "GET", true, false);
    assert_eq!(
        decide(verdict, &planner, &mut budget, Duration::ZERO),
        Decision::Exhausted
    );
    assert_eq!(budget.counts(Duration::ZERO), (0, 0));
}

#[test]
fn rty_43_a_grant_becomes_the_next_try_to_a_pod_or_the_original_destination() {
    let set = endpoints(1);
    let mut budget = Budget::new(0, 1, secs(10));
    let mut planner = Planner::new(3);
    assert!(matches!(next(&mut planner, &set), Next::Pod(_)));
    let verdict = classify(Failure::Connect, "POST", true, false);
    planner.failed(verdict.outcome);
    assert_eq!(
        decide(verdict, &planner, &mut budget, Duration::ZERO),
        Decision::Retry
    );
    assert_eq!(budget.counts(Duration::ZERO), (0, 1));
    // The only pod was tried: the next try is the original destination, which asks too.
    assert_eq!(next(&mut planner, &set), Next::Original);
    // After the original destination the planner has no try left and asks nothing.
    planner.failed(Outcome::ConnectFailed);
    assert_eq!(
        decide(verdict, &planner, &mut budget, Duration::ZERO),
        Decision::Exhausted
    );
    assert_eq!(budget.counts(Duration::ZERO), (0, 1));
}

#[test]
fn rty_43_a_refusal_ends_the_tries_and_the_failure_stands() {
    let set = endpoints(3);
    let mut budget = Budget::new(0, 0, secs(10));
    let mut planner = Planner::new(3);
    assert!(matches!(next(&mut planner, &set), Next::Pod(_)));
    let verdict = classify(Failure::RefusedStream, "GET", true, true);
    planner.failed(verdict.outcome);
    assert_eq!(
        decide(verdict, &planner, &mut budget, Duration::ZERO),
        Decision::Budget
    );
    // The verdict, and so its result label, is the caller's to keep.
    assert_eq!(verdict.outcome, Outcome::RefusedStream);
    assert_eq!(budget.counts(Duration::ZERO), (0, 0));
    assert_eq!(
        decide(verdict, &planner, &mut budget, Duration::ZERO),
        Decision::Budget
    );
}
