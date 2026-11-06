//! SHR-03, SHR-05: the bounded merge with outbound scopes, retention and the split (spec §7.3).

use std::collections::BTreeMap;

use proptest::prelude::*;
use serde_json::{Value, json};

use super::check::Violation;
use super::observed::Observation;
use super::redact::Redactor;
use super::report::{Direction, Exchange, Limits, Report, Via, merge};
use super::scope::ScopeKey;
use super::store::{Kind, Refusal, drop_expired, merge_bounded};

fn limits(max_endpoints: usize, max_authorities: usize) -> Limits {
    Limits {
        max_literals: 8,
        max_endpoints,
        max_authorities,
        sample_budget: 16 << 20,
    }
}

fn exchange(direction: Direction, path: &str, status: u16) -> Exchange<'_> {
    Exchange {
        direction,
        port: 80,
        method: "GET",
        path,
        status,
        head_ms: 1,
        host: None,
        grpc: false,
        observation: Observation::default(),
    }
}

fn inbound(r: &mut Report, l: &Limits, path: &str, times: usize) {
    for _ in 0..times {
        r.record(
            l,
            &Redactor::default(),
            &exchange(Direction::Inbound, path, 200),
        );
    }
}

fn outbound(r: &mut Report, l: &Limits, authority: &str, path: &str, times: usize) {
    for _ in 0..times {
        r.record_at(
            ScopeKey::Authority(authority.into()),
            &Via {
                tls: true,
                sni: Some("sni.example.com"),
                dst: None,
            },
            l,
            &Redactor::default(),
            &exchange(Direction::Outbound, path, 200),
        );
    }
}

fn tunnels(r: &mut Report, l: &Limits, authority: &str, times: usize) {
    for _ in 0..times {
        r.record_tunnel(l, ScopeKey::Authority(authority.into()), "tcp");
    }
}

fn authorities(r: &Report) -> Vec<String> {
    let v: Value = serde_json::from_str(&r.to_json().unwrap()).unwrap();
    v["scopes"]
        .get("outbound")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

fn wide() -> Limits {
    limits(256, 1000)
}

/// Stored: `a:443` and `b:443` with 100 exchanges each.
fn stored_two() -> Report {
    let l = wide();
    let mut r = Report::new(None);
    outbound(&mut r, &l, "a.example.com:443", "/x", 100);
    outbound(&mut r, &l, "b.example.com:443", "/x", 100);
    r
}

#[test]
fn shr_03_a_push_of_many_small_authorities_does_not_crowd_the_stored_ones() {
    let l = limits(256, 2);
    let mut push = Report::new(None);
    for i in 0..50 {
        outbound(
            &mut push,
            &wide(),
            &format!("h{i:02}.example.com:443"),
            "/y",
            1,
        );
    }
    let out = merge_bounded(stored_two(), push, &l);
    assert_eq!(
        authorities(&out.report),
        ["a.example.com:443", "b.example.com:443"]
    );
    assert_eq!(out.refused, BTreeMap::from([(Refusal::AuthorityLimit, 50)]));
    assert!(out.evicted.is_empty());
    // The refused authorities' endpoints and samples are counted nowhere else.
    assert_eq!(out.report.endpoints().len(), 2);
    assert_eq!(out.report.exchanges(), 200 + 50);
    assert_eq!(Refusal::AuthorityLimit.as_str(), "authority_limit");
    assert_eq!(Refusal::AuthorityLimit.kind(), Kind::Authority);
    assert_eq!(Kind::Authority.as_str(), "authority");
}

#[test]
fn shr_03_more_traffic_evicts_the_least_stored_authority_ties_by_key() {
    let l = limits(256, 2);
    // 101 exchanges.
    let mut push = Report::new(None);
    outbound(&mut push, &wide(), "c.example.com:443", "/y", 101);
    let out = merge_bounded(stored_two(), push, &l);
    // `a` and `b` tie at 100: the larger key goes.
    assert_eq!(
        authorities(&out.report),
        ["a.example.com:443", "c.example.com:443"]
    );
    assert_eq!(out.evicted, BTreeMap::from([(Kind::Authority, 1)]));
    assert!(out.refused.is_empty());
    // 101 tunnel counts and no endpoint.
    let mut push = Report::new(None);
    tunnels(&mut push, &wide(), "c.example.com:443", 101);
    let out = merge_bounded(stored_two(), push, &l);
    assert_eq!(
        authorities(&out.report),
        ["a.example.com:443", "c.example.com:443"]
    );
    assert_eq!(out.evicted, BTreeMap::from([(Kind::Authority, 1)]));
    // Traffic of the push adds to a stored authority's.
    let mut push = Report::new(None);
    outbound(&mut push, &wide(), "b.example.com:443", "/x", 5);
    outbound(&mut push, &wide(), "c.example.com:443", "/y", 103);
    let out = merge_bounded(stored_two(), push, &l);
    assert_eq!(
        authorities(&out.report),
        ["b.example.com:443", "c.example.com:443"]
    );
}

#[test]
fn shr_03_a_new_authority_tying_the_lowest_stored_does_not_displace_it() {
    let l = limits(256, 2);
    let mut push = Report::new(None);
    outbound(&mut push, &wide(), "0.example.com:443", "/y", 100);
    let out = merge_bounded(stored_two(), push, &l);
    assert_eq!(
        authorities(&out.report),
        ["a.example.com:443", "b.example.com:443"]
    );
    assert_eq!(out.refused, BTreeMap::from([(Refusal::AuthorityLimit, 1)]));
    assert!(out.evicted.is_empty());
}

#[test]
fn shr_03_endpoint_limits_are_per_direction() {
    let l = limits(3, 64);
    let mut r = Report::new(None);
    for p in ["/a", "/b", "/c"] {
        inbound(&mut r, &wide(), p, 1);
        outbound(&mut r, &wide(), "a.example.com:443", p, 1);
    }
    let out = merge_bounded(Report::new(None), r.clone(), &l);
    assert!(out.refused.is_empty() && out.evicted.is_empty());
    assert_eq!(out.report.endpoints().len(), 6);
    // A fourth outbound endpoint refuses no inbound one.
    let mut more = Report::new(None);
    outbound(&mut more, &wide(), "a.example.com:443", "/d", 1);
    let out = merge_bounded(r.clone(), more, &l);
    assert_eq!(out.refused, BTreeMap::from([(Refusal::EndpointLimit, 1)]));
    let dirs = |r: &Report, d| r.endpoints().iter().filter(|e| e.direction == d).count();
    assert_eq!(dirs(&out.report, Direction::Inbound), 3);
    assert_eq!(dirs(&out.report, Direction::Outbound), 3);
    // And the reverse.
    let mut more = Report::new(None);
    inbound(&mut more, &wide(), "/d", 1);
    let out = merge_bounded(r, more, &l);
    assert_eq!(out.refused, BTreeMap::from([(Refusal::EndpointLimit, 1)]));
    assert_eq!(dirs(&out.report, Direction::Outbound), 3);
}

#[test]
fn shr_03_the_same_input_gives_byte_identical_output() {
    let l = limits(256, 2);
    let mut push = Report::new(None);
    for i in 0..5 {
        outbound(
            &mut push,
            &wide(),
            &format!("h{i}.example.com:443"),
            "/y",
            i + 1,
        );
    }
    let a = merge_bounded(stored_two(), push.clone(), &l);
    let b = merge_bounded(stored_two(), push, &l);
    assert_eq!(a, b);
    assert_eq!(a.report.to_json().unwrap(), b.report.to_json().unwrap());
}

#[derive(Debug, Clone)]
struct Item {
    kind: u8,
    authority: usize,
    path: &'static str,
    times: usize,
}

const AUTHS: [&str; 4] = [
    "a.example.com:443",
    "b.example.com:80",
    "[::1]:8080",
    "10.0.0.1:443",
];

fn item() -> impl Strategy<Value = Item> {
    (
        0u8..3,
        0..AUTHS.len(),
        prop_oneof![Just("/a"), Just("/b"), Just("/u/x"), Just("/u/y")],
        1usize..4,
    )
        .prop_map(|(kind, authority, path, times)| Item {
            kind,
            authority,
            path,
            times,
        })
}

fn build(l: &Limits, items: &[Item]) -> Report {
    let mut r = Report::new(None);
    for i in items {
        match i.kind {
            0 => inbound(&mut r, l, i.path, i.times),
            1 => outbound(&mut r, l, AUTHS[i.authority], i.path, i.times),
            _ => tunnels(&mut r, l, AUTHS[i.authority], i.times),
        }
    }
    r
}

fn split3(items: &[Item], a: usize, b: usize) -> [Vec<Item>; 3] {
    let (a, b) = (a.min(items.len()), b.min(items.len()));
    let (lo, hi) = (a.min(b), a.max(b));
    [
        items[..lo].to_vec(),
        items[lo..hi].to_vec(),
        items[hi..].to_vec(),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    #[test]
    fn shr_03_below_the_limits_the_merge_is_commutative_and_associative(
        items in proptest::collection::vec(item(), 0..12),
        cut1 in 0usize..13,
        cut2 in 0usize..13,
        days in (0u64..50, 0u64..50, 0u64..50),
    ) {
        let l = limits(256, 64);
        let whole = build(&l, &items);
        let [x, y, z] = split3(&items, cut1, cut2).map(|g| build(&l, &g));
        let m = |a: &Report, b: &Report| merge_bounded(a.clone(), b.clone(), &l).report;
        let json = |r: &Report| r.to_json().unwrap();
        prop_assert_eq!(json(&m(&m(&x, &y), &z)), json(&m(&x, &m(&y, &z))));
        prop_assert_eq!(json(&m(&x, &y)), json(&m(&y, &x)));
        prop_assert_eq!(json(&m(&m(&x, &y), &z)), json(&whole));
        prop_assert_eq!(json(&m(&m(&x, &y), &z)), json(&merge(merge(x.clone(), y.clone(), &l), z.clone(), &l)));
        // With stamps: the larger day wins, whatever the order.
        let stamp = |r: &Report, d: u64| { let mut r = r.clone(); r.stamp(d); r };
        let (sx, sy, sz) = (stamp(&x, days.0), stamp(&y, days.1), stamp(&z, days.2));
        prop_assert_eq!(json(&m(&m(&sx, &sy), &sz)), json(&m(&sx, &m(&sy, &sz))));
        prop_assert_eq!(json(&m(&sx, &sy)), json(&m(&sy, &sx)));
        prop_assert_eq!(m(&sx, &sy).check(256, 64), Ok(()));
    }

    #[test]
    fn shr_05_the_union_of_the_split_is_the_merged_report(
        items in proptest::collection::vec(item(), 0..12),
        day in 0u64..50,
    ) {
        let l = limits(256, 64);
        let mut r = build(&l, &items);
        r.stamp(day);
        let (inbound, outbound) = r.split();
        prop_assert_eq!(inbound.version(), 1);
        prop_assert_eq!(inbound.check(256, 64), Ok(()));
        match &outbound {
            Some(o) => {
                prop_assert_eq!(o.version(), 2);
                prop_assert_eq!(o.check(256, 64), Ok(()));
                prop_assert!(o.endpoints().iter().all(|e| e.direction == Direction::Outbound));
            }
            None => prop_assert_eq!(authorities(&r).len(), 0),
        }
        prop_assert!(inbound.endpoints().iter().all(|e| e.direction == Direction::Inbound));
        prop_assert_eq!(Report::join(inbound, outbound), r);
    }
}

#[test]
fn shr_05_stamping_dates_every_outbound_scope() {
    let l = wide();
    let mut r = Report::new(None);
    outbound(&mut r, &l, "a.example.com:443", "/x", 1);
    tunnels(&mut r, &l, "t.example.com:443", 2);
    inbound(&mut r, &l, "/in", 1);
    r.stamp(42);
    let v: Value = serde_json::from_str(&r.to_json().unwrap()).unwrap();
    for key in ["a.example.com:443", "t.example.com:443"] {
        assert_eq!(v["scopes"]["outbound"][key]["last_seen"], 42, "{key}");
    }
    // Inbound scopes carry none: the F7 bytes stay.
    assert!(v["scopes"]["inbound"]["80"].get("last_seen").is_none());
    // A merge keeps the larger value.
    let mut later = Report::new(None);
    tunnels(&mut later, &l, "t.example.com:443", 1);
    later.stamp(50);
    let merged = merge_bounded(r.clone(), later.clone(), &l).report;
    let v: Value = serde_json::from_str(&merged.to_json().unwrap()).unwrap();
    assert_eq!(
        v["scopes"]["outbound"]["t.example.com:443"]["last_seen"],
        50
    );
    let merged = merge_bounded(later, r, &l).report;
    let v: Value = serde_json::from_str(&merged.to_json().unwrap()).unwrap();
    assert_eq!(
        v["scopes"]["outbound"]["t.example.com:443"]["last_seen"],
        50
    );
    // The check accepts a stamped outbound scope and refuses one on an inbound scope.
    assert_eq!(merged.check(256, 64), Ok(()));
    let mut bad = v;
    bad["scopes"]["inbound"]["80"]["last_seen"] = json!(1);
    assert!(Report::parse_checked(&bad.to_string(), 256, 64).is_err());
}

#[test]
fn shr_05_retention_drops_a_scope_only_when_stale_and_without_endpoint() {
    let l = wide();
    // Tunnel-only scope seen at day 10; today 100 with a TTL of 10 days.
    let mut r = Report::new(None);
    tunnels(&mut r, &l, "t.example.com:443", 3);
    r.stamp(10);
    assert!(
        drop_expired(&mut r.clone(), 20, 10).is_empty(),
        "10 + 10 < 20 is false"
    );
    let dropped = drop_expired(&mut r, 100, 10);
    assert_eq!(dropped, BTreeMap::from([(Kind::Authority, 1)]));
    assert!(authorities(&r).is_empty());
    assert_eq!(r.version(), 1);
    assert_eq!(r.check(256, 64), Ok(()));
    // A scope whose own date is old but that holds an endpoint seen later stays.
    let mut r = Report::new(None);
    outbound(&mut r, &l, "a.example.com:443", "/x", 1);
    r.stamp(95);
    let mut v: Value = serde_json::from_str(&r.to_json().unwrap()).unwrap();
    v["scopes"]["outbound"]["a.example.com:443"]["last_seen"] = json!(10);
    let mut r = Report::from_json(&v.to_string()).unwrap();
    assert!(drop_expired(&mut r, 100, 10).is_empty());
    assert_eq!(authorities(&r), ["a.example.com:443"]);
    // Once its endpoint expires too, the scope goes in the same pass.
    let mut r = Report::new(None);
    outbound(&mut r, &l, "a.example.com:443", "/x", 1);
    r.stamp(10);
    let dropped = drop_expired(&mut r, 100, 10);
    assert_eq!(dropped.get(&Kind::Endpoint), Some(&1));
    assert_eq!(dropped.get(&Kind::Authority), Some(&1));
    assert!(authorities(&r).is_empty());
    assert_eq!(r.version(), 1);
}

#[test]
fn shr_05_the_split_equals_what_f7_would_have_stored() {
    let l = wide();
    let day = 20_000;
    let push_in = {
        let mut r = Report::new(Some("p".into()));
        inbound(&mut r, &l, "/in/1", 3);
        inbound(&mut r, &l, "/in/2", 1);
        r
    };
    let push_out = {
        let mut r = push_in.clone();
        outbound(&mut r, &l, "a.example.com:443", "/x", 4);
        tunnels(&mut r, &l, "t.example.com:443", 2);
        r
    };
    let store = |push: Report| {
        let mut push = push;
        push.stamp(day);
        merge_bounded(Report::new(None), push, &l).report
    };
    let f7 = store(push_in);
    let merged = store(push_out);
    let (inbound_part, outbound_part) = merged.split();
    assert_eq!(inbound_part.to_json().unwrap(), f7.to_json().unwrap());
    let outbound_part = outbound_part.unwrap();
    let v: Value = serde_json::from_str(&outbound_part.to_json().unwrap()).unwrap();
    assert_eq!(v["version"], 2);
    assert!(v["scopes"].get("inbound").is_none());
    assert_eq!(Report::join(inbound_part, Some(outbound_part)), merged);
    // Without outbound scopes there is no outbound part.
    let (_, none) = f7.split();
    assert!(none.is_none());
}

/// The `outbound_exchanges` member of a report's JSON, if any.
fn counter(r: &Report) -> Option<Value> {
    let v: Value = serde_json::from_str(&r.to_json().unwrap()).unwrap();
    v.get("outbound_exchanges").cloned()
}

#[test]
fn shr_05_the_recorder_counts_outbound_exchanges_in_version_2_only() {
    let l = wide();
    let mut r = Report::new(None);
    inbound(&mut r, &l, "/in", 2);
    assert_eq!(counter(&r), None);
    assert!(!r.to_json().unwrap().contains("outbound_exchanges"));
    outbound(&mut r, &l, "a.example.com:443", "/x", 3);
    // A tunnel is no exchange.
    tunnels(&mut r, &l, "t.example.com:443", 4);
    assert_eq!(r.exchanges(), 5);
    assert_eq!(counter(&r), Some(json!(3)));
    // Version 2 without an exchange still carries it.
    let mut only_tunnels = Report::new(None);
    tunnels(&mut only_tunnels, &l, "t.example.com:443", 1);
    assert_eq!(counter(&only_tunnels), Some(json!(0)));
}

#[test]
fn shr_05_the_merge_adds_the_counter_and_saturates() {
    let l = wide();
    let mut a = Report::new(None);
    outbound(&mut a, &l, "a.example.com:443", "/x", 2);
    let mut b = Report::new(None);
    inbound(&mut b, &l, "/in", 1);
    outbound(&mut b, &l, "b.example.com:443", "/y", 3);
    let m = merge(a.clone(), b.clone(), &l);
    assert_eq!(counter(&m), Some(json!(5)));
    assert_eq!(m.exchanges(), 6);
    assert_eq!(merge_bounded(a.clone(), b, &l).report, m);
    // At the top of the range.
    let mut v: Value = serde_json::from_str(&a.to_json().unwrap()).unwrap();
    v["exchanges"] = json!(u64::MAX - 1);
    v["outbound_exchanges"] = json!(u64::MAX - 1);
    let big = Report::from_json(&v.to_string()).unwrap();
    let twice = merge(big.clone(), big, &l);
    assert_eq!(counter(&twice), Some(json!(u64::MAX)));
    assert_eq!(twice.exchanges(), u64::MAX);
}

#[test]
fn shr_05_the_check_bounds_the_counter() {
    let l = wide();
    let mut r = Report::new(None);
    inbound(&mut r, &l, "/in", 1);
    outbound(&mut r, &l, "a.example.com:443", "/x", 2);
    let mut v: Value = serde_json::from_str(&r.to_json().unwrap()).unwrap();
    assert_eq!(Report::parse_checked(&v.to_string(), 256, 64), Ok(r));
    // Over `exchanges`.
    v["outbound_exchanges"] = json!(4);
    assert_eq!(
        Report::parse_checked(&v.to_string(), 256, 64),
        Err(Violation::Malformed)
    );
    // Absent in version 2 reads as 0.
    v.as_object_mut().unwrap().remove("outbound_exchanges");
    assert!(Report::parse_checked(&v.to_string(), 256, 64).is_ok());
    // A version 1 report carries none.
    let mut v1: Value = serde_json::from_str(&{
        let mut r = Report::new(None);
        inbound(&mut r, &l, "/in", 2);
        r.to_json().unwrap()
    })
    .unwrap();
    assert!(Report::parse_checked(&v1.to_string(), 256, 64).is_ok());
    v1["outbound_exchanges"] = json!(1);
    assert_eq!(
        Report::parse_checked(&v1.to_string(), 256, 64),
        Err(Violation::Malformed)
    );
}

#[test]
fn shr_05_the_split_is_exact_after_evictions() {
    let l = limits(256, 1);
    // Stored: 3 inbound, 5 + 2 outbound exchanges in two authorities.
    let mut push = Report::new(None);
    inbound(&mut push, &wide(), "/in", 3);
    outbound(&mut push, &wide(), "a.example.com:443", "/x", 5);
    outbound(&mut push, &wide(), "b.example.com:443", "/y", 2);
    let out = merge_bounded(Report::new(None), push, &l);
    assert_eq!(out.evicted.len() + out.refused.len(), 1);
    assert_eq!(authorities(&out.report), ["a.example.com:443"]);
    // All 10 exchanges stay counted; the evicted ones stay on the outbound side.
    let (inbound_part, outbound_part) = out.report.split();
    assert_eq!(inbound_part.exchanges(), 3);
    assert_eq!(counter(&inbound_part), None);
    let outbound_part = outbound_part.unwrap();
    assert_eq!(outbound_part.exchanges(), 7);
    assert_eq!(counter(&outbound_part), Some(json!(7)));
    assert_eq!(outbound_part.check(256, 64), Ok(()));
    assert_eq!(Report::join(inbound_part, Some(outbound_part)), out.report);
}

#[test]
fn shr_05_the_split_is_exact_when_every_outbound_scope_expired() {
    let l = wide();
    let mut r = Report::new(None);
    inbound(&mut r, &l, "/in", 3);
    outbound(&mut r, &l, "a.example.com:443", "/x", 5);
    r.stamp(10);
    // Keep the inbound endpoint fresh: only the outbound scope is stale.
    let mut fresh = Report::new(None);
    inbound(&mut fresh, &l, "/in", 1);
    fresh.stamp(100);
    r.merge(&l, fresh);
    drop_expired(&mut r, 100, 5);
    assert!(authorities(&r).is_empty());
    // The expired outbound exchanges leave with their scope: a valid version 1 report.
    assert_round_trip_is_inbound_only(&r, 4);
}

/// A report without outbound scope: version 1, no counter, and the same after a store
/// round trip, `check` and `split`, with `inbound` exchanges all counted as inbound.
fn assert_round_trip_is_inbound_only(r: &Report, inbound: u64) {
    assert_eq!(r.version(), 1);
    assert_eq!(r.exchanges(), inbound);
    assert_eq!(r.outbound_exchanges(), 0);
    assert_eq!(r.check(256, 64), Ok(()));
    assert_eq!(counter(r), None);
    let json = r.to_json().unwrap();
    assert!(!json.contains("outbound_exchanges"));
    let back = Report::from_json(&json).unwrap();
    assert_eq!(&back, r);
    assert_eq!(back.to_json().unwrap(), json);
    assert_eq!(Report::parse_checked(&json, 256, 64).as_ref(), Ok(r));
    let (inbound_part, outbound_part) = back.split();
    assert!(outbound_part.is_none());
    assert_eq!(inbound_part.exchanges(), inbound);
    assert_eq!(inbound_part.check(256, 64), Ok(()));
    assert_eq!(Report::join(inbound_part, None), *r);
}

#[test]
fn shr_05_a_report_that_loses_every_outbound_scope_by_eviction_is_valid_version_1() {
    let mut push = Report::new(None);
    inbound(&mut push, &wide(), "/in", 3);
    outbound(&mut push, &wide(), "a.example.com:443", "/x", 5);
    outbound(&mut push, &wide(), "b.example.com:443", "/y", 2);
    let out = merge_bounded(Report::new(None), push, &limits(256, 0));
    assert_eq!(out.evicted.len() + out.refused.len(), 1);
    assert!(authorities(&out.report).is_empty());
    assert_round_trip_is_inbound_only(&out.report, 3);
    // The same from a stored report with outbound data.
    let mut stored = Report::new(None);
    inbound(&mut stored, &wide(), "/in", 3);
    outbound(&mut stored, &wide(), "a.example.com:443", "/x", 5);
    let mut more = Report::new(None);
    inbound(&mut more, &wide(), "/in", 1);
    let out = merge_bounded(stored, more, &limits(256, 0));
    assert!(authorities(&out.report).is_empty());
    assert_round_trip_is_inbound_only(&out.report, 4);
}

#[test]
fn shr_05_removing_the_last_authority_resets_the_counter() {
    let l = wide();
    let mut r = Report::new(None);
    inbound(&mut r, &l, "/in", 3);
    outbound(&mut r, &l, "a.example.com:443", "/x", 5);
    outbound(&mut r, &l, "b.example.com:443", "/x", 2);
    r.remove_authority(&ScopeKey::Authority("a.example.com:443".into()));
    // One scope left: still version 2, the counter untouched.
    assert_eq!(
        (r.version(), r.exchanges(), r.outbound_exchanges()),
        (2, 10, 7)
    );
    r.remove_authority(&ScopeKey::Authority("b.example.com:443".into()));
    assert_round_trip_is_inbound_only(&r, 3);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1_000))]

    #[test]
    fn shr_05_the_merge_with_the_counter_is_associative_and_valid(
        items in proptest::collection::vec(item(), 0..12),
        cut1 in 0usize..13,
        cut2 in 0usize..13,
    ) {
        let l = limits(256, 64);
        let [x, y, z] = split3(&items, cut1, cut2).map(|g| build(&l, &g));
        let m = |a: &Report, b: &Report| merge_bounded(a.clone(), b.clone(), &l).report;
        let (left, right) = (m(&m(&x, &y), &z), m(&x, &m(&y, &z)));
        prop_assert_eq!(left.outbound_exchanges(), right.outbound_exchanges());
        prop_assert_eq!(left.exchanges(), right.exchanges());
        let outbound_n: u64 = items.iter().filter(|i| i.kind == 1).map(|i| i.times as u64).sum();
        prop_assert_eq!(left.outbound_exchanges(), outbound_n);
        // Whatever remains, the report survives a store round trip with its counter.
        prop_assert_eq!(left.check(256, 64), Ok(()));
        prop_assert_eq!(Report::from_json(&left.to_json().unwrap()).unwrap(), left.clone());
        // Dropping every outbound scope (any order) leaves the inbound exchanges exact.
        let mut dropped = left.clone();
        for key in dropped.outbound_keys() {
            dropped.remove_authority(&key);
        }
        let inbound_n: u64 = items.iter().filter(|i| i.kind == 0).map(|i| i.times as u64).sum();
        prop_assert_eq!(dropped.exchanges(), inbound_n);
        prop_assert_eq!(dropped.outbound_exchanges(), 0);
        prop_assert_eq!(dropped.check(256, 64), Ok(()));
    }

    #[test]
    fn shr_05_the_parts_of_a_split_count_each_direction(
        items in proptest::collection::vec(item(), 0..12),
    ) {
        let l = wide();
        let r = build(&l, &items);
        let inbound_n: u64 = items.iter().filter(|i| i.kind == 0).map(|i| i.times as u64).sum();
        let outbound_n: u64 = items.iter().filter(|i| i.kind == 1).map(|i| i.times as u64).sum();
        let (inbound_part, outbound_part) = r.split();
        prop_assert_eq!(inbound_part.exchanges(), inbound_n);
        prop_assert_eq!(outbound_part.map_or(0, |o| o.exchanges()), outbound_n);
    }
}
