//! CBR-07 to CBR-12: the outlier state machine of `mesh::outlier`.

use std::net::SocketAddr;
use std::time::Duration;

use proptest::prelude::*;

use super::outlier::{Cause, Ejection, Outcome, Outlier, Reason, Restoration};
use super::policy::OutlierDetection;

fn addr(n: u8) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, n], 8080))
}

fn addrs(n: u8) -> Vec<SocketAddr> {
    (1..=n).map(addr).collect()
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn policy(failures: u32, fives: u32, base: u32, percent: u32) -> OutlierDetection {
    OutlierDetection {
        consecutive_failures: failures,
        consecutive_5xx: fives,
        base_ejection_seconds: base,
        max_ejection_percent: percent,
    }
}

/// `n` endpoints under `policy`, at time 0.
fn state(policy: &OutlierDetection, n: u8) -> Outlier {
    let mut o = Outlier::new(policy);
    assert!(o.reconcile(policy, &addrs(n), Duration::ZERO).is_empty());
    o
}

/// `count` failures on `a`, one second apart from `start`; the last answer.
fn fail(o: &mut Outlier, a: SocketAddr, count: u32, now: Duration) -> Option<Ejection> {
    let mut last = None;
    for _ in 0..count {
        last = o.record(a, Outcome::Reset, now);
    }
    last
}

/// Every outcome of the table with the `(failures, 5xx)` change it must make:
/// `Some(true)` +1, `Some(false)` set to 0, `None` kept.
const TABLE: &[(Outcome, Option<bool>, Option<bool>)] = &[
    (Outcome::ConnectFailed, Some(true), None),
    (Outcome::AppRefused, Some(true), None),
    (Outcome::Reset, Some(true), None),
    (Outcome::InvalidResponse, Some(true), None),
    (Outcome::Head(500), Some(false), Some(true)),
    (Outcome::Head(503), Some(false), Some(true)),
    (Outcome::Head(599), Some(false), Some(true)),
    (Outcome::Head(200), Some(false), Some(false)),
    (Outcome::Head(204), Some(false), Some(false)),
    (Outcome::Head(404), Some(false), Some(false)),
    (Outcome::Head(499), Some(false), Some(false)),
    (Outcome::Head(100), None, None),
    (Outcome::Head(103), None, None),
    (Outcome::Connected, Some(false), Some(false)),
    (Outcome::Overloaded, None, None),
    (Outcome::QueueTimeout, None, None),
    (Outcome::RefusedStream, None, None),
    (Outcome::Timeout, None, None),
    (Outcome::AppLeft, None, None),
    (Outcome::CopyDeadline, None, None),
    (Outcome::AfterHead, None, None),
];

/// The counters of `a`, read through two probes that do not eject: thresholds are high.
fn counters(o: &Outlier, a: SocketAddr) -> (u32, u32) {
    o.counters(a).expect("a known endpoint")
}

#[test]
fn cbr_07_each_outcome_changes_the_counters_as_the_table_says() {
    // Thresholds 100 and 100 so that no outcome ejects here.
    let p = policy(100, 100, 30, 100);
    for start in [(0u32, 0u32), (3, 3)] {
        for &(outcome, failures, fives) in TABLE {
            let mut o = state(&p, 1);
            o.set_counters_for_test(addr(1), start.0, start.1);
            assert_eq!(o.record(addr(1), outcome, ms(1)), None, "{outcome:?}");
            let want = |change: Option<bool>, before: u32| match change {
                Some(true) => before + 1,
                Some(false) => 0,
                None => before,
            };
            assert_eq!(
                counters(&o, addr(1)),
                (want(failures, start.0), want(fives, start.1)),
                "{outcome:?} from {start:?}"
            );
        }
    }
}

#[test]
fn cbr_07_an_unknown_endpoint_records_nothing() {
    let p = policy(1, 1, 30, 100);
    let mut o = state(&p, 2);
    assert_eq!(o.record(addr(9), Outcome::Reset, ms(1)), None);
    assert_eq!(o.ejected_count(ms(1)), 0);
}

#[test]
fn cbr_08_consecutive_failures_eject_at_the_threshold() {
    let p = policy(5, 0, 30, 100);
    let mut o = state(&p, 3);
    let a = addr(1);
    assert_eq!(fail(&mut o, a, 4, ms(1)), None);
    o.record(a, Outcome::Head(200), ms(2));
    assert_eq!(fail(&mut o, a, 4, ms(3)), None);
    assert_eq!(counters(&o, a), (4, 0));
    let got = o.record(a, Outcome::ConnectFailed, ms(4));
    assert_eq!(
        got,
        Some(Ejection {
            reason: Reason::ConsecutiveFailures,
            seconds: 30,
            ejections: 1
        })
    );
    assert_eq!(
        counters(&o, a),
        (0, 0),
        "an ejection sets both counters to 0"
    );
    assert!(o.is_ejected(a, ms(5)));
    assert!(!o.is_ejected(addr(2), ms(5)));
}

#[test]
fn cbr_08_a_neutral_outcome_keeps_the_count_and_a_5xx_head_resets_it() {
    let p = policy(5, 0, 30, 100);
    let mut o = state(&p, 3);
    let a = addr(1);
    fail(&mut o, a, 4, ms(1));
    assert_eq!(o.record(a, Outcome::Overloaded, ms(2)), None);
    assert_eq!(o.record(a, Outcome::Timeout, ms(2)), None);
    assert!(o.record(a, Outcome::Reset, ms(3)).is_some(), "4 + 1");
    let mut o = state(&p, 3);
    fail(&mut o, a, 4, ms(1));
    assert_eq!(o.record(a, Outcome::Head(500), ms(2)), None);
    assert_eq!(counters(&o, a), (0, 1));
    assert_eq!(fail(&mut o, a, 4, ms(3)), None);
}

#[test]
fn cbr_08_consecutive_5xx_eject_at_the_threshold_and_reset_on_a_good_head() {
    let p = policy(5, 3, 30, 100);
    let a = addr(1);
    let mut o = state(&p, 3);
    o.record(a, Outcome::Head(500), ms(1));
    o.record(a, Outcome::Head(503), ms(1));
    let got = o.record(a, Outcome::Head(502), ms(1));
    assert_eq!(got.map(|e| e.reason), Some(Reason::Consecutive5xx));
    for good in [Outcome::Head(200), Outcome::Head(404)] {
        let mut o = state(&p, 3);
        o.record(a, Outcome::Head(500), ms(1));
        o.record(a, Outcome::Head(500), ms(1));
        o.record(a, good, ms(1));
        assert_eq!(o.record(a, Outcome::Head(500), ms(1)), None, "{good:?}");
    }
    // A proxy-level failure between them keeps the 5xx count.
    let mut o = state(&p, 3);
    o.record(a, Outcome::Head(500), ms(1));
    o.record(a, Outcome::Head(500), ms(1));
    o.record(a, Outcome::Reset, ms(1));
    assert_eq!(counters(&o, a), (1, 2));
    assert!(o.record(a, Outcome::Head(500), ms(1)).is_some());
}

#[test]
fn cbr_08_with_consecutive_5xx_0_no_number_of_5xx_ejects() {
    let p = policy(5, 0, 30, 100);
    let mut o = state(&p, 3);
    for _ in 0..1000 {
        assert_eq!(o.record(addr(1), Outcome::Head(500), ms(1)), None);
    }
    assert_eq!(o.ejected_count(ms(1)), 0);
}

/// Ejects `a` once at `now`, with `failures` 1.
fn eject(o: &mut Outlier, a: SocketAddr, now: Duration) -> Ejection {
    o.record(a, Outcome::Reset, now).expect("ejected")
}

#[test]
fn cbr_09_the_ejections_of_a_streak_double_up_to_300_s() {
    for (base, want) in [
        (30u32, vec![30u32, 60, 120, 240, 300, 300]),
        (300, vec![300, 300, 300]),
        (1, vec![1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300]),
    ] {
        let p = policy(1, 0, base, 100);
        let mut o = state(&p, 2);
        let mut now = secs(10);
        for (i, secs_want) in want.iter().enumerate() {
            let got = eject(&mut o, addr(1), now);
            assert_eq!(got.seconds, *secs_want, "base {base}, ejection {i}");
            assert_eq!(got.ejections, i as u32 + 1);
            // Restored exactly at its end; the next ejection comes 1 s later.
            now += secs(u64::from(*secs_want)) + secs(1);
        }
    }
}

#[test]
fn cbr_09_the_end_of_an_ejection_is_exact() {
    let p = policy(1, 0, 30, 100);
    let mut o = state(&p, 2);
    let t = ms(5_000);
    assert_eq!(eject(&mut o, addr(1), t).seconds, 30);
    assert!(o.is_ejected(addr(1), t + secs(30) - ms(1)));
    assert!(!o.is_ejected(addr(1), t + secs(30)));
    assert_eq!(counters(&o, addr(1)), (0, 0));
}

#[test]
fn cbr_09_the_streak_ends_300_s_after_the_restoration() {
    let p = policy(1, 0, 30, 100);
    let t = ms(1000);
    let r = t + secs(30);
    for (later, want) in [(ms(299_999), 60u32), (secs(300), 30)] {
        let mut o = state(&p, 2);
        assert_eq!(eject(&mut o, addr(1), t).seconds, 30);
        assert_eq!(eject(&mut o, addr(1), r + later).seconds, want, "{later:?}");
    }
}

#[test]
fn cbr_09_overflow_and_a_clock_that_goes_back_never_panic() {
    let p = policy(1, 0, 300, 100);
    let mut o = state(&p, 2);
    let mut now = secs(1);
    let mut last = 0;
    for _ in 0..100 {
        now += secs(300) + secs(1);
        last = eject(&mut o, addr(1), now).ejections;
    }
    assert_eq!(
        last, 100,
        "within a streak the count saturates, not overflows"
    );
    let top = ms(u64::MAX);
    let mut o = state(&p, 2);
    assert!(o.record(addr(1), Outcome::Reset, top).is_some());
    let _ = o.is_ejected(addr(1), top);
    let _ = o.reconcile(&p, &addrs(2), top);
    // The clock goes back: the ejection does not end early.
    let mut o = state(&p, 2);
    eject(&mut o, addr(1), secs(100));
    assert!(o.is_ejected(addr(1), secs(50)));
    assert!(o.record(addr(1), Outcome::Reset, secs(50)).is_none());
    assert!(o.reconcile(&p, &addrs(2), secs(50)).is_empty());
    assert!(o.is_ejected(addr(1), secs(50)));
}

#[test]
fn cbr_10_at_most_n_times_percent_over_100_are_ejected() {
    for n in 1u8..=10 {
        for percent in [0u32, 10, 33, 50, 99, 100] {
            let p = policy(1, 0, 30, percent);
            let mut o = state(&p, n);
            for i in 1..=n {
                let _ = o.record(addr(i), Outcome::Reset, ms(1));
            }
            let allowed = usize::from(n) * percent as usize / 100;
            assert_eq!(o.ejected_count(ms(2)), allowed, "n {n} at {percent}%");
        }
    }
    // The documented cases.
    for (n, percent, want) in [(1u8, 50u32, 0usize), (2, 50, 1), (3, 50, 1), (4, 50, 2)] {
        let p = policy(1, 0, 30, percent);
        let mut o = state(&p, n);
        for i in 1..=n {
            let _ = o.record(addr(i), Outcome::Reset, ms(1));
        }
        assert_eq!(o.ejected_count(ms(2)), want);
    }
}

#[test]
fn cbr_10_an_endpoint_without_room_keeps_counting_and_is_ejected_after_a_restoration() {
    let p = policy(2, 0, 30, 50);
    let mut o = state(&p, 2);
    assert_eq!(fail(&mut o, addr(1), 2, ms(1)).map(|e| e.seconds), Some(30));
    assert_eq!(fail(&mut o, addr(2), 5, ms(2)), None, "no room");
    assert_eq!(counters(&o, addr(2)), (5, 0));
    assert!(!o.is_ejected(addr(2), ms(3)));
    // After the first ejection ended, the next failure of the second ejects it.
    let after = ms(1) + secs(30);
    assert!(o.record(addr(2), Outcome::ConnectFailed, after).is_some());
}

#[test]
fn cbr_11_a_departed_endpoint_drops_its_state_and_a_returning_one_starts_afresh() {
    let p = policy(1, 0, 30, 100);
    let mut o = state(&p, 3);
    eject(&mut o, addr(1), ms(1));
    let gone = o.reconcile(&p, &[addr(2), addr(3)], ms(2));
    assert!(
        gone.is_empty(),
        "an endpoint that leaves reports no restoration"
    );
    assert_eq!(o.counters(addr(1)), None);
    assert_eq!(o.ejected_count(ms(2)), 0);
    assert!(o.reconcile(&p, &addrs(3), ms(3)).is_empty());
    assert!(!o.is_ejected(addr(1), ms(3)));
    assert_eq!(counters(&o, addr(1)), (0, 0));
    assert_eq!(eject(&mut o, addr(1), ms(4)).ejections, 1, "streak 0");
}

#[test]
fn cbr_11_a_shrinking_view_restores_the_earliest_ending_ejection_at_once() {
    let p = policy(1, 0, 30, 50);
    let mut o = state(&p, 4);
    eject(&mut o, addr(3), ms(10));
    eject(&mut o, addr(4), ms(20));
    assert_eq!(o.ejected_count(ms(30)), 2);
    // Two endpoints left, both ejected: allowed is 1, the one that ends first goes back.
    let got = o.reconcile(&p, &[addr(3), addr(4)], ms(30));
    assert_eq!(
        got,
        vec![Restoration {
            addr: addr(3),
            cause: Cause::MaxPercent
        }]
    );
    assert!(!o.is_ejected(addr(3), ms(30)));
    assert!(o.is_ejected(addr(4), ms(30)));
    assert_eq!(o.ejected_count(ms(30)), 1);
}

#[test]
fn cbr_11_equal_ends_restore_the_lower_address_first() {
    let p = policy(1, 0, 30, 100);
    let mut o = state(&p, 4);
    for i in [4u8, 3] {
        eject(&mut o, addr(i), ms(10));
    }
    let lower = policy(1, 0, 30, 50);
    let got = o.reconcile(&lower, &[addr(3), addr(4)], ms(20));
    assert_eq!(got.iter().map(|r| r.addr).collect::<Vec<_>>(), [addr(3)]);
}

#[test]
fn cbr_11_a_lower_percent_does_the_same_and_zero_restores_all() {
    let p = policy(1, 0, 30, 100);
    let mut o = state(&p, 4);
    for i in 1..=4 {
        eject(&mut o, addr(i), ms(u64::from(i)));
    }
    let got = o.reconcile(&policy(1, 0, 30, 50), &addrs(4), ms(10));
    assert_eq!(got.len(), 2);
    assert!(got.iter().all(|r| r.cause == Cause::MaxPercent));
    assert_eq!(o.ejected_count(ms(10)), 2);
    let got = o.reconcile(&policy(1, 0, 30, 0), &addrs(4), ms(11));
    assert_eq!(got.len(), 2);
    assert_eq!(o.ejected_count(ms(11)), 0);
}

#[test]
fn cbr_11_new_thresholds_apply_at_the_next_outcome_and_the_new_base_at_the_next_ejection() {
    let p = policy(10, 0, 30, 100);
    let mut o = state(&p, 2);
    fail(&mut o, addr(1), 4, ms(1));
    let lower = policy(3, 0, 60, 100);
    assert!(o.reconcile(&lower, &addrs(2), ms(2)).is_empty());
    assert!(!o.is_ejected(addr(1), ms(2)), "no ejection at the view");
    let got = o
        .record(addr(1), Outcome::Reset, ms(3))
        .expect("over the new threshold");
    assert_eq!(got.seconds, 60, "the new base applies to the next ejection");
    // An ejection that was made keeps its own time.
    let mut o = state(&p, 2);
    let first = fail(&mut o, addr(1), 10, ms(1)).expect("ejected");
    assert_eq!(first.seconds, 30);
    o.reconcile(&policy(10, 0, 300, 100), &addrs(2), ms(2));
    assert!(!o.is_ejected(addr(1), ms(1) + secs(30)));
}

#[test]
fn cbr_11_elapsed_ejections_are_reported_once_by_the_sweep() {
    let p = policy(1, 0, 30, 100);
    let mut o = state(&p, 2);
    eject(&mut o, addr(1), ms(1));
    assert!(o.reconcile(&p, &addrs(2), secs(10)).is_empty());
    let got = o.reconcile(&p, &addrs(2), secs(31));
    assert_eq!(
        got,
        vec![Restoration {
            addr: addr(1),
            cause: Cause::Elapsed
        }]
    );
    assert!(
        o.reconcile(&p, &addrs(2), secs(32)).is_empty(),
        "reported once"
    );
    // Re-ejected before the sweep: the restoration is still reported.
    let mut o = state(&p, 2);
    eject(&mut o, addr(1), ms(1));
    eject(&mut o, addr(1), secs(31));
    let got = o.reconcile(&p, &addrs(2), secs(32));
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].cause, Cause::Elapsed);
}

#[derive(Debug, Clone)]
enum Step {
    Outcome(u8, u8, u64),
    View(u8, u32, u32, u32, u32),
    Tick(u64),
}

fn outcome_of(k: u8) -> Outcome {
    match k % 12 {
        0 => Outcome::ConnectFailed,
        1 => Outcome::AppRefused,
        2 => Outcome::Reset,
        3 => Outcome::InvalidResponse,
        4 => Outcome::Head(500),
        5 => Outcome::Head(200),
        6 => Outcome::Head(404),
        7 => Outcome::Overloaded,
        8 => Outcome::Timeout,
        9 => Outcome::Connected,
        10 => Outcome::Head(100),
        _ => Outcome::AppLeft,
    }
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => (1u8..=16, any::<u8>(), 0u64..2000).prop_map(|(a, k, d)| Step::Outcome(a, k, d)),
        1 => (0u8..=16, 1u32..=4, 0u32..=3, 1u32..=300, 0u32..=100)
            .prop_map(|(n, f, c, b, p)| Step::View(n, f, c, b, p)),
        1 => (0u64..400_000).prop_map(Step::Tick),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn cbr_12_the_bound_the_thresholds_and_the_times_hold_over_any_sequence(
        f in 1u32..=4, c in 0u32..=3, base in 1u32..=300, percent in 0u32..=100,
        n in 1u8..=16,
        steps in proptest::collection::vec(step(), 1..120),
    ) {
        let run = |steps: &[Step]| {
            let mut p = policy(f, c, base, percent);
            let mut o = state(&p, n);
            let mut now = Duration::ZERO;
            let mut members = addrs(n);
            let mut log = Vec::new();
            // Counters the table predicts, since the last ejection, restoration or arrival.
            let mut model: std::collections::BTreeMap<SocketAddr, (u32, u32)> =
                members.iter().map(|a| (*a, (0, 0))).collect();
            for s in steps {
                match s {
                    Step::Tick(d) => now += ms(*d),
                    Step::Outcome(a, k, d) => {
                        now += ms(*d);
                        let a = addr(*a);
                        let outcome = outcome_of(*k);
                        let was = o.is_ejected(a, now);
                        let got = o.record(a, outcome, now);
                        log.push(format!("{a} {outcome:?} {now:?} {got:?}"));
                        if let Some(m) = model.get_mut(&a) && !was {
                            let (mf, mv) = *m;
                            let (nf, nv) = match outcome {
                                Outcome::ConnectFailed | Outcome::AppRefused | Outcome::Reset
                                | Outcome::InvalidResponse => (mf + 1, mv),
                                Outcome::Head(s) if (500..=599).contains(&s) => (0, mv + 1),
                                Outcome::Head(s) if s >= 200 => (0, 0),
                                Outcome::Connected => (0, 0),
                                _ => (mf, mv),
                            };
                            *m = (nf, nv);
                            if let Some(e) = got {
                                let hit = match e.reason {
                                    Reason::ConsecutiveFailures => nf >= p.consecutive_failures,
                                    Reason::Consecutive5xx => {
                                        p.consecutive_5xx != 0 && nv >= p.consecutive_5xx
                                    }
                                };
                                prop_assert!(hit, "ejected below its threshold");
                                prop_assert!(
                                    (p.base_ejection_seconds..=300).contains(&e.seconds)
                                        || e.seconds == 300,
                                    "ejection time {}", e.seconds
                                );
                                *m = (0, 0);
                            }
                        } else if got.is_some() {
                            prop_assert!(false, "an unknown or ejected endpoint was ejected");
                        }
                    }
                    Step::View(count, nf, nc, nb, np) => {
                        p = policy(*nf, *nc, *nb, *np);
                        members = addrs(*count);
                        let restored = o.reconcile(&p, &members, now);
                        log.push(format!("view {count} {restored:?}"));
                        model.retain(|a, _| members.contains(a));
                        for a in &members { model.entry(*a).or_insert((0, 0)); }
                        // A restoration changes no counter: they were zeroed by the ejection, and
                        // the failures counted since (a restoration is reported late) stay.
                        drop(restored);
                    }
                }
                let allowed = members.len() * p.max_ejection_percent as usize / 100;
                prop_assert!(
                    o.ejected_count(now) <= allowed,
                    "{} ejected, {} allowed", o.ejected_count(now), allowed
                );
            }
            Ok(log)
        };
        let first = run(&steps)?;
        let second = run(&steps)?;
        prop_assert_eq!(first, second, "the same sequence gives the same reports");
    }
}

#[test]
fn cbr_08_an_outcome_of_an_endpoint_ejected_at_the_time_counts_nothing() {
    // A try in flight when its endpoint got ejected ends with an outcome: it is ignored, so
    // the counters stay at 0 and the ejection is neither extended nor counted twice.
    let p = policy(2, 0, 30, 100);
    let mut o = state(&p, 2);
    assert!(fail(&mut o, addr(1), 2, ms(1)).is_some());
    assert_eq!(o.record(addr(1), Outcome::Reset, ms(2)), None);
    assert_eq!(o.record(addr(1), Outcome::Head(200), ms(3)), None);
    assert_eq!(counters(&o, addr(1)), (0, 0));
    assert!(o.is_ejected(addr(1), secs(30)));
    assert!(!o.is_ejected(addr(1), ms(30_001)));
    // The same for an endpoint the view does not hold.
    assert_eq!(o.record(addr(9), Outcome::Reset, ms(4)), None);
    assert_eq!(o.counters(addr(9)), None);
}
