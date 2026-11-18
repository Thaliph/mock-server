//! ADM-01 to ADM-07: the admission state machine of `mesh::admission`.

use std::collections::BTreeMap;
use std::time::Duration;

use proptest::prelude::*;

use super::admission::{
    Admission, Contract, Counts, MAX_CONCURRENCY, MAX_RPS, Offer, Permit, QUEUE_WAIT, Refusal,
    Resolved, Ticket, Verdict,
};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn ns(n: u64) -> Duration {
    Duration::from_nanos(n)
}

fn contract(concurrency: u32, rps: u32) -> Option<Contract> {
    Some(Contract { concurrency, rps })
}

/// A state with the contract `c/r` set at `now`.
fn state(c: u32, r: u32, now: Duration) -> Admission {
    let mut a = Admission::new();
    let mut out = Vec::new();
    a.set_contract(contract(c, r), now, &mut out);
    assert!(out.is_empty());
    a
}

fn admitted(offer: Offer) -> Permit {
    match offer {
        Offer::Admitted(p) => p,
        other => panic!("expected an admission, got {other:?}"),
    }
}

fn queued(offer: Offer) -> Ticket {
    match offer {
        Offer::Queued(t) => t,
        other => panic!("expected a queued offer, got {other:?}"),
    }
}

/// `n` offers at `now`, each admitted at once.
fn admit_n(a: &mut Admission, n: usize, now: Duration) -> Vec<Permit> {
    let mut out = Vec::new();
    let permits = (0..n).map(|_| admitted(a.offer(now, &mut out))).collect();
    assert!(out.is_empty());
    permits
}

/// What `out` holds: each ticket with its refusal (`None`: admitted).
fn summary(out: &[Resolved]) -> Vec<(Ticket, Option<Refusal>)> {
    out.iter()
        .map(|r| match r.verdict {
            Verdict::Admitted(_) => (r.ticket, None),
            Verdict::Refused(refusal) => (r.ticket, Some(refusal)),
        })
        .collect()
}

/// Calls `f` on a cleared `out` and returns the summary of what it resolved.
fn resolved(
    out: &mut Vec<Resolved>,
    f: impl FnOnce(&mut Vec<Resolved>),
) -> Vec<(Ticket, Option<Refusal>)> {
    out.clear();
    f(out);
    summary(out)
}

/// The permits of the entries `out` admitted.
fn permits(out: &mut Vec<Resolved>) -> Vec<Permit> {
    out.drain(..)
        .filter_map(|r| match r.verdict {
            Verdict::Admitted(p) => Some(p),
            Verdict::Refused(_) => None,
        })
        .collect()
}

#[test]
fn adm_01_two_admitted_a_third_queued_and_a_release_admits_it_at_once() {
    let mut a = state(2, 1000, ms(0));
    let mut out = Vec::new();
    let p1 = admitted(a.offer(ms(0), &mut out));
    let _p2 = admitted(a.offer(ms(0), &mut out));
    let t = queued(a.offer(ms(0), &mut out));
    assert!(out.is_empty());
    assert_eq!(a.counts(ms(0)).in_flight, 2);
    assert_eq!(resolved(&mut out, |o| a.release(p1, ms(0), o)), [(t, None)]);
    assert_eq!(a.counts(ms(0)).in_flight, 2);
    assert_eq!(a.counts(ms(0)).depth, 0);
}

#[test]
fn adm_01_the_token_limit_alone_queues_below_the_concurrency() {
    let mut a = state(1000, 10, ms(0));
    let mut out = Vec::new();
    let _p = admitted(a.offer(ms(0), &mut out));
    let t = queued(a.offer(ms(0), &mut out));
    assert_eq!(a.counts(ms(0)).in_flight, 1);
    assert!(resolved(&mut out, |o| a.apply(ms(100) - ns(1), o)).is_empty());
    assert_eq!(resolved(&mut out, |o| a.apply(ms(100), o)), [(t, None)]);
    assert_eq!(a.counts(ms(100)).in_flight, 2);
}

#[test]
fn adm_02_burst_and_queue_size_round_up_with_a_floor_of_one() {
    for (rps, burst) in [(1, 1), (9, 1), (10, 1), (11, 2), (1_000_000, 100_000)] {
        let c = Contract {
            concurrency: 1,
            rps,
        };
        assert_eq!(c.burst(), burst, "rps {rps}");
    }
    for (concurrency, size) in [(1, 1), (9, 1), (10, 1), (11, 2), (100_000, 10_000)] {
        let c = Contract {
            concurrency,
            rps: 1,
        };
        assert_eq!(c.queue_size(), size, "concurrency {concurrency}");
    }
    let bounds = |concurrency, rps| Contract { concurrency, rps }.in_bounds();
    assert!(bounds(1, 1));
    assert!(bounds(MAX_CONCURRENCY, MAX_RPS));
    assert!(!bounds(0, 1));
    assert!(!bounds(1, 0));
    assert!(!bounds(MAX_CONCURRENCY + 1, 1));
    assert!(!bounds(1, MAX_RPS + 1));
}

#[test]
fn adm_02_a_new_contract_starts_with_a_full_bucket() {
    let mut a = state(1000, 100, ms(0));
    let _held = admit_n(&mut a, 10, ms(0));
    let mut out = Vec::new();
    let _t = queued(a.offer(ms(0), &mut out));
}

#[test]
fn adm_02_the_next_token_is_at_ceil_integer_nanoseconds() {
    let mut a = state(1000, 3, ms(0));
    let mut out = Vec::new();
    let _p = admitted(a.offer(ms(0), &mut out));
    let mut b = a.clone();
    let t = queued(a.offer(ns(333_333_333), &mut out));
    assert_eq!(a.next_instant(), Some(ns(333_333_334)));
    assert_eq!(
        resolved(&mut out, |o| a.apply(ns(333_333_334), o)),
        [(t, None)]
    );
    let _p = admitted(b.offer(ns(333_333_334), &mut out));
    assert!(out.is_empty());
}

#[test]
fn adm_02_the_extremes_do_not_overflow() {
    let mut a = state(MAX_CONCURRENCY, MAX_RPS, ms(0));
    let t = secs(315_576_000);
    let mut held = admit_n(&mut a, 100_000, t);
    let mut out = Vec::new();
    let _q = queued(a.offer(t, &mut out));
    assert_eq!(a.counts(t + secs(1)).in_flight_ns, 100_000 * 1_000_000_000);
    assert_eq!(a.next_instant(), Some(t + Duration::from_micros(1)));
    let p = held.pop().expect("a permit");
    a.release(p, t, &mut out);
    a.apply(t, &mut out);
    // The clock at its very end: saturating, not overflowing.
    a.apply(Duration::MAX, &mut out);
    let _ = a.offer(Duration::MAX, &mut out);
    let _ = a.next_instant();
    let _ = a.counts(Duration::MAX);
}

#[test]
fn adm_03_a_full_queue_refuses_overloaded_and_stays_unchanged() {
    let mut a = state(1, 1000, ms(0));
    let mut out = Vec::new();
    let pa = admitted(a.offer(ms(0), &mut out));
    let b = queued(a.offer(ms(0), &mut out));
    assert_eq!(
        a.offer(ms(0), &mut out),
        Offer::Refused(Refusal::Overloaded)
    );
    assert!(out.is_empty());
    let c = a.counts(ms(0));
    assert_eq!((c.depth, c.queued, c.overloaded), (1, 1, 1));
    assert_eq!(resolved(&mut out, |o| a.release(pa, ms(0), o)), [(b, None)]);
    assert_eq!(Refusal::Overloaded.as_str(), "overloaded");
    assert_eq!(Refusal::QueueTimeout.as_str(), "queue-timeout");
}

#[test]
fn adm_03_a_queued_request_times_out_at_exactly_150_ms() {
    let mut a = state(1, 1000, ms(0));
    let mut out = Vec::new();
    let _held = admitted(a.offer(ms(0), &mut out));
    let b = queued(a.offer(ms(7), &mut out));
    assert!(resolved(&mut out, |o| a.apply(ms(157) - ns(1), o)).is_empty());
    assert_eq!(
        resolved(&mut out, |o| a.apply(ms(157), o)),
        [(b, Some(Refusal::QueueTimeout))]
    );
    assert_eq!(a.counts(ms(157)).queue_timeout, 1);
    assert_eq!(a.counts(ms(157)).depth, 0);
    assert_eq!(QUEUE_WAIT, ms(150));
}

#[test]
fn adm_03_next_instant_is_the_next_token_or_the_deadline_and_none_when_empty() {
    let mut out = Vec::new();
    let mut a = Admission::new();
    let _p = admitted(a.offer(ms(0), &mut out));
    assert_eq!(a.next_instant(), None, "no contract");
    let mut a = state(1, 1000, ms(0));
    assert_eq!(a.next_instant(), None, "an empty queue");
    let _held = admitted(a.offer(ms(0), &mut out));
    assert_eq!(a.next_instant(), None, "an empty queue, a slot held");
    let _t = queued(a.offer(ms(0), &mut out));
    assert_eq!(
        a.next_instant(),
        Some(ms(150)),
        "the head waits for a slot only"
    );
    let mut a = state(1000, 10, ms(0));
    let _p = admitted(a.offer(ms(0), &mut out));
    let _t = queued(a.offer(ms(0), &mut out));
    assert_eq!(
        a.next_instant(),
        Some(ms(100)),
        "the next token comes first"
    );
    let mut a = state(1000, 1, ms(0));
    let _p = admitted(a.offer(ms(0), &mut out));
    let _t = queued(a.offer(ms(0), &mut out));
    assert_eq!(a.next_instant(), Some(ms(150)), "the deadline comes first");
}

#[test]
fn adm_03_a_cancelled_entry_frees_its_place_and_is_never_admitted() {
    let mut a = state(1, 1000, ms(0));
    let mut out = Vec::new();
    let pa = admitted(a.offer(ms(0), &mut out));
    let b = queued(a.offer(ms(0), &mut out));
    assert!(a.cancel(b));
    assert!(!a.cancel(b));
    let c = queued(a.offer(ms(0), &mut out));
    assert_ne!(b, c);
    assert_eq!(resolved(&mut out, |o| a.release(pa, ms(0), o)), [(c, None)]);
    assert!(resolved(&mut out, |o| a.apply(secs(1), o)).is_empty());
    let n = a.counts(secs(1));
    assert_eq!(
        (n.cancelled, n.overloaded, n.queue_timeout, n.depth),
        (1, 0, 0, 0)
    );
    assert!(!a.cancel(c), "an admitted ticket is no longer queued");
    assert_eq!(a.counts(secs(1)).cancelled, 1);
}

#[test]
fn adm_04_queued_requests_are_admitted_in_offer_order_under_either_limit() {
    // The slot limit.
    let mut a = state(11, 1000, ms(0));
    let mut held = admit_n(&mut a, 11, ms(0));
    let mut out = Vec::new();
    let b = queued(a.offer(ms(0), &mut out));
    let c = queued(a.offer(ms(1), &mut out));
    let p = held.pop().expect("a permit");
    assert_eq!(resolved(&mut out, |o| a.release(p, ms(2), o)), [(b, None)]);
    let p = held.pop().expect("a permit");
    assert_eq!(resolved(&mut out, |o| a.release(p, ms(3), o)), [(c, None)]);
    // The token limit.
    let mut a = state(20, 10, ms(0));
    let _pa = admitted(a.offer(ms(0), &mut out));
    let b = queued(a.offer(ms(0), &mut out));
    let c = queued(a.offer(ms(1), &mut out));
    assert_eq!(resolved(&mut out, |o| a.apply(ms(100), o)), [(b, None)]);
    assert_eq!(
        resolved(&mut out, |o| a.apply(ms(151), o)),
        [(c, Some(Refusal::QueueTimeout))]
    );
}

#[test]
fn adm_04_an_offer_applies_the_queue_first_and_never_overtakes() {
    let mut a = state(1000, 10, ms(0));
    let mut out = Vec::new();
    let _pa = admitted(a.offer(ms(0), &mut out));
    let b = queued(a.offer(ms(0), &mut out));
    out.clear();
    let c = queued(a.offer(ms(100), &mut out));
    assert_eq!(summary(&out), [(b, None)]);
    assert_ne!(b, c);
}

#[test]
fn adm_05_an_instant_that_goes_back_admits_nothing_early_and_frees_no_token() {
    let mut a = state(1000, 10, ms(0));
    let mut out = Vec::new();
    let _p = admitted(a.offer(secs(1), &mut out));
    let t = queued(a.offer(ms(500), &mut out));
    assert_eq!(a.next_instant(), Some(ms(1100)));
    assert!(resolved(&mut out, |o| a.apply(ms(900), o)).is_empty());
    assert!(resolved(&mut out, |o| a.apply(ms(1100) - ns(1), o)).is_empty());
    let early = a.counts(ms(200));
    assert_eq!(early, a.counts(ms(1100) - ns(1)));
    assert_eq!(resolved(&mut out, |o| a.apply(ms(1100), o)), [(t, None)]);
    assert_eq!(a.counts(ms(200)), a.counts(ms(1100)));
}

#[test]
fn adm_05_a_contract_out_of_range_is_clamped_and_never_panics() {
    let mut a = state(0, 0, ms(0));
    assert_eq!(a.contract(), contract(1, 1));
    let mut out = Vec::new();
    let _p = admitted(a.offer(ms(0), &mut out));
    let _t = queued(a.offer(ms(0), &mut out));
    assert_eq!(a.next_instant(), Some(ms(150)));
    let a = state(u32::MAX, u32::MAX, ms(0));
    assert_eq!(a.contract(), contract(MAX_CONCURRENCY, MAX_RPS));
}

/// One step of the ADM-05 sequence.
#[derive(Debug, Clone)]
enum Op {
    Offer,
    Release(usize),
    Cancel(usize),
    Contract(Option<(u32, u32)>),
    Apply,
}

fn op() -> impl Strategy<Value = Op> {
    let c = prop_oneof![1u32..=12, Just(MAX_CONCURRENCY), 1u32..=MAX_CONCURRENCY];
    let r = prop_oneof![1u32..=12, Just(1000u32), Just(MAX_RPS), 1u32..=MAX_RPS];
    prop_oneof![
        8 => Just(Op::Offer),
        3 => any::<usize>().prop_map(Op::Release),
        1 => any::<usize>().prop_map(Op::Cancel),
        1 => proptest::option::weighted(0.8, (c, r)).prop_map(Op::Contract),
        1 => Just(Op::Apply),
    ]
}

/// The step of the clock, in nanoseconds: back by up to 50 ms, forward by up to 200 ms, 0 and
/// the exact token periods included.
fn dt() -> impl Strategy<Value = i64> {
    prop_oneof![
        2 => Just(0i64),
        4 => -50_000_000i64..=200_000_000,
        2 => (1i64..=12).prop_map(|r| (1_000_000_000 + r - 1) / r),
        1 => Just(1_000_000i64),
        1 => Just(1_000i64),
        1 => Just(150_000_000i64),
        1 => Just(149_999_999i64),
    ]
}

/// The ADM-05 harness: the state, what it should hold, and the instants it gave.
struct Harness {
    a: Admission,
    out: Vec<Resolved>,
    /// The effective instant: the latest one seen.
    last: Duration,
    /// The presence segment, bumped on each `None ↔ Some`.
    presence: u64,
    /// Live permits, with the presence segment they were admitted in.
    live: Vec<(u64, Permit)>,
    /// The admission instants since the contract took its current value.
    admissions: Vec<Duration>,
    /// The queued tickets, with their offer instant, and every ticket ever given.
    pending: BTreeMap<Ticket, Duration>,
    tickets: Vec<Ticket>,
    offers: u64,
}

impl Harness {
    /// Records what one call (at the effective instant `self.last`) resolved.
    /// `admits_in_segment` is false for the entries a removal admitted.
    fn absorb(&mut self, admits_in_segment: bool) -> Result<(), TestCaseError> {
        let mut admitted_here = 0usize;
        for r in std::mem::take(&mut self.out) {
            let Some(offered) = self.pending.remove(&r.ticket) else {
                return Err(TestCaseError::fail(format!(
                    "{:?} resolved but not queued",
                    r.ticket
                )));
            };
            let waited = self.last.saturating_sub(offered);
            match r.verdict {
                Verdict::Admitted(p) => {
                    prop_assert!(waited < QUEUE_WAIT, "admitted after {waited:?}");
                    self.live.push((self.presence, p));
                    if admits_in_segment {
                        admitted_here += 1;
                    }
                }
                Verdict::Refused(refusal) => {
                    prop_assert_eq!(refusal, Refusal::QueueTimeout);
                    prop_assert_eq!(waited, QUEUE_WAIT, "timed out after {:?}", waited);
                }
            }
        }
        self.admitted(admitted_here)
    }

    /// `n` admissions at `self.last` under the contract in force, if any.
    fn admitted(&mut self, n: usize) -> Result<(), TestCaseError> {
        let Some(c) = self.a.contract() else {
            return Ok(());
        };
        for _ in 0..n {
            self.admissions.push(self.last);
            let j = self.admissions.len() - 1;
            let tj = self.admissions[j];
            for (i, ti) in self.admissions.iter().enumerate() {
                let span = (tj - *ti).as_nanos();
                let allowed = u128::from(c.burst()) + u128::from(c.rps) * span / 1_000_000_000;
                prop_assert!(
                    (j - i + 1) as u128 <= allowed,
                    "{} admissions in {:?} under {:?}",
                    j - i + 1,
                    tj - *ti,
                    c
                );
            }
        }
        if n > 0 {
            let live = self
                .live
                .iter()
                .filter(|(s, _)| *s == self.presence)
                .count();
            prop_assert!(
                live <= c.concurrency as usize,
                "{live} in flight under {c:?}"
            );
        }
        Ok(())
    }

    fn check(&self) -> Result<(), TestCaseError> {
        let n = self.a.counts(self.last);
        if self.a.contract().is_some() {
            let live = self
                .live
                .iter()
                .filter(|(s, _)| *s == self.presence)
                .count();
            prop_assert_eq!(n.in_flight as usize, live);
        } else {
            prop_assert_eq!(n.in_flight, 0);
        }
        prop_assert_eq!(n.depth as usize, self.pending.len());
        prop_assert_eq!(n.admitted_at_once + n.overloaded + n.queued, self.offers);
        prop_assert_eq!(
            n.queued,
            n.admitted_from_queue + n.queue_timeout + n.cancelled + u64::from(n.depth)
        );
        if let Some(next) = self.a.next_instant() {
            prop_assert!(next > self.last, "{next:?} is not after {:?}", self.last);
        }
        Ok(())
    }

    /// Applies the state at each instant it gives up to `t`, as the sidecar's timer does.
    fn drive(&mut self, t: Duration) -> Result<(), TestCaseError> {
        let mut rounds = 0;
        while let Some(next) = self.a.next_instant()
            && next <= t
        {
            rounds += 1;
            prop_assert!(rounds <= 10_000, "the timer does not progress");
            self.last = self.last.max(next);
            self.a.apply(next, &mut self.out);
            self.absorb(true)?;
            self.check()?;
        }
        Ok(())
    }

    fn step(&mut self, op: &Op, t: Duration) -> Result<(), TestCaseError> {
        self.drive(t)?;
        self.last = self.last.max(t);
        match op {
            Op::Offer => {
                self.offers += 1;
                let offer = self.a.offer(t, &mut self.out);
                self.absorb(true)?;
                let n = self.a.counts(self.last);
                let size = self.a.contract().map(|c| c.queue_size());
                match offer {
                    Offer::Admitted(p) => {
                        self.live.push((self.presence, p));
                        self.admitted(1)?;
                    }
                    Offer::Queued(ticket) => {
                        let size = size.expect("queued under a contract");
                        prop_assert!(n.depth <= size, "depth {} over {size}", n.depth);
                        self.pending.insert(ticket, self.last);
                        self.tickets.push(ticket);
                    }
                    Offer::Refused(refusal) => {
                        prop_assert_eq!(refusal, Refusal::Overloaded);
                        let size = size.expect("refused under a contract");
                        prop_assert!(n.depth >= size, "overloaded at depth {}", n.depth);
                    }
                }
            }
            Op::Release(i) => {
                if !self.live.is_empty() {
                    let (_, p) = self.live.remove(i % self.live.len());
                    self.a.release(p, t, &mut self.out);
                } else {
                    self.a.apply(t, &mut self.out);
                }
                self.absorb(true)?;
            }
            Op::Cancel(i) => {
                if !self.tickets.is_empty() {
                    let ticket = self.tickets[i % self.tickets.len()];
                    let was = self.pending.remove(&ticket).is_some();
                    prop_assert_eq!(self.a.cancel(ticket), was);
                }
            }
            Op::Contract(c) => {
                let new = c.map(|(concurrency, rps)| Contract { concurrency, rps });
                let old = self.a.contract();
                self.a.set_contract(new, t, &mut self.out);
                if old.is_some() != new.is_some() {
                    self.presence += 1;
                }
                if old != new {
                    self.admissions.clear();
                }
                // Its admissions count in the new segment; those of a removal in none.
                self.absorb(new.is_some())?;
            }
            Op::Apply => {
                self.a.apply(t, &mut self.out);
                self.absorb(true)?;
            }
        }
        self.check()
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn adm_05_any_sequence_keeps_the_limits_the_queue_and_the_deadlines(
        steps in proptest::collection::vec((op(), dt()), 1..=200),
    ) {
        let mut h = Harness {
            a: Admission::new(),
            out: Vec::new(),
            last: Duration::ZERO,
            presence: 0,
            live: Vec::new(),
            admissions: Vec::new(),
            pending: BTreeMap::new(),
            tickets: Vec::new(),
            offers: 0,
        };
        let mut t: i64 = 0;
        for (op, d) in &steps {
            t = t.saturating_add(*d).max(0);
            h.step(op, ns(t as u64))?;
        }
        h.drive(Duration::MAX)?;
        prop_assert_eq!(h.a.counts(h.last).depth, 0);
        prop_assert!(h.pending.is_empty(), "still queued: {:?}", h.pending);
        prop_assert_eq!(h.a.next_instant(), None);
    }
}

#[test]
fn adm_06_a_lower_concurrency_cuts_nothing() {
    let mut a = state(4, 1000, ms(0));
    let mut held = admit_n(&mut a, 4, ms(0));
    let mut out = Vec::new();
    assert!(resolved(&mut out, |o| a.set_contract(contract(2, 1000), ms(0), o)).is_empty());
    assert_eq!(a.counts(ms(0)).in_flight, 4);
    let b = queued(a.offer(ms(0), &mut out));
    for _ in 0..2 {
        let p = held.pop().expect("a permit");
        assert!(resolved(&mut out, |o| a.release(p, ms(1), o)).is_empty());
    }
    let p = held.pop().expect("a permit");
    assert_eq!(resolved(&mut out, |o| a.release(p, ms(1), o)), [(b, None)]);
    assert_eq!(a.counts(ms(1)).in_flight, 2);
}

#[test]
fn adm_06_a_higher_concurrency_admits_the_queued_in_order() {
    let mut a = state(11, 1000, ms(0));
    let mut held = admit_n(&mut a, 11, ms(0));
    let mut out = Vec::new();
    let b = queued(a.offer(ms(0), &mut out));
    let c = queued(a.offer(ms(0), &mut out));
    assert!(resolved(&mut out, |o| a.set_contract(contract(1, 1000), ms(0), o)).is_empty());
    assert_eq!(a.counts(ms(0)).depth, 2);
    for _ in 0..10 {
        let p = held.pop().expect("a permit");
        assert!(resolved(&mut out, |o| a.release(p, ms(1), o)).is_empty());
    }
    assert_eq!(a.counts(ms(1)).in_flight, 1);
    assert_eq!(
        resolved(&mut out, |o| a.set_contract(contract(3, 1000), ms(1), o)),
        [(b, None), (c, None)]
    );
    assert_eq!(a.counts(ms(1)).in_flight, 3);
}

#[test]
fn adm_06_a_new_rps_keeps_the_tokens_capped_at_the_new_burst() {
    let mut out = Vec::new();
    let mut a = state(1000, 1000, ms(0));
    let _taken = admit_n(&mut a, 30, ms(0));
    a.set_contract(contract(1000, 200), ms(0), &mut out);
    let _capped = admit_n(&mut a, 20, ms(0));
    let _t = queued(a.offer(ms(0), &mut out));
    let mut a = state(1000, 200, ms(0));
    let _taken = admit_n(&mut a, 15, ms(0));
    a.set_contract(contract(1000, 1000), ms(0), &mut out);
    let _kept = admit_n(&mut a, 5, ms(0));
    let _t = queued(a.offer(ms(0), &mut out));
    assert!(out.is_empty());
}

#[test]
fn adm_06_a_smaller_queue_keeps_its_entries() {
    let mut a = state(30, 1000, ms(0));
    let _held = admit_n(&mut a, 30, ms(0));
    let mut out = Vec::new();
    let tickets: Vec<Ticket> = (0..3).map(|_| queued(a.offer(ms(0), &mut out))).collect();
    assert!(resolved(&mut out, |o| a.set_contract(contract(10, 1000), ms(0), o)).is_empty());
    assert_eq!(a.counts(ms(0)).depth, 3);
    assert_eq!(
        a.offer(ms(0), &mut out),
        Offer::Refused(Refusal::Overloaded)
    );
    assert!(a.cancel(tickets[0]));
    assert!(a.cancel(tickets[1]));
    assert_eq!(a.counts(ms(0)).depth, 1);
    assert_eq!(
        a.offer(ms(0), &mut out),
        Offer::Refused(Refusal::Overloaded)
    );
    assert!(a.cancel(tickets[2]));
    let _t = queued(a.offer(ms(0), &mut out));
    assert!(out.is_empty());
}

#[test]
fn adm_06_removing_the_contract_admits_every_queued_entry_and_old_releases_are_harmless() {
    let mut a = state(1, 1000, ms(0));
    let mut out = Vec::new();
    let pa = admitted(a.offer(ms(0), &mut out));
    let b = queued(a.offer(ms(0), &mut out));
    assert_eq!(
        resolved(&mut out, |o| a.set_contract(None, ms(10), o)),
        [(b, None)]
    );
    assert_eq!(a.counts(ms(10)).in_flight, 0);
    let pb = permits(&mut out).pop().expect("a permit");
    a.release(pa, ms(20), &mut out);
    a.release(pb, ms(20), &mut out);
    assert_eq!(a.counts(ms(20)).in_flight, 0);
    let _p = admitted(a.offer(ms(20), &mut out));
    assert!(out.is_empty());
    a.set_contract(contract(1, 1000), ms(30), &mut out);
    let _p = admitted(a.offer(ms(30), &mut out));
    let _t = queued(a.offer(ms(30), &mut out));
}

#[test]
fn adm_06_a_contract_set_while_requests_are_in_flight_starts_at_zero() {
    let mut a = Admission::new();
    let mut held = admit_n(&mut a, 3, ms(0));
    assert_eq!(a.counts(ms(0)).in_flight, 0);
    let mut out = Vec::new();
    a.set_contract(contract(1, 1000), ms(0), &mut out);
    let p4 = admitted(a.offer(ms(0), &mut out));
    assert_eq!(a.counts(ms(0)).in_flight, 1);
    let p1 = held.remove(0);
    a.release(p1, ms(0), &mut out);
    assert_eq!(a.counts(ms(0)).in_flight, 1);
    let t = queued(a.offer(ms(0), &mut out));
    assert_eq!(resolved(&mut out, |o| a.release(p4, ms(0), o)), [(t, None)]);
}

#[test]
fn adm_06_the_same_contract_again_changes_nothing() {
    let mut a = state(1000, 10, ms(0));
    let mut out = Vec::new();
    let _p = admitted(a.offer(ms(0), &mut out));
    a.set_contract(contract(1000, 10), ms(50), &mut out);
    let _t = queued(a.offer(ms(50), &mut out));
    assert_eq!(a.next_instant(), Some(ms(100)));
    assert!(out.is_empty());
}

#[test]
fn adm_07_counts_of_a_scripted_sequence_are_exact() {
    let mut a = state(2, 1000, ms(0));
    let mut out = Vec::new();
    let pa = admitted(a.offer(ms(0), &mut out));
    let pb = admitted(a.offer(ms(0), &mut out));
    let c = queued(a.offer(ms(0), &mut out));
    assert_eq!(
        a.offer(ms(0), &mut out),
        Offer::Refused(Refusal::Overloaded)
    );
    assert_eq!(
        resolved(&mut out, |o| a.release(pa, ms(10), o)),
        [(c, None)]
    );
    let pc = permits(&mut out).pop().expect("a permit");
    let e = queued(a.offer(ms(10), &mut out));
    assert_eq!(
        resolved(&mut out, |o| a.apply(ms(160), o)),
        [(e, Some(Refusal::QueueTimeout))]
    );
    let at_160 = Counts {
        in_flight: 2,
        depth: 0,
        admitted_at_once: 2,
        queued: 2,
        admitted_from_queue: 1,
        overloaded: 1,
        queue_timeout: 1,
        cancelled: 0,
        in_flight_ns: 320_000_000,
    };
    assert_eq!(a.counts(ms(160)), at_160);
    assert_eq!(a.counts(ms(160)), at_160, "read twice");
    let f = queued(a.offer(ms(170), &mut out));
    assert!(a.cancel(f));
    a.release(pb, ms(200), &mut out);
    a.release(pc, ms(200), &mut out);
    assert!(out.is_empty());
    let at_300 = Counts {
        in_flight: 0,
        queued: 3,
        cancelled: 1,
        in_flight_ns: 400_000_000,
        ..at_160
    };
    assert_eq!(a.counts(ms(300)), at_300);
    assert_eq!(a.counts(ms(300)), at_300, "read twice");
    let _ = a.counts(ms(500));
    assert_eq!(a.counts(ms(300)), at_300, "a later read stores nothing");
    // The state still stands at 200 ms: an offer at 300 ms is judged at 300 ms, and the
    // integral from there is that of one request in flight.
    let _p = admitted(a.offer(ms(300), &mut out));
    assert_eq!(a.counts(ms(300)).in_flight_ns, 400_000_000);
    assert_eq!(a.counts(ms(301)).in_flight_ns, 401_000_000);
}
