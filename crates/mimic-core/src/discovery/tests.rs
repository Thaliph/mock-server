use proptest::prelude::*;

use super::histogram::{Histogram, OVERFLOW, bucket_index};
use super::report::{Direction, Exchange, Limits, Report, merge};
use super::template::is_param;

fn ex<'a>(path: &'a str, status: u16, head_ms: u64) -> Exchange<'a> {
    Exchange {
        direction: Direction::Inbound,
        port: 8080,
        method: "GET",
        path,
        status,
        head_ms,
        host: None,
        grpc: false,
    }
}

fn limits(max_literals: usize) -> Limits {
    Limits { max_literals }
}

fn report_of(l: &Limits, exchanges: &[Exchange<'_>]) -> Report {
    let mut r = Report::new(Some("p".into()));
    for e in exchanges {
        r.record(l, e);
    }
    r
}

fn templates(r: &Report) -> Vec<String> {
    r.endpoints().into_iter().map(|e| e.template).collect()
}

fn template_of(path: &str) -> String {
    let r = report_of(&limits(32), &[ex(path, 200, 1)]);
    templates(&r).remove(0)
}

#[test]
fn dsc_01_params() {
    let params = [
        "42",
        "123e4567-e89b-12d3-a456-426614174000",
        "123E4567-E89B-12D3-A456-426614174000",
        "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "5f2b9c1d",
        "2026-11-01",
        "2026-11-01T10:00:00Z",
        "2026-11-01T10:00:00.5+02:00",
        "a1b2c3d4e5f6g7h8i9",
        "alice@example.com",
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2ln",
    ];
    for p in params {
        assert!(is_param(p), "{p} should be a parameter");
    }
    let literals = [
        "v1",
        "api",
        "users",
        "deadbeef",
        "health",
        "",
        "%41",
        "2026-13-01",
    ];
    for l in literals {
        assert!(!is_param(l), "{l:?} should be a literal");
    }
}

#[test]
fn dsc_01_names() {
    assert_eq!(
        template_of("/users/42/orders/7"),
        "/users/{users_id}/orders/{orders_id}"
    );
    assert_eq!(template_of("/42"), "/{param1}");
    assert_eq!(template_of("/a/1/a/2"), "/a/{a_id}/a/{param4}");
    assert_eq!(template_of("/Foo-Bar/1"), "/Foo-Bar/{foo_bar_id}");
    assert_eq!(template_of("/users/"), "/users/");
    assert_eq!(template_of("/"), "/");
    assert_eq!(template_of("/v1/%41"), "/v1/%41");
}

#[test]
fn dsc_01_grpc_never_templated() {
    let mut r = Report::new(None);
    let mut e = ex("/pkg.Svc/Get/42", 200, 1);
    e.grpc = true;
    r.record(&limits(1), &e);
    e.path = "/pkg.Svc/Get/43";
    r.record(&limits(1), &e);
    assert_eq!(templates(&r), ["/pkg.Svc/Get/42", "/pkg.Svc/Get/43"]);
}

#[test]
fn dsc_02_absorbing() {
    let l = limits(3);
    let r = report_of(
        &l,
        &[
            ex("/u/a", 200, 1),
            ex("/u/b", 200, 1),
            ex("/u/c", 200, 1),
            ex("/x/a", 200, 1),
        ],
    );
    assert_eq!(templates(&r), ["/u/a", "/u/b", "/u/c", "/x/a"]);

    let r = report_of(
        &l,
        &[
            ex("/u/a", 200, 1),
            ex("/u/b", 200, 1),
            ex("/u/c", 404, 1),
            ex("/u/d", 200, 1),
            ex("/x/a", 200, 1),
        ],
    );
    assert_eq!(templates(&r), ["/u/{u_id}", "/x/a"]);
    let eps = r.endpoints();
    assert_eq!(eps[0].stats.statuses.get(&200), Some(&3));
    assert_eq!(eps[0].stats.statuses.get(&404), Some(&1));
    assert_eq!(eps[0].stats.latency.count(), 4);

    // Later literals and a parameter share the absorbed node.
    let mut r = r;
    r.record(&l, &ex("/u/zzz", 200, 1));
    r.record(&l, &ex("/u/42", 200, 1));
    assert_eq!(templates(&r), ["/u/{u_id}", "/x/a"]);
    assert_eq!(r.endpoints()[0].stats.latency.count(), 6);

    // A parameter before the literals joins them on absorption.
    let r = report_of(
        &l,
        &[
            ex("/u/42", 200, 1),
            ex("/u/a", 200, 1),
            ex("/u/b", 200, 1),
            ex("/u/c", 200, 1),
            ex("/u/d", 200, 1),
        ],
    );
    assert_eq!(templates(&r), ["/u/{u_id}"]);
    assert_eq!(r.endpoints()[0].stats.latency.count(), 5);
}

#[test]
fn dsc_02_absorption_recurses() {
    let l = limits(2);
    let r = report_of(
        &l,
        &[
            ex("/a/x/1", 200, 1),
            ex("/a/y/2", 200, 1),
            ex("/a/x/p", 200, 1),
            ex("/a/y/q", 200, 1),
            ex("/a/x/r", 200, 1),
            ex("/a/z", 200, 1),
        ],
    );
    // `a` has three literals, so it absorbs; the merged children then hold p, q, r and absorb too.
    assert_eq!(templates(&r), ["/a/{a_id}", "/a/{a_id}/{param3}"]);
    assert_eq!(r.endpoints()[1].stats.latency.count(), 5);
}

fn path_strategy() -> impl Strategy<Value = String> {
    let seg = prop_oneof![
        Just("a".to_string()),
        Just("b".to_string()),
        Just("c".to_string()),
        Just("d".to_string()),
        Just("e".to_string()),
        Just("users".to_string()),
        Just("42".to_string()),
        Just("7".to_string()),
        Just("".to_string()),
    ];
    proptest::collection::vec(seg, 1..4).prop_map(|s| format!("/{}", s.join("/")))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn dsc_02_order_independent(
        paths in proptest::collection::vec(path_strategy(), 1..14),
        max in 1usize..4,
        seed in any::<u64>(),
    ) {
        let l = limits(max);
        let exs: Vec<_> = paths.iter().map(|p| ex(p, 200, 1)).collect();
        let a = report_of(&l, &exs);
        let mut shuffled = exs.clone();
        // Deterministic permutation from the seed.
        let mut s = seed;
        for i in (1..shuffled.len()).rev() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            shuffled.swap(i, (s >> 33) as usize % (i + 1));
        }
        let b = report_of(&l, &shuffled);
        prop_assert_eq!(templates(&a), templates(&b));
        prop_assert_eq!(a, b);
    }
}

#[test]
fn dsc_10_buckets() {
    assert_eq!(bucket_index(0), 0);
    assert_eq!(bucket_index(1), 0);
    assert_eq!(bucket_index(2), 4);
    assert_eq!(bucket_index(3), 7);
    assert_eq!(bucket_index(4), 8);
    assert_eq!(bucket_index(5), 10);
    assert_eq!(bucket_index(1024), 40);
    assert_eq!(bucket_index(131_072), 68);
    assert_eq!(bucket_index(131_073), OVERFLOW);
    assert_eq!(bucket_index(u64::MAX), OVERFLOW);
}

#[test]
fn dsc_10_merge_adds() {
    let mut a = Histogram::default();
    let mut b = Histogram::default();
    a.observe(0);
    a.observe(2);
    b.observe(1);
    b.observe(2);
    b.observe(200_000);
    a.merge(&b);
    assert_eq!(a.buckets().get(&0), Some(&2));
    assert_eq!(a.buckets().get(&4), Some(&2));
    assert_eq!(a.buckets().get(&OVERFLOW), Some(&1));
    assert_eq!(a.count(), 5);
}

fn exchange_strategy() -> impl Strategy<Value = (u16, String, u16, u64, u8)> {
    (
        prop_oneof![Just(8080u16), Just(9090u16)],
        path_strategy(),
        prop_oneof![Just(200u16), Just(201), Just(500)],
        prop_oneof![0u64..5, 0u64..3000, 100_000u64..300_000],
        0u8..3,
    )
}

fn build(l: &Limits, items: &[(u16, String, u16, u64, u8)]) -> Report {
    const HOSTS: [&str; 3] = ["h1", "h2", "h3"];
    let mut r = Report::new(Some("p".into()));
    for (port, path, status, ms, h) in items {
        r.record(
            l,
            &Exchange {
                direction: Direction::Inbound,
                port: *port,
                method: if path.len() % 2 == 0 { "GET" } else { "POST" },
                path,
                status: *status,
                head_ms: *ms,
                host: Some(HOSTS[*h as usize]),
                grpc: false,
            },
        );
    }
    r
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn dsc_09_counters_algebra(
        items in proptest::collection::vec(exchange_strategy(), 0..24),
        parts in proptest::collection::vec(0usize..3, 24),
        max in 1usize..4,
    ) {
        let l = limits(max);
        let whole = build(&l, &items);
        let mut groups: [Vec<_>; 3] = Default::default();
        for (i, it) in items.iter().enumerate() {
            groups[parts[i]].push(it.clone());
        }
        let [a, b, c] = groups.map(|g| build(&l, &g));
        // Commutative.
        prop_assert_eq!(merge(a.clone(), b.clone(), &l), merge(b.clone(), a.clone(), &l));
        // Associative.
        let left = merge(merge(a.clone(), b.clone(), &l), c.clone(), &l);
        let right = merge(a.clone(), merge(b.clone(), c.clone(), &l), &l);
        prop_assert_eq!(&left, &right);
        // A partition merges back to the sequential recording.
        prop_assert_eq!(&left, &whole);
        prop_assert_eq!(left.exchanges(), items.len() as u64);
        // Serialisation is canonical and round-trips.
        let json = whole.to_json();
        prop_assert_eq!(&json, &left.to_json());
        prop_assert_eq!(Report::from_json(&json).unwrap(), whole);
    }
}

#[test]
fn dsc_09_union_absorbs() {
    let l = limits(3);
    let a = report_of(&l, &[ex("/u/a", 200, 1), ex("/u/b", 200, 1)]);
    let b = report_of(&l, &[ex("/u/c", 200, 1), ex("/u/d", 500, 1)]);
    let m = merge(a, b, &l);
    assert_eq!(templates(&m), ["/u/{u_id}"]);
    assert_eq!(m.endpoints()[0].stats.statuses.get(&200), Some(&3));
    assert_eq!(m.endpoints()[0].stats.statuses.get(&500), Some(&1));
}

#[test]
fn dsc_09_json_shape() {
    let mut e = ex("/u/1", 200, 3);
    e.host = Some("svc");
    let r = report_of(&limits(3), &[e]);
    let json = r.to_json();
    assert!(json.contains("\"version\":1"), "{json}");
    assert_eq!(json, r.clone().to_json());
    assert_eq!(Report::from_json(&json).unwrap(), r);
    assert!(Report::from_json(&json.replace("\"version\":1", "\"version\":2")).is_err());
}

#[test]
fn dsc_09_hosts_capped() {
    let l = limits(3);
    let hosts: Vec<String> = (0..20).map(|i| format!("h{i:02}")).collect();
    let mut r = Report::new(None);
    for h in hosts.iter().rev() {
        let mut e = ex("/a", 200, 1);
        e.host = Some(h);
        r.record(&l, &e);
    }
    let kept: Vec<_> = r.endpoints()[0].stats.hosts.iter().cloned().collect();
    assert_eq!(kept, hosts[..16]);
}
