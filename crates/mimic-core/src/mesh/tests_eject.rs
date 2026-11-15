//! CBR-13: `mesh::lb` and `mesh::retry::Planner` with ejected endpoints.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::time::Duration;

use proptest::prelude::*;

use super::lb::Endpoints;
use super::retry::{Next, Outcome, Planner};

fn addr(n: u8) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, n], 8080))
}

fn endpoints(n: u8) -> Endpoints {
    let mut set = Endpoints::new(Duration::ZERO);
    let addrs: Vec<_> = (1..=n).map(addr).collect();
    set.update(&addrs, Duration::ZERO);
    set
}

fn ejected_of(mask: u8) -> BTreeSet<SocketAddr> {
    (0..8)
        .filter(|i| mask >> i & 1 == 1)
        .map(|i| addr(i + 1))
        .collect()
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

proptest! {
    #[test]
    fn cbr_13_the_pick_never_returns_an_ejected_endpoint_and_says_none_when_all_are(
        n in 1u8..=8, mask in any::<u8>(), seed in 1u64..u64::MAX,
        loads in prop::collection::vec(0u32..50, 8),
    ) {
        let set = endpoints(n);
        let ejected = ejected_of(mask);
        let live = (1..=n).filter(|i| !ejected.contains(&addr(*i))).count();
        let mut r = Rng(seed);
        let got = set.pick_unejected(
            Duration::ZERO,
            |i| loads[i],
            &mut |m| r.below(m),
            |a| ejected.contains(&a),
        );
        match got {
            None => prop_assert_eq!(live, 0, "None only when every endpoint is ejected"),
            Some(i) => {
                prop_assert!(i < usize::from(n));
                let a = set.addr(i).expect("an index of the set");
                prop_assert!(!ejected.contains(&a), "{a} is ejected");
            }
        }
    }

    #[test]
    fn cbr_13_without_ejection_the_pick_is_the_one_of_the_m1_choice(
        n in 1u8..=8, seed in 1u64..u64::MAX, loads in prop::collection::vec(0u32..50, 8),
    ) {
        let set = endpoints(n);
        let (mut a, mut b) = (Rng(seed), Rng(seed));
        let (mut calls_a, mut calls_b) = (0, 0);
        let plain = set.pick(Duration::ZERO, |i| loads[i], &mut |m| { calls_a += 1; a.below(m) });
        let ejecting = set.pick_unejected(
            Duration::ZERO, |i| loads[i], &mut |m| { calls_b += 1; b.below(m) }, |_| false,
        );
        prop_assert_eq!(plain, ejecting);
        prop_assert_eq!(calls_a, calls_b, "the same draws");
    }

    #[test]
    fn cbr_13_the_planner_never_plans_an_ejected_pod(
        n in 1u8..=8, mask in any::<u8>(), retries in 0u32..=10, seed in 1u64..u64::MAX,
        outcomes in prop::collection::vec(0u8..4, 12),
    ) {
        let set = endpoints(n);
        let ejected = ejected_of(mask);
        let mut planner = Planner::new(retries);
        let mut r = Rng(seed);
        for k in outcomes {
            let next = planner.next_unejected(
                &set, Duration::ZERO, |_| 0, &mut |m| r.below(m), |a| ejected.contains(&a),
            );
            match next {
                Next::Pod(pod) => prop_assert!(!ejected.contains(&pod), "{pod} is ejected"),
                Next::Original | Next::Stop => break,
            }
            planner.failed(match k {
                0 => Outcome::ConnectFailed,
                1 => Outcome::Reset,
                2 => Outcome::Overloaded,
                _ => Outcome::RefusedStream,
            });
        }
    }
}

#[test]
fn cbr_13_the_choice_among_the_others_keeps_avoidance() {
    // Endpoint 2 is avoided (a million requests in flight): it is picked only when it is the
    // only one left, as without ejection.
    let set = endpoints(3);
    let load = |i: usize| if i == 1 { 1_000_000 } else { 0 };
    for first in 0..3usize {
        let mut draws = vec![first, 0].into_iter();
        let got = set.pick_unejected(
            Duration::ZERO,
            load,
            &mut |_| draws.next().unwrap_or(0),
            |a| a == addr(3),
        );
        assert_eq!(got, Some(0), "the avoided one loses to the other live one");
    }
    let got = set.pick_unejected(Duration::ZERO, load, &mut |_| 0, |a| a != addr(2));
    assert_eq!(
        got,
        Some(1),
        "the avoided one is picked when it is the only one left"
    );
}

#[test]
fn cbr_13_all_ejected_is_none_and_the_set_is_not_empty() {
    let set = endpoints(3);
    assert_eq!(
        set.pick_unejected(Duration::ZERO, |_| 0, &mut |_| 0, |_| true),
        None
    );
    assert!(!set.is_empty());
}

fn plan(planner: &mut Planner, set: &Endpoints, ejected: &BTreeSet<SocketAddr>) -> Next {
    planner.next_unejected(
        set,
        Duration::ZERO,
        |_| 0,
        &mut |_| 0,
        |a| ejected.contains(&a),
    )
}

#[test]
fn cbr_13_a_first_try_with_every_pod_ejected_has_no_pod() {
    let set = endpoints(2);
    let all: BTreeSet<_> = [addr(1), addr(2)].into();
    assert_eq!(plan(&mut Planner::new(3), &set, &all), Next::Stop);
}

#[test]
fn cbr_13_a_pod_ejected_by_the_requests_own_try_is_no_candidate_then_the_original() {
    // One pod; its connect failure ejects it: only connect failures so far, no candidate.
    let set = endpoints(1);
    let mut ejected = BTreeSet::new();
    let mut planner = Planner::new(3);
    assert_eq!(plan(&mut planner, &set, &ejected), Next::Pod(addr(1)));
    planner.failed(Outcome::ConnectFailed);
    ejected.insert(addr(1));
    assert_eq!(plan(&mut planner, &set, &ejected), Next::Original);
    assert_eq!(plan(&mut planner, &set, &ejected), Next::Stop);
}

#[test]
fn cbr_13_after_another_kind_of_failure_the_request_has_no_fallback() {
    let set = endpoints(1);
    let mut ejected = BTreeSet::new();
    let mut planner = Planner::new(3);
    assert_eq!(plan(&mut planner, &set, &ejected), Next::Pod(addr(1)));
    planner.failed(Outcome::Reset);
    ejected.insert(addr(1));
    assert_eq!(
        plan(&mut planner, &set, &ejected),
        Next::Stop,
        "the failure as is"
    );
}

#[test]
fn cbr_13_untried_ejected_pods_do_not_count_as_untried_for_the_original_destination() {
    // Pods 1 and 2; pod 2 is ejected from the start. After pod 1 failed to connect no live
    // untried pod is left: the original destination.
    let set = endpoints(2);
    let ejected: BTreeSet<_> = [addr(2)].into();
    let mut planner = Planner::new(3);
    assert_eq!(plan(&mut planner, &set, &ejected), Next::Pod(addr(1)));
    planner.failed(Outcome::ConnectFailed);
    assert_eq!(plan(&mut planner, &set, &ejected), Next::Original);
}

#[test]
fn cbr_13_after_a_refusal_a_tried_live_pod_is_reused_and_an_ejected_one_never() {
    let set = endpoints(2);
    let ejected: BTreeSet<_> = [addr(2)].into();
    let mut planner = Planner::new(3);
    assert_eq!(plan(&mut planner, &set, &ejected), Next::Pod(addr(1)));
    planner.failed(Outcome::Overloaded);
    assert_eq!(plan(&mut planner, &set, &ejected), Next::Pod(addr(1)));
}

#[test]
fn cbr_13_without_ejection_the_planner_is_the_one_of_m2() {
    let set = endpoints(3);
    let none = BTreeSet::new();
    let mut a = Planner::new(3);
    let mut b = Planner::new(3);
    for _ in 0..5 {
        let plain = a.next(&set, Duration::ZERO, |_| 0, &mut |_| 0);
        assert_eq!(plain, plan(&mut b, &set, &none));
        a.failed(Outcome::ConnectFailed);
        b.failed(Outcome::ConnectFailed);
    }
}
