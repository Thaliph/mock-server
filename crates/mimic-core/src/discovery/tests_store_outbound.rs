//! SHR-03, SHR-05: the bounded merge with outbound scopes, retention and the split (spec §7.3).

use std::collections::BTreeMap;

use proptest::prelude::*;
use serde_json::{Value, json};

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
