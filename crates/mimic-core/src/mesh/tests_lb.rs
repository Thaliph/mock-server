//! MSH-43 to MSH-45: the pick and the slow start of `mesh::lb`.

use std::net::SocketAddr;
use std::time::Duration;

use proptest::prelude::*;

use super::lb::{Endpoints, MIN_WEIGHT};

const S: Duration = Duration::from_secs(1);

fn addr(n: u8) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, n], 8080))
}

fn addrs(n: u8) -> Vec<SocketAddr> {
    (1..=n).map(addr).collect()
}

/// An endpoint set past its first view, all at full weight.
fn full(n: u8) -> Endpoints {
    let mut set = Endpoints::new(30 * S);
    set.update(&addrs(n), Duration::ZERO);
    set
}

#[test]
fn msh_43_pick_takes_the_lower_in_flight_of_two_distinct_draws() {
    let set = full(3);
    let load = [5u32, 1, 3];
    // Draws: first index 0 of 3, second index 0 of 2 which becomes index 1 (distinct).
    let mut calls = Vec::new();
    let mut draw = |n: usize| {
        calls.push(n);
        0
    };
    let picked = set.pick(Duration::ZERO, |i| load[i], &mut draw);
    assert_eq!(picked, Some(1), "endpoint 1 has 1 in flight against 5");
    assert_eq!(calls, vec![3, 2], "the draws ask for n then n - 1");
}

#[test]
fn msh_43_pick_is_independent_of_the_order_of_the_draws() {
    let set = full(2);
    let load = [4u32, 0];
    for draws in [[0usize, 0], [1, 0]] {
        let mut values = draws.into_iter();
        let mut draw = move |_n: usize| values.next().unwrap_or(0);
        assert_eq!(set.pick(Duration::ZERO, |i| load[i], &mut draw), Some(1));
    }
}

#[test]
fn msh_43_pick_breaks_a_tie_for_the_first_draw() {
    let set = full(3);
    for first in 0..3usize {
        let mut values = vec![first, 0].into_iter();
        let mut draw = move |_n: usize| values.next().unwrap_or(0);
        assert_eq!(
            set.pick(Duration::ZERO, |_| 2, &mut draw),
            Some(first),
            "equal load: the first draw"
        );
    }
}

#[test]
fn msh_43_pick_with_one_endpoint_draws_nothing_and_with_none_gives_none() {
    let one = full(1);
    let mut draw = |_n: usize| panic!("no draw with one endpoint");
    assert_eq!(one.pick(Duration::ZERO, |_| 9, &mut draw), Some(0));
    let none = full(0);
    assert_eq!(none.pick(Duration::ZERO, |_| 0, &mut draw), None);
    assert!(none.is_empty());
    assert_eq!(none.addr(0), None);
}

#[test]
fn msh_43_pick_divides_the_load_by_the_weight() {
    // Endpoint 0 has full weight and one request in flight, endpoint 1 is new (weight 0.1)
    // and idle: (1 + 1) / 1 = 2 against (0 + 1) / 0.1 = 10.
    let mut set = Endpoints::new(10 * S);
    set.update(&[addr(1)], Duration::ZERO);
    set.update(&[addr(1), addr(2)], Duration::ZERO);
    let mut draw = |_n: usize| 0;
    let load = [1u32, 0];
    assert_eq!(set.pick(Duration::ZERO, |i| load[i], &mut draw), Some(0));
    // Once the window has passed both weigh 1 and the idle one wins.
    assert_eq!(set.pick(10 * S, |i| load[i], &mut draw), Some(1));
}

#[test]
fn msh_45_slow_start_weights() {
    let mut set = Endpoints::new(10 * S);
    set.update(&[addr(1), addr(2)], 100 * S);
    // The first view: full weight, at any time.
    assert_eq!(set.weight(0, 100 * S), 1.0);
    assert_eq!(set.weight(1, 100 * S), 1.0);
    set.update(&[addr(1), addr(2), addr(3)], 200 * S);
    assert_eq!(set.addr(2), Some(addr(3)));
    assert_eq!(set.weight(2, 200 * S), MIN_WEIGHT, "age 0");
    assert_eq!(
        set.weight(2, 201 * S),
        MIN_WEIGHT,
        "1/10 is not above the floor"
    );
    assert!(
        (set.weight(2, 205 * S) - 0.5).abs() < 1e-9,
        "half the window"
    );
    assert_eq!(set.weight(2, 210 * S), 1.0, "the whole window");
    assert_eq!(set.weight(2, 900 * S), 1.0, "never above 1");
    assert_eq!(
        set.weight(2, 100 * S),
        MIN_WEIGHT,
        "a clock behind the birth is age 0"
    );
    assert_eq!(
        set.weight(0, 200 * S),
        1.0,
        "known endpoints keep their weight"
    );
}

#[test]
fn msh_45_a_known_endpoint_keeps_its_first_seen_time_and_a_returning_one_is_new() {
    let mut set = Endpoints::new(10 * S);
    set.update(&[addr(1)], Duration::ZERO);
    set.update(&[addr(1), addr(2)], 100 * S);
    set.update(&[addr(1), addr(2)], 105 * S);
    assert!(
        (set.weight(1, 105 * S) - 0.5).abs() < 1e-9,
        "kept across updates"
    );
    set.update(&[addr(1)], 106 * S);
    set.update(&[addr(1), addr(2)], 120 * S);
    assert_eq!(
        set.weight(1, 120 * S),
        MIN_WEIGHT,
        "it left and came back: new"
    );
}

#[test]
fn msh_45_window_zero_disables_the_slow_start() {
    let mut set = Endpoints::new(Duration::ZERO);
    set.update(&[addr(1)], Duration::ZERO);
    set.update(&[addr(1), addr(2)], 5 * S);
    assert_eq!(set.weight(1, 5 * S), 1.0);
    let mut set = Endpoints::new(10 * S);
    set.update(&[addr(1)], Duration::ZERO);
    set.update(&[addr(1), addr(2)], 5 * S);
    set.set_window(Duration::ZERO);
    assert_eq!(
        set.weight(1, 5 * S),
        1.0,
        "a window set to 0 applies to the endpoints in ramp"
    );
}

#[test]
fn msh_45_update_does_not_depend_on_the_order_or_duplicates_given() {
    let mut a = Endpoints::new(10 * S);
    a.update(&[addr(3), addr(1), addr(2), addr(1)], Duration::ZERO);
    let mut b = Endpoints::new(10 * S);
    b.update(&[addr(1), addr(2), addr(3)], Duration::ZERO);
    assert_eq!(a.len(), 3);
    for i in 0..3 {
        assert_eq!(a.addr(i), b.addr(i));
        assert_eq!(a.addr(i), Some(addr(i as u8 + 1)), "sorted by address");
    }
}

/// A small deterministic generator for the property tests.
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
    /// MSH-44: requests that complete after a random time, 10 000 picks.
    #[test]
    fn msh_44_distribution_under_feedback(n in 2u8..9, seed in 1u64..u64::MAX, hold in 1usize..20) {
        let set = full(n);
        let mut rng = Rng(seed);
        let mut in_flight = vec![0u32; n as usize];
        let mut counts = vec![0u32; n as usize];
        let mut pending: Vec<(usize, usize)> = Vec::new();
        for step in 0..10_000usize {
            let load = in_flight.clone();
            let mut r = Rng(seed ^ (step as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut draw = |m: usize| r.below(m);
            let i = set.pick(Duration::ZERO, |k| load[k], &mut draw).unwrap();
            prop_assert!(i < n as usize);
            counts[i] += 1;
            in_flight[i] += 1;
            pending.push((step + 1 + rng.below(hold), i));
            pending.retain(|&(due, k)| {
                if due <= step { in_flight[k] -= 1; false } else { true }
            });
        }
        let mean = 10_000.0 / f64::from(n);
        let busiest = f64::from(*counts.iter().max().unwrap());
        prop_assert!(busiest <= 2.0 * mean, "busiest {busiest} over twice the mean {mean}: {counts:?}");
    }

    #[test]
    fn msh_44_never_passes_over_the_idle_one_for_a_loaded_one(
        loads in prop::collection::vec(0u32..50, 2..12),
        seed in 1u64..u64::MAX,
    ) {
        let n = u8::try_from(loads.len()).unwrap();
        let set = full(n);
        let mut r = Rng(seed);
        let mut draws = Vec::new();
        let mut draw = |m: usize| { let v = r.below(m); draws.push(v); v };
        let i = set.pick(Duration::ZERO, |k| loads[k], &mut draw).unwrap();
        // The two indices drawn, the second shifted past the first.
        let first = draws[0];
        let second = if draws[1] >= first { draws[1] + 1 } else { draws[1] };
        prop_assert_ne!(first, second);
        prop_assert!(i == first || i == second);
        prop_assert!(loads[i] <= loads[first].min(loads[second]));
    }

    #[test]
    fn msh_44_a_full_weight_endpoint_beats_one_in_ramp_under_the_same_load(
        seed in 1u64..u64::MAX,
        load in 0u32..20,
    ) {
        let mut set = Endpoints::new(100 * S);
        set.update(&[addr(1)], Duration::ZERO);
        set.update(&[addr(1), addr(2)], Duration::ZERO);
        let mut counts = [0u32; 2];
        let mut r = Rng(seed);
        for _ in 0..2000 {
            let mut draw = |m: usize| r.below(m);
            let i = set.pick(S, |_| load, &mut draw).unwrap();
            counts[i] += 1;
        }
        prop_assert!(counts[0] > counts[1], "{counts:?}");
    }
}
