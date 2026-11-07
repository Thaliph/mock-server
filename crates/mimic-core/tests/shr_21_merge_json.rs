//! SHR-21 (#339), merge clause: merging a push read from its JSON into a stored report gives the
//! same bytes and the same `refused` and `evicted` counts as the bounded merge of the parsed
//! push, for any order and grouping of pushes.

mod common;

use common::{Op, build, limits};
use mimic_core::discovery::report::{Limits, Report};
use mimic_core::discovery::store::{merge_bounded, merge_bounded_json};
use proptest::prelude::*;

fn op() -> impl Strategy<Value = Op> {
    (
        any::<bool>(),
        0..6usize,
        0..8usize,
        0..10usize,
        0..8usize,
        (0..4usize, 0..8usize, 0..12usize),
    )
        .prop_map(
            |(outbound, target, method, path, status, (query, body, tunnel))| Op {
                outbound,
                target,
                method,
                path,
                status,
                query,
                body,
                tunnel: tunnel == 0,
            },
        )
}

fn ops() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(op(), 1..30)
}

/// Merges `push` into `stored` both ways, checks that they agree and returns the result.
fn both(stored: Report, push: Report, limits: &Limits) -> Report {
    let json = push.to_json().expect("serialises");
    let expected = merge_bounded(stored.clone(), push, limits);
    let streamed = merge_bounded_json(stored, &json, limits).expect("a report we wrote reads");
    assert_eq!(streamed.refused, expected.refused);
    assert_eq!(streamed.evicted, expected.evicted);
    assert_eq!(
        streamed.report.to_json().expect("serialises"),
        expected.report.to_json().expect("serialises")
    );
    expected.report
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn shr_21_merge_while_parsing_equals_bounded_merge(
        a in ops(),
        b in ops(),
        c in ops(),
        order in 0..6usize,
        grouped in any::<bool>(),
        endpoints in 1..12usize,
        authorities in 1..5usize,
        budget in 512..65_536usize,
    ) {
        let limits = limits(endpoints, authorities, budget);
        let all = [
            build(&a, &limits, 19_000),
            build(&b, &limits, 19_001),
            build(&c, &limits, 19_002),
        ];
        // The six orders of three pushes.
        let [i, j, k] = [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]][order];
        let mut pushes = vec![all[i].clone(), all[j].clone(), all[k].clone()];
        if grouped {
            // The last two are merged first: a stored report is a push too.
            let last = pushes.pop().expect("three pushes");
            let middle = pushes.pop().expect("three pushes");
            pushes.push(both(middle, last, &limits));
        }
        let mut stored = Report::new(None);
        for push in pushes {
            stored = both(stored, push, &limits);
        }
    }
}
