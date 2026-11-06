//! SHR-04: the per-direction sample floor of the bounded merge (spec §7.3).

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use super::observed::Observation;
use super::redact::Redactor;
use super::report::{Direction, Exchange, Limits, Report, Via};
use super::scope::ScopeKey;
use super::store::{Kind, Refusal, merge_bounded};

const AUTHORITY: &str = "a.example.com:443";

fn limits(sample_budget: usize) -> Limits {
    Limits {
        max_literals: 8,
        max_endpoints: 256,
        max_authorities: 64,
        sample_budget,
    }
}

/// One sample of `path` in `direction`: `q` distinguishes samples of the same class, and
/// every sample has the same size.
fn record(r: &mut Report, direction: Direction, path: &str, status: u16, q: usize) {
    let query = format!("q{q}=1");
    let exchange = Exchange {
        direction,
        port: 80,
        method: "GET",
        path,
        status,
        head_ms: 1,
        host: None,
        grpc: false,
        observation: Observation {
            query: &query,
            ..Observation::default()
        },
    };
    let key = match direction {
        Direction::Inbound => ScopeKey::Port(80),
        Direction::Outbound => ScopeKey::Authority(AUTHORITY.into()),
    };
    let via = Via {
        tls: true,
        sni: None,
        dst: None,
    };
    r.record_at(
        key,
        &via,
        &limits(64 << 20),
        &Redactor::default(),
        &exchange,
    );
}

/// The bytes of one sample.
fn unit() -> usize {
    let mut r = Report::new(None);
    record(&mut r, Direction::Inbound, "/i", 200, 1);
    let size = r.sample_bytes();
    let mut o = Report::new(None);
    record(&mut o, Direction::Outbound, "/o", 200, 1);
    assert_eq!(o.sample_bytes(), size, "samples differ in size");
    size
}

/// A budget of six samples and a half.
fn budget() -> usize {
    unit() * 6 + unit() / 2
}

/// `(direction, path, samples)`: `samples` samples of status 200 on the endpoint.
type Spec<'a> = (Direction, &'a str, usize);

fn build(specs: &[Spec<'_>]) -> Report {
    let mut r = Report::new(None);
    for (direction, path, samples) in specs {
        for q in 0..*samples {
            record(&mut r, *direction, path, 200, q);
        }
    }
    r
}

/// The samples held per endpoint: `(direction, path)` to count.
fn held(r: &Report) -> BTreeMap<(Direction, String), usize> {
    r.endpoints()
        .into_iter()
        .map(|e| ((e.direction, e.template), e.stats.samples().len()))
        .collect()
}

fn count(r: &Report, direction: Direction, path: &str) -> usize {
    held(r)
        .get(&(direction, path.to_string()))
        .copied()
        .unwrap_or(0)
}

#[test]
fn shr_04_outbound_growth_evicts_outbound_while_inbound_holds_half_or_less() {
    // Inbound 3 samples in one pair, outbound 2 + 1: full (6 of 6.5).
    let stored = build(&[
        (Direction::Inbound, "/i", 3),
        (Direction::Outbound, "/o", 2),
        (Direction::Outbound, "/p", 1),
    ]);
    let push = build(&[(Direction::Outbound, "/n", 1)]);
    let out = merge_bounded(stored, push, &limits(budget()));
    assert_eq!(count(&out.report, Direction::Inbound, "/i"), 3);
    assert_eq!(count(&out.report, Direction::Outbound, "/o"), 1);
    assert_eq!(count(&out.report, Direction::Outbound, "/n"), 1);
    assert_eq!(out.evicted, BTreeMap::from([(Kind::Sample, 1)]));
    assert!(out.refused.is_empty());
}

#[test]
fn shr_04_outbound_growth_is_refused_when_no_outbound_pair_can_give() {
    let stored = build(&[
        (Direction::Inbound, "/i", 3),
        (Direction::Outbound, "/o1", 1),
        (Direction::Outbound, "/o2", 1),
        (Direction::Outbound, "/o3", 1),
    ]);
    let push = build(&[(Direction::Outbound, "/n", 1)]);
    let out = merge_bounded(stored.clone(), push, &limits(budget()));
    assert_eq!(count(&out.report, Direction::Inbound, "/i"), 3);
    assert_eq!(count(&out.report, Direction::Outbound, "/n"), 0);
    assert!(out.evicted.is_empty());
    assert_eq!(out.refused, BTreeMap::from([(Refusal::Budget, 1)]));
    assert_eq!(out.report.sample_bytes(), stored.sample_bytes());
}

#[test]
fn shr_04_inbound_growth_evicts_inbound_while_outbound_holds_half_or_less() {
    let stored = build(&[
        (Direction::Outbound, "/o", 3),
        (Direction::Inbound, "/i", 2),
        (Direction::Inbound, "/j", 1),
    ]);
    let push = build(&[(Direction::Inbound, "/n", 1)]);
    let out = merge_bounded(stored, push, &limits(budget()));
    assert_eq!(count(&out.report, Direction::Outbound, "/o"), 3);
    assert_eq!(count(&out.report, Direction::Inbound, "/i"), 1);
    assert_eq!(count(&out.report, Direction::Inbound, "/n"), 1);
    assert_eq!(out.evicted, BTreeMap::from([(Kind::Sample, 1)]));
}

#[test]
fn shr_04_inbound_growth_is_refused_when_no_inbound_pair_can_give() {
    let stored = build(&[
        (Direction::Outbound, "/o", 3),
        (Direction::Inbound, "/i1", 1),
        (Direction::Inbound, "/i2", 1),
        (Direction::Inbound, "/i3", 1),
    ]);
    let push = build(&[(Direction::Inbound, "/n", 1)]);
    let out = merge_bounded(stored, push, &limits(budget()));
    assert_eq!(count(&out.report, Direction::Outbound, "/o"), 3);
    assert_eq!(count(&out.report, Direction::Inbound, "/n"), 0);
    assert!(out.evicted.is_empty());
    assert_eq!(out.refused, BTreeMap::from([(Refusal::Budget, 1)]));
}

#[test]
fn shr_04_the_other_direction_gives_while_it_holds_more_than_half() {
    // Inbound 4 of 6.5 (above half), outbound 2.
    let stored = build(&[
        (Direction::Inbound, "/i", 4),
        (Direction::Outbound, "/o", 2),
    ]);
    let push = build(&[(Direction::Outbound, "/n", 1)]);
    let out = merge_bounded(stored, push, &limits(budget()));
    assert_eq!(count(&out.report, Direction::Inbound, "/i"), 3);
    assert_eq!(count(&out.report, Direction::Outbound, "/o"), 2);
    assert_eq!(count(&out.report, Direction::Outbound, "/n"), 1);
    assert_eq!(out.evicted, BTreeMap::from([(Kind::Sample, 1)]));
}

#[test]
fn shr_04_a_lowered_budget_trims_the_direction_above_half() {
    // 8 samples over a budget of 6.5: inbound holds 3 (at most half), outbound 5.
    let stored = build(&[
        (Direction::Inbound, "/i", 3),
        (Direction::Outbound, "/o1", 2),
        (Direction::Outbound, "/o2", 2),
        (Direction::Outbound, "/o3", 1),
    ]);
    let out = merge_bounded(stored, Report::new(None), &limits(budget()));
    assert_eq!(count(&out.report, Direction::Inbound, "/i"), 3);
    assert!(out.report.sample_bytes() <= budget());
    assert_eq!(out.evicted, BTreeMap::from([(Kind::Sample, 2)]));
    assert_eq!(held(&out.report).values().sum::<usize>(), 6);
}

const PATHS: [&str; 3] = ["/a", "/b", "/c"];

type Item = (bool, usize, usize, bool);

fn push_of() -> impl Strategy<Value = Vec<Item>> {
    proptest::collection::vec(
        (any::<bool>(), 0..PATHS.len(), 0usize..5, any::<bool>()),
        0..10,
    )
}

fn push_report(items: &[Item], only: Option<Direction>) -> Report {
    let mut r = Report::new(None);
    for (out, path, q, five) in items {
        let direction = if *out {
            Direction::Outbound
        } else {
            Direction::Inbound
        };
        if only.is_some_and(|d| d != direction) {
            continue;
        }
        let status = if *five { 500 } else { 200 };
        record(&mut r, direction, PATHS[*path], status, *q);
    }
    r
}

fn hashes(r: &Report, direction: Direction) -> BTreeSet<(String, u64, u16)> {
    r.endpoints()
        .into_iter()
        .filter(|e| e.direction == direction)
        .flat_map(|e| {
            let path = e.template.clone();
            e.stats
                .samples()
                .iter()
                .map(|(h, s)| (path.clone(), h, s.status()))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn bytes(r: &Report, direction: Direction) -> usize {
    r.endpoints()
        .into_iter()
        .filter(|e| e.direction == direction)
        .map(|e| e.stats.samples().bytes())
        .sum()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn shr_04_a_direction_at_most_half_never_loses_a_sample_to_the_other(
        pushes in proptest::collection::vec((push_of(), any::<bool>()), 1..8),
        extra in 0usize..3,
    ) {
        let b = unit() * (4 + extra) + unit() / 2;
        let l = limits(b);
        let mut stored = Report::new(None);
        for (items, outbound_grows) in pushes {
            // A push of one direction: the other one can only lose samples to it.
            let (grower, protected) = if outbound_grows {
                (Direction::Outbound, Direction::Inbound)
            } else {
                (Direction::Inbound, Direction::Outbound)
            };
            let push = push_report(&items, Some(grower));
            let before = hashes(&stored, protected);
            let was_half = bytes(&stored, protected) <= b / 2;
            let out = merge_bounded(stored, push, &l);
            prop_assert!(out.report.sample_bytes() <= b);
            if was_half {
                let after = hashes(&out.report, protected);
                prop_assert!(before.is_subset(&after), "{before:?} lost samples: {after:?}");
            }
            stored = out.report;
        }
    }

    /// Pushes of both directions: a direction at most half only loses samples to its own
    /// new ones, or to the other once its own growth took it above half.
    #[test]
    fn shr_04_mixed_pushes_respect_the_floor(
        pushes in proptest::collection::vec(push_of(), 1..8),
        extra in 0usize..3,
    ) {
        let unit = unit();
        let b = unit * (4 + extra) + unit / 2;
        let l = limits(b);
        let mut stored = Report::new(None);
        for items in pushes {
            let push = push_report(&items, None);
            let out = merge_bounded(stored.clone(), push.clone(), &l);
            prop_assert!(out.report.sample_bytes() <= b);
            for direction in [Direction::Inbound, Direction::Outbound] {
                let own = items.iter().filter(|i| i.0 == (direction == Direction::Outbound)).count();
                let (before_bytes, before) = (bytes(&stored, direction), hashes(&stored, direction));
                if before_bytes <= b / 2 {
                    let after = hashes(&out.report, direction);
                    if own == 0 {
                        // No growth of its own: not a single sample lost.
                        prop_assert!(before.is_subset(&after), "{before:?} lost samples: {after:?}");
                    }
                    // Whatever the other direction does, it does not shrink: it loses a
                    // sample only to a new one of its own, of the same size.
                    prop_assert!(
                        bytes(&out.report, direction) >= before_bytes,
                        "{direction:?} shrank from {before_bytes}"
                    );
                }
            }
            stored = out.report;
        }
    }

    #[test]
    fn shr_04_the_merge_stays_commutative_below_the_budget(a in push_of(), b in push_of()) {
        let l = limits(64 << 20);
        let (x, y) = (push_report(&a, None), push_report(&b, None));
        let xy = merge_bounded(x.clone(), y.clone(), &l).report;
        let yx = merge_bounded(y, x, &l).report;
        prop_assert_eq!(xy.to_json().unwrap(), yx.to_json().unwrap());
    }
}
