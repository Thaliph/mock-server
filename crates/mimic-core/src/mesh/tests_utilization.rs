//! ADM-52: the 15 s average of `mesh::utilization`.

use std::time::Duration;

use proptest::prelude::*;

use super::admission::{Contract, Counts, MAX_CONCURRENCY, MAX_RPS};
use super::utilization::{Averages, TIME_CONSTANT, Utilization};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn contract(concurrency: u32, rps: u32) -> Option<Contract> {
    Some(Contract { concurrency, rps })
}

/// What each 100 ms adds to the counts: the mean in flight and the offers by end.
#[derive(Clone, Copy, Default)]
struct Rates {
    in_flight: f64,
    at_once: u64,
    from_queue: u64,
    overloaded: u64,
    queue_timeout: u64,
    cancelled: u64,
    queued: u64,
}

/// Cumulative counts fed at a steady rate and a `Utilization` ticked with them.
struct Feed {
    rates: Rates,
    now: Duration,
    counts: Counts,
    u: Utilization,
}

impl Feed {
    /// The first tick, at `t = 0`.
    fn start(rates: Rates, c: Option<Contract>) -> Feed {
        let mut feed = Feed {
            rates,
            now: Duration::ZERO,
            counts: Counts::default(),
            u: Utilization::new(),
        };
        feed.u.tick(feed.now, c, feed.counts);
        feed
    }

    /// Advances by `step` (the rates scale with it) without a tick.
    fn advance(&mut self, step: Duration) {
        let r = self.rates;
        // In 100 ms units; every step of these tests is a multiple of 50 ms.
        let units = step.as_millis() as u64;
        let scaled = |per_100: u64| per_100 * units / 100;
        let c = &mut self.counts;
        c.in_flight_ns += (r.in_flight * step.as_nanos() as f64) as u128;
        c.admitted_at_once += scaled(r.at_once);
        c.admitted_from_queue += scaled(r.from_queue);
        c.overloaded += scaled(r.overloaded);
        c.queue_timeout += scaled(r.queue_timeout);
        c.cancelled += scaled(r.cancelled);
        c.queued += scaled(r.queued);
        // The instant value is not the mean: the average must not read it.
        c.in_flight = 0;
        self.now += step;
    }

    /// Ticks every `step` until `until`.
    fn run(&mut self, step: Duration, until: Duration, c: Option<Contract>) {
        while self.now < until {
            self.advance(step);
            self.u.tick(self.now, c, self.counts);
        }
    }

    fn value(&self) -> f64 {
        self.u.value().expect("a contract")
    }

    fn averages(&self) -> Averages {
        self.u.averages().expect("a contract")
    }
}

fn half(c: u32) -> Rates {
    Rates {
        in_flight: f64::from(c) / 2.0,
        ..Rates::default()
    }
}

/// `value()` at 1, 15 and 60 s of the half-contract feed ticked every `step`, with no tick in
/// `gap` (start, end).
fn curve(step: Duration, gap: Option<(Duration, Duration)>) -> [f64; 3] {
    let c = contract(64, 1000);
    let mut feed = Feed::start(half(64), c);
    let mut out = [0.0; 3];
    for (i, at) in [1, 15, 60].map(Duration::from_secs).into_iter().enumerate() {
        while feed.now < at {
            feed.advance(step);
            let skipped = gap.is_some_and(|(from, to)| feed.now > from && feed.now < to);
            if !skipped {
                feed.u.tick(feed.now, c, feed.counts);
            }
        }
        out[i] = feed.value();
    }
    out
}

fn rel(a: f64, b: f64) -> f64 {
    ((a - b) / b).abs()
}

#[test]
fn adm_52_a_steady_half_contract_follows_the_exponential() {
    assert_eq!(TIME_CONSTANT, Duration::from_secs(15));
    let got = curve(ms(100), None);
    for (v, t) in got.into_iter().zip([1.0, 15.0, 60.0]) {
        let want = 0.5 * (1.0 - f64::exp(-t / 15.0));
        assert!(rel(v, want) < 1e-9, "at {t} s: {v} against {want}");
    }
}

#[test]
fn adm_52_late_or_irregular_ticks_give_the_same_curve() {
    let reference = curve(ms(100), None);
    let gap = Some((Duration::from_secs(3), Duration::from_secs(13)));
    for (name, got) in [
        ("50 ms", curve(ms(50), None)),
        ("250 ms", curve(ms(250), None)),
        ("1 s", curve(ms(1000), None)),
        ("a 10 s gap", curve(ms(100), gap)),
    ] {
        for (i, (v, r)) in got.into_iter().zip(reference).enumerate() {
            assert!(rel(v, r) < 0.01, "{name}, instant {i}: {v} against {r}");
        }
    }
}

fn mix() -> Rates {
    Rates {
        at_once: 5,
        from_queue: 5,
        overloaded: 6,
        queue_timeout: 4,
        cancelled: 50,
        queued: 15,
        ..Rates::default()
    }
}

#[test]
fn adm_52_demand_at_twice_the_rps_tends_to_two_and_cancelled_adds_nothing() {
    let c = contract(1000, 100);
    let mut feed = Feed::start(mix(), c);
    feed.run(ms(100), Duration::from_secs(150), c);
    let v = feed.value();
    assert!((v - 2.0).abs() < 1e-3, "{v}");
    let demand = feed.averages().demand;
    assert!(rel(demand, 200.0) < 1e-3, "{demand}");

    let mut bare = Feed::start(
        Rates {
            cancelled: 0,
            queued: 0,
            ..mix()
        },
        c,
    );
    bare.run(ms(100), Duration::from_secs(150), c);
    assert_eq!(
        bare.value().to_bits(),
        v.to_bits(),
        "{} against {v}",
        bare.value()
    );
}

#[test]
fn adm_52_the_value_is_the_larger_term_and_in_flight_never_exceeds_one() {
    let long = Duration::from_secs(900);
    // In-flight term 0.5, demand term 0.3.
    let c = contract(10, 100);
    let rates = Rates {
        in_flight: 5.0,
        at_once: 3,
        ..Rates::default()
    };
    let mut feed = Feed::start(rates, c);
    feed.run(ms(100), long, c);
    assert!((feed.value() - 0.5).abs() < 1e-9, "{}", feed.value());
    // In-flight term 0.2, demand term 1.5.
    let rates = Rates {
        in_flight: 2.0,
        at_once: 15,
        ..Rates::default()
    };
    let mut feed = Feed::start(rates, c);
    feed.run(ms(100), long, c);
    assert!((feed.value() - 1.5).abs() < 1e-9, "{}", feed.value());
    // A steady 4 in flight under 8, then a contract of 2.
    let eight = contract(8, 1_000_000);
    let mut feed = Feed::start(
        Rates {
            in_flight: 4.0,
            ..Rates::default()
        },
        eight,
    );
    feed.run(ms(100), long, eight);
    assert!((feed.value() - 0.5).abs() < 1e-9, "{}", feed.value());
    let two = contract(2, 1_000_000);
    feed.run(ms(100), long + ms(100), two);
    assert_eq!(feed.value(), 1.0);
    assert!(
        (feed.averages().in_flight - 4.0).abs() < 1e-9,
        "{:?}",
        feed.averages()
    );
}

#[test]
fn adm_52_new_values_keep_the_averages_and_a_new_contract_starts_at_zero() {
    let rates = Rates {
        in_flight: 4.0,
        at_once: 5,
        ..Rates::default()
    };
    let alpha = -f64::exp_m1(-0.1 / 15.0);
    let (sample_in_flight, sample_demand) = (4.0, 50.0);
    // (a) 8/100 for 30 s, then 16/200.
    let first = contract(8, 100);
    let mut feed = Feed::start(rates, first);
    feed.run(ms(100), Duration::from_secs(30), first);
    let old = feed.averages();
    assert!(old.in_flight > 0.5 && old.demand > 5.0, "{old:?}");
    feed.run(
        ms(100),
        Duration::from_secs(30) + ms(100),
        contract(16, 200),
    );
    let new = feed.averages();
    let want = Averages {
        in_flight: old.in_flight + alpha * (sample_in_flight - old.in_flight),
        demand: old.demand + alpha * (sample_demand - old.demand),
    };
    assert!(
        (new.in_flight - want.in_flight).abs() < 1e-12,
        "{new:?} {want:?}"
    );
    assert!((new.demand - want.demand).abs() < 1e-12, "{new:?} {want:?}");
    let value = f64::max(f64::min(1.0, new.in_flight / 16.0), new.demand / 200.0);
    assert_eq!(feed.value(), value);

    // (b) Removed, then a contract again: it starts at 0.
    feed.advance(ms(100));
    feed.u.tick(feed.now, None, feed.counts);
    assert_eq!(feed.u.value(), None);
    assert_eq!(feed.u.averages(), None);
    feed.advance(ms(100));
    feed.u.tick(feed.now, first, feed.counts);
    assert_eq!(feed.u.value(), Some(0.0));
    feed.advance(ms(100));
    feed.u.tick(feed.now, first, feed.counts);
    let restarted = feed.averages();
    assert!(
        (restarted.in_flight - alpha * sample_in_flight).abs() < 1e-12,
        "{restarted:?}"
    );
    assert!(
        (restarted.demand - alpha * sample_demand).abs() < 1e-12,
        "{restarted:?}"
    );
}

#[test]
fn adm_52_a_tick_not_after_the_previous_one_changes_nothing() {
    let c = contract(8, 100);
    let rates = Rates {
        in_flight: 4.0,
        at_once: 5,
        ..Rates::default()
    };
    let mut feed = Feed::start(rates, c);
    let mut twin = Feed::start(rates, c);
    feed.run(ms(100), Duration::from_secs(5), c);
    twin.run(ms(100), Duration::from_secs(5), c);
    let (averages, value) = (feed.averages(), feed.value());
    let t = feed.now;
    let doubled = Counts {
        in_flight_ns: feed.counts.in_flight_ns * 2,
        admitted_at_once: feed.counts.admitted_at_once * 2,
        overloaded: 1000,
        ..feed.counts
    };
    feed.u.tick(t, contract(2, 10), doubled);
    assert_eq!((feed.averages(), feed.value()), (averages, value));
    feed.u.tick(t - ms(1), contract(2, 10), doubled);
    assert_eq!((feed.averages(), feed.value()), (averages, value));
    feed.run(ms(100), t + ms(100), c);
    twin.run(ms(100), t + ms(100), c);
    let (a, b) = (feed.averages(), twin.averages());
    assert_eq!(
        (a.in_flight.to_bits(), a.demand.to_bits()),
        (b.in_flight.to_bits(), b.demand.to_bits()),
        "{a:?} {b:?}"
    );
    assert_eq!(feed.value().to_bits(), twin.value().to_bits());
}

/// One step of the property: an instant move in ms (-1000 to 20000), count increments and a
/// contract.
type Step = (i64, [u64; 6], u128, Option<(u32, u32)>);

fn step() -> impl Strategy<Value = Step> {
    (
        -1000i64..=20_000,
        prop::array::uniform6(0u64..=u64::MAX / 4),
        0u128..=u128::MAX / 4,
        prop::option::of((1..=MAX_CONCURRENCY, 1..=MAX_RPS)),
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn adm_52_any_counts_and_contracts_give_a_finite_value_at_least_zero(
        steps in prop::collection::vec(step(), 1..=200),
    ) {
        let mut u = Utilization::new();
        let mut now = Duration::from_secs(3600);
        let mut counts = Counts::default();
        // The contract in force and the instant of the last tick taken, as the doc says.
        let mut held: Option<(Contract, Duration)> = None;
        for (moved, inc, in_flight_ns, c) in steps {
            now = if moved < 0 {
                now.saturating_sub(ms(moved.unsigned_abs()))
            } else {
                now + ms(moved as u64)
            };
            counts.admitted_at_once = counts.admitted_at_once.saturating_add(inc[0]);
            counts.admitted_from_queue = counts.admitted_from_queue.saturating_add(inc[1]);
            counts.overloaded = counts.overloaded.saturating_add(inc[2]);
            counts.queue_timeout = counts.queue_timeout.saturating_add(inc[3]);
            counts.cancelled = counts.cancelled.saturating_add(inc[4]);
            counts.queued = counts.queued.saturating_add(inc[5]);
            counts.in_flight_ns = counts.in_flight_ns.saturating_add(in_flight_ns);
            let c = c.map(|(concurrency, rps)| Contract { concurrency, rps });
            u.tick(now, c, counts);
            held = match (c, held) {
                (None, _) => None,
                (Some(c), None) => Some((c, now)),
                (Some(c), Some((_, at))) if now > at => Some((c, now)),
                (Some(_), kept) => kept,
            };
            prop_assert_eq!(u.value().is_some(), c.is_some());
            if let (Some(v), Some(a), Some((c, _))) = (u.value(), u.averages(), held) {
                prop_assert!(v.is_finite() && v >= 0.0, "{}", v);
                prop_assert!(v <= f64::max(1.0, a.demand / f64::from(c.rps)), "{} {:?}", v, a);
            }
        }
    }
}
