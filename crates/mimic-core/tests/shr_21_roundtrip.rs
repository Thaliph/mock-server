//! SHR-21 (#339): `to_json(from_json(x))` equals `x` for every report the recorder and the
//! bounded merge produce, and `from_json` accepts object keys in any order and keeps the last of
//! duplicate keys at every map of the report.

mod common;

use common::{Op, build, limits};
use mimic_core::discovery::report::Report;
use mimic_core::discovery::store::merge_bounded;
use proptest::prelude::*;
use serde_json::Value;

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

fn roundtrip(json: &str) {
    let parsed = Report::from_json(json).expect("a report the recorder wrote reads back");
    assert_eq!(parsed.to_json().expect("serialises"), json);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn shr_21_roundtrip_of_recorded_reports(
        ops in ops(),
        endpoints in 1..12usize,
        authorities in 1..5usize,
        budget in 512..65_536usize,
    ) {
        let limits = limits(endpoints, authorities, budget);
        roundtrip(&build(&ops, &limits, 19_000).to_json().expect("serialises"));
    }

    #[test]
    fn shr_21_roundtrip_of_bounded_merges(
        a in ops(),
        b in ops(),
        c in ops(),
        endpoints in 1..12usize,
        authorities in 1..5usize,
        budget in 512..65_536usize,
    ) {
        let limits = limits(endpoints, authorities, budget);
        let mut stored = Report::new(None);
        for (day, ops) in (19_000u64..).zip([a, b, c]) {
            stored = merge_bounded(stored, build(&ops, &limits, day), &limits).report;
            roundtrip(&stored.to_json().expect("serialises"));
            let (inbound, outbound) = stored.split();
            roundtrip(&inbound.to_json().expect("serialises"));
            if let Some(outbound) = outbound {
                roundtrip(&outbound.to_json().expect("serialises"));
            }
        }
    }
}

/// Writes `value` with the keys of every object in reverse order. At the objects `maps` selects
/// (a map of the report, whose values all have the same type), each entry is also written first
/// with the value of the next entry, so that the last of the duplicates is the real one.
fn scramble(value: &Value, key: &str, parent: &str, out: &mut String) {
    match value {
        Value::Object(map) => {
            let is_map = map.len() > 1 && is_map(key, parent);
            let entries: Vec<(&String, &Value)> = map.iter().rev().collect();
            out.push('{');
            let mut first = true;
            let mut write = |k: &str, v: &Value, out: &mut String| {
                if !first {
                    out.push(',');
                }
                first = false;
                out.push_str(&Value::from(k).to_string());
                out.push(':');
                scramble(v, k, key, out);
            };
            for (i, (k, v)) in entries.iter().enumerate() {
                if is_map {
                    write(k, entries[(i + 1) % entries.len()].1, out);
                }
                write(k, v, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                scramble(item, key, parent, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// The maps of a report by the name of the member that holds them (the generators of `common`
/// use no literal segment with one of these names), and the method maps under `raw`.
fn is_map(key: &str, parent: &str) -> bool {
    matches!(
        key,
        "scopes" | "inbound" | "outbound" | "literals" | "endpoints" | "raw" | "statuses"
    ) || parent == "raw"
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn shr_21_from_json_accepts_any_key_order_and_keeps_the_last_duplicate(
        a in ops(),
        b in ops(),
    ) {
        let limits = limits(12, 5, 65_536);
        let report = merge_bounded(
            build(&a, &limits, 19_000),
            build(&b, &limits, 19_001),
            &limits,
        )
        .report;
        let json = report.to_json().expect("serialises");
        let value: Value = serde_json::from_str(&json).expect("JSON");
        let mut scrambled = String::new();
        scramble(&value, "", "", &mut scrambled);
        let parsed = Report::from_json(&scrambled).expect("any key order reads");
        prop_assert_eq!(parsed.to_json().expect("serialises"), json);
    }
}
