//! ADM-50: `mesh::reason` is exactly the §9 `x-mimic-error` table.

use std::collections::BTreeSet;

use proptest::prelude::*;

use super::admission;
use super::reason::Reason;
use super::retry::{self, Outcome};

/// The rows of the §9 reasons table: each name with its status (`None` when backticked).
fn spec_table() -> Vec<(String, Option<u16>)> {
    let spec = include_str!("../../../../docs/SPEC.md");
    let mut lines = spec
        .lines()
        .map(str::trim)
        .skip_while(|l| !l.starts_with("| Reason | Status | Who answers"))
        .skip(2);
    let mut rows = Vec::new();
    for row in lines.by_ref().take_while(|l| l.starts_with('|')) {
        let cells: Vec<&str> = row.split('|').map(str::trim).collect();
        let names: Vec<String> = cells[1]
            .split('`')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect();
        let statuses: Vec<Option<u16>> = cells[2]
            .split(", ")
            .map(|s| {
                if s.starts_with('`') {
                    None
                } else {
                    Some(s.parse().expect("a numeric status"))
                }
            })
            .collect();
        let statuses = if statuses.len() == 1 {
            vec![statuses[0]; names.len()]
        } else {
            statuses
        };
        assert_eq!(
            names.len(),
            statuses.len(),
            "names and statuses of a §9 row"
        );
        rows.extend(names.into_iter().zip(statuses));
    }
    rows
}

#[test]
fn adm_50_all_is_exactly_the_reasons_table_of_the_spec() {
    let table = spec_table();
    assert_eq!(table.len(), 13, "the §9 table rows");
    let listed: BTreeSet<&str> = table.iter().map(|(n, _)| n.as_str()).collect();
    let code: BTreeSet<&str> = Reason::ALL.iter().map(|r| r.as_str()).collect();
    assert_eq!(code.len(), Reason::ALL.len(), "ALL has a duplicate");
    assert_eq!(listed, code);
    for (name, status) in &table {
        let reason = Reason::parse(name.as_bytes()).expect("a reason of the table");
        assert_eq!(reason.status(), *status, "status of {name}");
    }
    assert_eq!(Reason::FaultInjected.status(), None);
    assert_eq!(Reason::NoRoute.status(), Some(404));
    let order: Vec<&str> = Reason::ALL.iter().map(|r| r.as_str()).collect();
    let rows: Vec<&str> = table.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(order, rows, "ALL in the table's row order");
}

#[test]
fn adm_50_as_str_and_parse_are_inverse_and_parse_refuses_the_rest() {
    for r in Reason::ALL {
        assert_eq!(Reason::parse(r.as_str().as_bytes()), Some(r));
    }
    assert_eq!(Reason::parse(b"queue-timeout"), Some(Reason::QueueTimeout));
    for rejected in [
        &b""[..],
        b"Overloaded",
        b" overloaded",
        b"overloaded ",
        b"queue_timeout",
        b"app-refused",
        b"none",
        b"overloaded\0",
        b"OVERLOADED",
        b"overloade",
        b"overloadedd",
    ] {
        assert_eq!(Reason::parse(rejected), None, "{rejected:?}");
    }
    let mut big = vec![b'a'; 64 * 1024];
    big[..10].copy_from_slice(b"overloaded");
    assert_eq!(Reason::parse(&big), None);
}

fn names() -> BTreeSet<&'static [u8]> {
    Reason::ALL.iter().map(|r| r.as_str().as_bytes()).collect()
}

/// A name with one byte changed: flipped bits, or its case swapped.
fn near_name() -> impl Strategy<Value = Vec<u8>> {
    (
        prop::sample::select(Reason::ALL.to_vec()),
        any::<prop::sample::Index>(),
        1u8..=255,
        any::<bool>(),
    )
        .prop_map(|(r, at, flip, case)| {
            let mut b = r.as_str().as_bytes().to_vec();
            let i = at.index(b.len());
            if case {
                b[i] = b[i].to_ascii_uppercase();
            } else {
                b[i] ^= flip;
            }
            b
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn adm_50_parse_accepts_only_the_thirteen_names(
        b in prop_oneof![prop::collection::vec(any::<u8>(), 0..=64), near_name()],
    ) {
        let known = names().contains(&b[..]);
        prop_assert_eq!(Reason::parse(&b).is_some(), known);
        if let Some(r) = Reason::parse(&b) {
            prop_assert_eq!(r.as_str().as_bytes(), &b[..]);
        }
    }
}

#[test]
fn adm_50_attempt_outcomes_map_to_their_reasons() {
    for (outcome, reason) in [
        (Outcome::Ok, None),
        (Outcome::ConnectFailed, None),
        (Outcome::RefusedStream, None),
        (Outcome::Reset, None),
        (Outcome::Overloaded, Some(Reason::Overloaded)),
        (Outcome::QueueTimeout, Some(Reason::QueueTimeout)),
        (Outcome::AppRefused, Some(Reason::ConnectFailed)),
        (Outcome::Timeout, None),
    ] {
        assert_eq!(Reason::of_outcome(outcome), reason, "{outcome:?}");
    }
}

#[test]
fn adm_50_trusted_inbound_is_exactly_the_three_refusals() {
    let trusted = Reason::trusted_inbound();
    assert_eq!(
        trusted,
        [
            Reason::ConnectFailed,
            Reason::Overloaded,
            Reason::QueueTimeout
        ]
    );
    for (refusal, reason) in [
        (admission::Refusal::Overloaded, Reason::Overloaded),
        (admission::Refusal::QueueTimeout, Reason::QueueTimeout),
    ] {
        assert_eq!(refusal.reason(), reason);
        assert!(trusted.contains(&refusal.reason()));
    }
    for (refusal, reason) in [
        (retry::Refusal::Overloaded, Reason::Overloaded),
        (retry::Refusal::QueueTimeout, Reason::QueueTimeout),
        (retry::Refusal::AppRefused, Reason::ConnectFailed),
    ] {
        assert_eq!(refusal.reason(), reason);
        assert!(trusted.contains(&refusal.reason()));
    }
}
