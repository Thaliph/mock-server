//! CTL-02: the bounded merge of the controller store (spec §7.3).

use std::collections::BTreeMap;

use proptest::prelude::*;
use serde_json::json;

use super::observed::Observation;
use super::redact::Redactor;
use super::report::{Direction, Exchange, Limits, Report, merge};
use super::store::{Bounded, Kind, Refusal, merge_bounded};

fn limits(max_literals: usize, max_endpoints: usize, sample_budget: usize) -> Limits {
    Limits {
        max_literals,
        max_endpoints,
        sample_budget,
    }
}

/// Records `times` exchanges of `GET path` with a response of `status` and the query `query`.
fn rec(r: &mut Report, l: &Limits, path: &str, status: u16, query: &str, times: usize) {
    let body = json!({"k": "v"}).to_string();
    let headers = [("content-type", "application/json")];
    for _ in 0..times {
        r.record(
            l,
            &Redactor::default(),
            &Exchange {
                direction: Direction::Inbound,
                port: 80,
                method: "GET",
                path,
                status,
                head_ms: 1,
                host: Some("svc"),
                grpc: false,
                observation: Observation {
                    query,
                    resp_headers: &headers,
                    resp_body: Some(body.as_bytes()),
                    ..Observation::default()
                },
            },
        );
    }
}

fn report(l: &Limits, items: &[(&str, u16, &str, usize)]) -> Report {
    let mut r = Report::new(None);
    for (path, status, query, times) in items {
        rec(&mut r, l, path, *status, query, *times);
    }
    r
}

fn templates(r: &Report) -> Vec<String> {
    r.endpoints().into_iter().map(|e| e.template).collect()
}

fn exchanges_of(r: &Report, template: &str) -> u64 {
    r.endpoints()
        .into_iter()
        .find(|e| e.template == template)
        .map_or(0, |e| e.stats.statuses.values().sum())
}

/// Samples of the endpoint at `template`, by status class.
fn classes(r: &Report, template: &str) -> BTreeMap<u16, usize> {
    let mut out = BTreeMap::new();
    for e in r.endpoints().into_iter().filter(|e| e.template == template) {
        for (_, s) in e.stats.samples().iter() {
            *out.entry(s.status() / 100).or_default() += 1;
        }
    }
    out
}

fn counts<K: Ord + Copy>(items: &[(K, u64)]) -> BTreeMap<K, u64> {
    items.iter().copied().collect()
}

fn ids(bounded: &Bounded) -> (String, BTreeMap<Refusal, u64>, BTreeMap<Kind, u64>) {
    (
        bounded.report.to_json().unwrap(),
        bounded.refused.clone(),
        bounded.evicted.clone(),
    )
}

#[derive(Debug, Clone)]
struct Item {
    path: &'static str,
    status: u16,
    query: &'static str,
    times: usize,
}

fn item() -> impl Strategy<Value = Item> {
    (
        prop_oneof![
            Just("/a"),
            Just("/b"),
            Just("/u/x"),
            Just("/u/y"),
            Just("/u/z"),
            Just("/u/7"),
            Just("/c/d/e"),
        ],
        prop_oneof![Just(200u16), Just(201), Just(400), Just(500)],
        prop_oneof![Just(""), Just("q=1"), Just("q=2"), Just("q=3")],
        1usize..4,
    )
        .prop_map(|(path, status, query, times)| Item {
            path,
            status,
            query,
            times,
        })
}

fn build(l: &Limits, items: &[Item]) -> Report {
    let mut r = Report::new(None);
    for i in items {
        rec(&mut r, l, i.path, i.status, i.query, i.times);
    }
    r
}

fn split3(items: &[Item], a: usize, b: usize) -> [Vec<Item>; 3] {
    let a = a.min(items.len());
    let b = b.min(items.len()).max(a);
    [
        items[..a].to_vec(),
        items[a..b].to_vec(),
        items[b..].to_vec(),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn ctl_02_equals_the_section_6_merge_within_the_limits(
        left in proptest::collection::vec(item(), 0..8),
        right in proptest::collection::vec(item(), 0..8),
        max_literals in 1usize..5,
    ) {
        let l = limits(max_literals, 256, 16 << 20);
        let (a, b) = (build(&l, &left), build(&l, &right));
        let bounded = merge_bounded(a.clone(), b.clone(), &l);
        prop_assert_eq!(bounded.report, merge(a, b, &l));
        prop_assert!(bounded.refused.is_empty());
        prop_assert!(bounded.evicted.is_empty());
    }

    #[test]
    fn ctl_02_does_not_depend_on_order_or_grouping_below_the_limits(
        items in proptest::collection::vec(item(), 0..12),
        cut1 in 0usize..13,
        cut2 in 0usize..13,
        max_literals in 1usize..5,
    ) {
        let l = limits(max_literals, 256, 16 << 20);
        let whole = build(&l, &items);
        let [x, y, z] = split3(&items, cut1, cut2).map(|g| build(&l, &g));
        let m = |a: &Report, b: &Report| merge_bounded(a.clone(), b.clone(), &l).report;
        let left = m(&m(&x, &y), &z);
        let right = m(&x, &m(&y, &z));
        prop_assert_eq!(left.to_json().unwrap(), right.to_json().unwrap());
        prop_assert_eq!(left.to_json().unwrap(), whole.to_json().unwrap());
        prop_assert_eq!(m(&z, &x).to_json().unwrap(), m(&x, &z).to_json().unwrap());
        prop_assert_eq!(m(&y, &x).to_json().unwrap(), m(&x, &y).to_json().unwrap());
    }

    #[test]
    fn ctl_02_respects_the_limits_and_is_deterministic(
        left in proptest::collection::vec(item(), 0..10),
        right in proptest::collection::vec(item(), 0..10),
        max_endpoints in 1usize..6,
        extra in 0usize..2000,
    ) {
        // A budget that holds what is stored, so that only the push is limited.
        let wide = limits(3, 256, 16 << 20);
        let stored = build(&wide, &left);
        let l = limits(3, max_endpoints, stored.sample_bytes() + extra);
        let stored = merge_bounded(Report::new(None), stored, &l).report;
        let push = build(&wide, &right);
        let one = merge_bounded(stored.clone(), push.clone(), &l);
        let two = merge_bounded(stored, push, &l);
        prop_assert_eq!(ids(&one), ids(&two));
        prop_assert!(one.report.endpoints().len() <= max_endpoints);
        prop_assert!(one.report.sample_bytes() <= l.sample_budget);
        prop_assert_eq!(one.report.check(256), Ok(()));
    }
}

#[test]
fn ctl_02_two_pushes_below_the_literal_limit_absorb_when_their_union_exceeds_it() {
    let l = limits(3, 256, 16 << 20);
    let a = report(&l, &[("/u/a", 200, "", 1), ("/u/b", 200, "", 1)]);
    let b = report(&l, &[("/u/c", 500, "", 1), ("/u/d", 200, "", 1)]);
    assert_eq!(templates(&a), ["/u/a", "/u/b"]);
    assert_eq!(templates(&b), ["/u/c", "/u/d"]);
    let m = merge_bounded(a, b, &l).report;
    assert_eq!(templates(&m), ["/u/{u_id}"]);
    let stats = m.endpoints()[0].stats;
    assert_eq!(stats.statuses.get(&200), Some(&3));
    assert_eq!(stats.statuses.get(&500), Some(&1));
}

#[test]
fn ctl_02_counters_and_buckets_saturate() {
    let l = limits(32, 256, 16 << 20);
    let r = report(&l, &[("/a", 200, "", 1)]);
    let json = r.to_json().unwrap();
    let max = json
        .replace("\"200\":1", &format!("\"200\":{}", u64::MAX))
        .replace(
            "\"buckets\":{\"0\":1}",
            &format!("\"buckets\":{{\"0\":{}}}", u64::MAX),
        )
        .replace("\"exchanges\":1", &format!("\"exchanges\":{}", u64::MAX))
        .replace("\"count\":1", &format!("\"count\":{}", u64::MAX));
    assert_ne!(json, max);
    let big = Report::from_json(&max).unwrap();
    let merged = merge_bounded(big.clone(), big, &l).report;
    let stats = merged.endpoints()[0].stats;
    assert_eq!(stats.statuses.get(&200), Some(&u64::MAX));
    assert_eq!(stats.latency.buckets().get(&0), Some(&u64::MAX));
    assert_eq!(stats.latency.count(), u64::MAX);
    assert_eq!(merged.exchanges(), u64::MAX);
    assert!(merged.to_json().unwrap().contains(&u64::MAX.to_string()));
    // Ranking by traffic does not overflow either.
    let tight = limits(32, 1, 16 << 20);
    let again = merge_bounded(merged.clone(), merged, &tight);
    assert_eq!(again.report.endpoints().len(), 1);
}

#[test]
fn ctl_02_the_result_has_no_pod() {
    let l = limits(32, 256, 16 << 20);
    let mut a = Report::new(Some("p1".into()));
    rec(&mut a, &l, "/a", 200, "", 1);
    let mut b = a.clone();
    rec(&mut b, &l, "/b", 200, "", 1);
    let same = merge_bounded(a.clone(), b, &l).report;
    assert!(same.to_json().unwrap().contains("\"pod\":null"));
    let from_none = merge_bounded(Report::new(None), a, &l).report;
    assert!(from_none.to_json().unwrap().contains("\"pod\":null"));
}

fn four_hundreds(l: &Limits) -> Report {
    report(
        l,
        &[
            ("/s1", 200, "", 100),
            ("/s2", 200, "", 100),
            ("/s3", 200, "", 100),
            ("/s4", 200, "", 100),
        ],
    )
}

#[test]
fn ctl_02_noise_does_not_crowd_out_a_full_store() {
    let wide = limits(1000, 256, 16 << 20);
    let l = limits(1000, 4, 16 << 20);
    let stored = four_hundreds(&wide);
    let paths: Vec<String> = (0..50).map(|i| format!("/!{i}")).collect();
    let noise = report(
        &wide,
        &paths
            .iter()
            .map(|p| (p.as_str(), 200, "", 1))
            .collect::<Vec<_>>(),
    );
    assert_eq!(noise.endpoints().len(), 50);
    let out = merge_bounded(stored.clone(), noise, &l);
    assert_eq!(templates(&out.report), ["/s1", "/s2", "/s3", "/s4"]);
    assert_eq!(out.refused, counts(&[(Refusal::EndpointLimit, 50)]));
    assert!(out.evicted.is_empty());
    // The same push again changes nothing.
    assert_eq!(out.report.endpoints(), stored.endpoints());
}

#[test]
fn ctl_02_a_busier_endpoint_evicts_the_quietest_stored_one() {
    let wide = limits(1000, 256, 16 << 20);
    let l = limits(1000, 4, 16 << 20);
    let hot = report(&wide, &[("/hot", 200, "", 101)]);
    let out = merge_bounded(four_hundreds(&wide), hot, &l);
    // All stored endpoints tie: the largest key goes.
    assert_eq!(templates(&out.report), ["/hot", "/s1", "/s2", "/s3"]);
    assert_eq!(out.evicted, counts(&[(Kind::Endpoint, 1)]));
    assert!(out.refused.is_empty());
    assert_eq!(exchanges_of(&out.report, "/hot"), 101);
}

#[test]
fn ctl_02_the_fewest_exchanges_go_first_then_the_larger_key() {
    let wide = limits(1000, 256, 16 << 20);
    let l = limits(1000, 3, 16 << 20);
    let stored = report(
        &wide,
        &[("/a", 200, "", 5), ("/b", 200, "", 2), ("/c", 200, "", 2)],
    );
    let push = report(&wide, &[("/z", 200, "", 3)]);
    let out = merge_bounded(stored, push, &l);
    assert_eq!(templates(&out.report), ["/a", "/b", "/z"]);
    assert_eq!(out.evicted, counts(&[(Kind::Endpoint, 1)]));
}

#[test]
fn ctl_02_a_new_endpoint_tying_the_lowest_stored_count_does_not_displace_it() {
    let wide = limits(1000, 256, 16 << 20);
    let l = limits(1000, 4, 16 << 20);
    // "/a0" sorts before every stored key, yet the stored endpoints win the tie.
    let push = report(&wide, &[("/a0", 200, "", 100)]);
    let out = merge_bounded(four_hundreds(&wide), push, &l);
    assert_eq!(templates(&out.report), ["/s1", "/s2", "/s3", "/s4"]);
    assert_eq!(out.refused, counts(&[(Refusal::EndpointLimit, 1)]));
    assert!(out.evicted.is_empty());
}

#[test]
fn ctl_02_a_push_on_a_stored_endpoint_counts_towards_its_rank() {
    let wide = limits(1000, 256, 16 << 20);
    let l = limits(1000, 2, 16 << 20);
    let stored = report(&wide, &[("/a", 200, "", 3), ("/b", 200, "", 3)]);
    // /b becomes 3 + 5 and outranks /a; /c (4) is between them and drops /a.
    let push = report(&wide, &[("/b", 200, "", 5), ("/c", 200, "", 4)]);
    let out = merge_bounded(stored, push, &l);
    assert_eq!(templates(&out.report), ["/b", "/c"]);
    assert_eq!(out.evicted, counts(&[(Kind::Endpoint, 1)]));
    assert!(out.refused.is_empty());
}

#[test]
fn ctl_02_the_endpoint_limit_is_per_direction() {
    let wide = limits(1000, 256, 16 << 20);
    let l = limits(1000, 1, 16 << 20);
    let mut r = Report::new(None);
    for (direction, path) in [
        (Direction::Inbound, "/a"),
        (Direction::Inbound, "/b"),
        (Direction::Outbound, "/c"),
        (Direction::Outbound, "/d"),
    ] {
        r.record(
            &wide,
            &Redactor::default(),
            &Exchange {
                direction,
                port: 80,
                method: "GET",
                path,
                status: 200,
                head_ms: 1,
                host: None,
                grpc: false,
                observation: Observation::default(),
            },
        );
    }
    let out = merge_bounded(Report::new(None), r, &l);
    assert_eq!(templates(&out.report), ["/a", "/c"]);
    assert_eq!(out.refused, counts(&[(Refusal::EndpointLimit, 2)]));
}

/// One endpoint `/e` with three 200 samples and a budget that exactly holds them.
fn full_budget() -> (Limits, Report, usize) {
    let wide = limits(32, 256, 16 << 20);
    let stored = report(
        &wide,
        &[
            ("/e", 200, "q=1", 1),
            ("/e", 200, "q=2", 1),
            ("/e", 200, "q=3", 1),
        ],
    );
    let bytes = stored.sample_bytes();
    (limits(32, 256, bytes), stored, bytes)
}

#[test]
fn ctl_02_a_new_class_evicts_from_the_most_populated_class() {
    let (l, stored, bytes) = full_budget();
    let wide = limits(32, 256, 16 << 20);
    let push = report(&wide, &[("/e", 500, "q=1", 1)]);
    assert_eq!(classes(&stored, "/e"), BTreeMap::from([(2, 3)]));
    let out = merge_bounded(stored, push, &l);
    assert_eq!(classes(&out.report, "/e"), BTreeMap::from([(2, 2), (5, 1)]));
    assert_eq!(out.evicted, counts(&[(Kind::Sample, 1)]));
    assert!(out.refused.is_empty());
    assert!(out.report.sample_bytes() <= bytes);
    // Counters take the refused or evicted exchanges in all cases.
    let e = &out.report.endpoints()[0];
    assert_eq!(e.stats.statuses.get(&200), Some(&3));
    assert_eq!(e.stats.statuses.get(&500), Some(&1));
}

#[test]
fn ctl_02_a_sample_of_a_class_already_held_is_refused_on_a_full_budget() {
    let (l, stored, _) = full_budget();
    let wide = limits(32, 256, 16 << 20);
    let push = report(&wide, &[("/e", 200, "q=4", 1)]);
    let out = merge_bounded(stored.clone(), push, &l);
    assert_eq!(out.refused, counts(&[(Refusal::Budget, 1)]));
    assert!(out.evicted.is_empty());
    assert_eq!(classes(&out.report, "/e"), BTreeMap::from([(2, 3)]));
    assert_eq!(
        out.report.endpoints()[0].stats.samples(),
        stored.endpoints()[0].stats.samples()
    );
    assert_eq!(exchanges_of(&out.report, "/e"), 4);
}

#[test]
fn ctl_02_a_class_is_never_emptied_and_nothing_is_evicted_when_room_cannot_be_made() {
    let wide = limits(32, 256, 16 << 20);
    let stored = report(&wide, &[("/e", 200, "q=1", 1), ("/e", 400, "q=1", 1)]);
    assert_eq!(classes(&stored, "/e"), BTreeMap::from([(2, 1), (4, 1)]));
    let l = limits(32, 256, stored.sample_bytes());
    let push = report(&wide, &[("/e", 500, "q=1", 1)]);
    let out = merge_bounded(stored.clone(), push, &l);
    assert_eq!(out.refused, counts(&[(Refusal::Budget, 1)]));
    assert!(out.evicted.is_empty());
    assert_eq!(classes(&out.report, "/e"), BTreeMap::from([(2, 1), (4, 1)]));
    assert_eq!(
        out.report.endpoints()[0].stats.samples(),
        stored.endpoints()[0].stats.samples()
    );
}

#[test]
fn ctl_02_eviction_takes_from_the_most_populated_pair_of_any_endpoint() {
    let wide = limits(32, 256, 16 << 20);
    let stored = report(
        &wide,
        &[
            ("/big", 200, "q=1", 1),
            ("/big", 200, "q=2", 1),
            ("/big", 200, "q=3", 1),
            ("/small", 200, "q=1", 1),
            ("/small", 200, "q=2", 1),
            ("/new", 200, "q=1", 1),
        ],
    );
    let l = limits(32, 256, stored.sample_bytes());
    let push = report(&wide, &[("/new", 500, "q=1", 1)]);
    let out = merge_bounded(stored, push, &l);
    assert_eq!(out.evicted, counts(&[(Kind::Sample, 1)]));
    assert_eq!(classes(&out.report, "/big"), BTreeMap::from([(2, 2)]));
    assert_eq!(classes(&out.report, "/small"), BTreeMap::from([(2, 2)]));
    assert_eq!(
        classes(&out.report, "/new"),
        BTreeMap::from([(2, 1), (5, 1)])
    );
}

#[test]
fn ctl_02_the_output_is_byte_identical_for_the_same_input() {
    let (l, stored, _) = full_budget();
    let wide = limits(32, 256, 16 << 20);
    let push = report(
        &wide,
        &[
            ("/e", 500, "q=1", 1),
            ("/f", 200, "q=1", 3),
            ("/e", 200, "q=9", 1),
        ],
    );
    let first = merge_bounded(stored.clone(), push.clone(), &l);
    for _ in 0..3 {
        let again = merge_bounded(stored.clone(), push.clone(), &l);
        assert_eq!(ids(&first), ids(&again));
    }
}

/// A report whose samples claim `bytes` (a forged push; the check would refuse it).
fn forged(l: &Limits, bytes: u64) -> Report {
    let r = report(l, &[("/e", 200, "q=1", 1), ("/e", 500, "q=1", 1)]);
    let json = r.to_json().unwrap();
    let real = r.sample_bytes() / 2;
    let json = json.replace(
        &format!("\"bytes\":{real}}}"),
        &format!("\"bytes\":{bytes}}}"),
    );
    assert_ne!(json, r.to_json().unwrap());
    Report::from_json(&json).unwrap()
}

#[test]
fn ctl_02_forged_sample_bytes_do_not_overflow_the_sums() {
    let wide = limits(32, 256, 16 << 20);
    let tight = limits(32, 256, 1000);
    for bytes in [0, u64::MAX] {
        let a = forged(&wide, bytes);
        let b = forged(&wide, bytes);
        let _ = a.sample_bytes();
        let _ = merge(a.clone(), b.clone(), &tight);
        let _ = merge_bounded(a, b, &tight);
    }
    // A max-sized claim is never admitted past a budget.
    let out = merge_bounded(Report::new(None), forged(&wide, u64::MAX), &tight);
    assert!(out.report.sample_bytes() <= tight.sample_budget);
}
