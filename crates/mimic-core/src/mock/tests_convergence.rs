//! MCK-07: mode convergence from the `:pulls` fields (spec §7.4).

use std::collections::BTreeMap;

use super::*;
use crate::Mode;

const NOW: u64 = 1_000_000;
const WINDOW: u64 = 10_000;

fn pulls(fields: &[(&str, &str)]) -> BTreeMap<String, String> {
    fields
        .iter()
        .map(|(pod, field)| (pod.to_string(), field.to_string()))
        .collect()
}

#[test]
fn mck_07_pods_carrying_the_effective_mode_and_the_other_one_are_counted() {
    let p = pulls(&[
        ("a", "mock 999000"),
        ("b", "mock 995000"),
        ("c", "discovery 999500"),
    ]);
    let c = convergence(Mode::Mock, &p, WINDOW, NOW);
    assert_eq!((c.pods, c.other_pods, c.unreadable), (2, 1, 0));
    let c = convergence(Mode::Discovery, &p, WINDOW, NOW);
    assert_eq!((c.pods, c.other_pods), (1, 2));
}

#[test]
fn mck_07_the_window_edge_counts_and_older_fields_are_in_neither_count() {
    let p = pulls(&[
        ("edge", "mock 990000"),
        ("old", "mock 989999"),
        ("old-other", "discovery 100"),
    ]);
    let c = convergence(Mode::Mock, &p, WINDOW, NOW);
    assert_eq!((c.pods, c.other_pods, c.unreadable), (1, 0, 0));
}

#[test]
fn mck_07_unreadable_fields_are_in_neither_count_and_reported() {
    let p = pulls(&[
        ("ok", "mock 999000"),
        ("nospace", "mock"),
        ("badmode", "turbo 999000"),
        ("badms", "mock soon"),
        ("empty", ""),
        ("future", "mock 2000000"),
    ]);
    let c = convergence(Mode::Mock, &p, WINDOW, NOW);
    assert_eq!((c.pods, c.other_pods, c.unreadable), (1, 0, 5));
}

#[test]
fn mck_07_a_little_clock_skew_ahead_still_counts() {
    let p = pulls(&[("a", "mock 1030000"), ("b", "discovery 1060000")]);
    let c = convergence(Mode::Mock, &p, WINDOW, NOW);
    assert_eq!((c.pods, c.other_pods, c.unreadable), (1, 1, 0));
}

#[test]
fn mck_07_no_fields_give_zero_counts() {
    let c = convergence(Mode::Mock, &BTreeMap::new(), WINDOW, NOW);
    assert_eq!(c, Convergence::default());
}
