//! RTE-80 to RTE-82: `mesh::mirror` (draw, decisions, deadline), `beforeModifier` in compile and
//! `View::from_json`.

use proptest::prelude::*;

use super::input::*;
use super::mirror::{Body, Candidate, Decision, MAX_IN_FLIGHT, MAX_IN_FLIGHT_TOTAL, TIMEOUT_MS};
use super::mirror::{deadline_ms, decide, drawn};
use super::route::{TimeoutsView, compile};
use super::testkit::*;
use super::view::{Limits, View, build};
use crate::Mode;

const TWO_64: u128 = 1 << 64;

fn reference(n: u32, d: u32, r: u64) -> bool {
    (u128::from(r) * u128::from(d) / TWO_64) < u128::from(n)
}

proptest! {
    #[test]
    fn rte_80_drawn_is_the_floor_formula(d in 1u32..=1_000_000, n in 0u32..=1_000_000, r in any::<u64>()) {
        let n = n.min(d);
        prop_assert_eq!(drawn(n, d, r), reference(n, d, r));
        prop_assert!(drawn(d, d, r), "n = d is always drawn");
        prop_assert!(!drawn(0, d, r), "0 is never drawn");
    }
}

#[test]
fn rte_80_the_extremes_of_r() {
    for d in [1u32, 2, 3, 100, 1000, 1_000_000] {
        for n in [1u32, d / 2 + 1, d] {
            let n = n.min(d);
            assert!(drawn(n, d, 0), "r = 0 draws {n}/{d}");
            assert_eq!(drawn(n, d, u64::MAX), n == d, "r = max for {n}/{d}");
        }
        assert!(!drawn(0, d, 0));
    }
}

#[test]
fn rte_80_the_boundary_values_draw_exactly_n_times() {
    for d in 1u32..=1000 {
        for n in [0, 1, d / 3, d / 2, d - 1, d] {
            let hits = (0..d)
                .filter(|k| {
                    let r = (u128::from(*k) * TWO_64).div_ceil(u128::from(d));
                    drawn(n, d, u64::try_from(r).expect("k < d"))
                })
                .count();
            assert_eq!(hits, n as usize, "{n}/{d}");
        }
    }
}

fn cand(port: &str, drawn: bool) -> Candidate<'_> {
    Candidate { port, drawn }
}

fn plenty(_: &str) -> usize {
    MAX_IN_FLIGHT
}

#[test]
fn rte_81_a_mirror_not_drawn_gives_nothing() {
    let got = decide(
        &[cand("a:80", false), cand("b:80", true)],
        0,
        |_| 0,
        Body::None,
        10,
    );
    assert_eq!(got, vec![None, Some(Decision::SkipLimit)]);
    let got = decide(&[cand("a:80", false)], 5, plenty, Body::Unknown, 0);
    assert_eq!(got, vec![None]);
}

#[test]
fn rte_81_no_free_slot_of_the_port_or_the_total_is_skip_limit() {
    let c = [cand("a:80", true)];
    assert_eq!(
        decide(&c, 5, |_| 0, Body::None, 10),
        vec![Some(Decision::SkipLimit)]
    );
    assert_eq!(
        decide(&c, 0, plenty, Body::None, 10),
        vec![Some(Decision::SkipLimit)]
    );
    assert_eq!(
        decide(&c, 1, |_| 1, Body::None, 10),
        vec![Some(Decision::Send)]
    );
    // A limit comes before a body too large: nothing is held, the answer is the limit.
    assert_eq!(
        decide(&c, 0, plenty, Body::Declared(99), 10),
        vec![Some(Decision::SkipLimit)]
    );
}

#[test]
fn rte_81_the_body_state_picks_the_decision() {
    let c = [cand("a:80", true)];
    let one = |b, share| decide(&c, 9, plenty, b, share)[0];
    assert_eq!(one(Body::None, 0), Some(Decision::Send));
    assert_eq!(one(Body::Declared(10), 10), Some(Decision::AwaitBody));
    assert_eq!(one(Body::Declared(11), 10), Some(Decision::SkipBody));
    assert_eq!(one(Body::Declared(1), 0), Some(Decision::SkipBody));
    assert_eq!(one(Body::Unknown, 0), Some(Decision::AwaitBody));
    assert_eq!(one(Body::Unknown, 10), Some(Decision::AwaitBody));
}

#[test]
fn rte_81_a_skipped_body_gives_its_slot_back() {
    // One slot in total, a body over the share: the first is skipped for its body and takes
    // nothing, so a second mirror of the same rule has the slot. Both skip for the same body.
    let c = [cand("a:80", true), cand("b:80", true)];
    let got = decide(&c, 1, plenty, Body::Declared(11), 10);
    assert_eq!(
        got,
        vec![Some(Decision::SkipBody), Some(Decision::SkipBody)]
    );
    assert!(!Decision::SkipBody.holds_slot());
    assert!(!Decision::SkipLimit.holds_slot());
    assert!(Decision::Send.holds_slot());
    assert!(Decision::AwaitBody.holds_slot());
}

#[test]
fn rte_81_two_drawn_mirrors_of_one_port_take_two_slots() {
    let c = [cand("a:80", true), cand("a:80", true), cand("a:80", true)];
    let got = decide(&c, 10, |_| 2, Body::None, 0);
    assert_eq!(
        got,
        vec![
            Some(Decision::Send),
            Some(Decision::Send),
            Some(Decision::SkipLimit)
        ]
    );
    let c = [cand("a:80", true), cand("b:80", true), cand("a:80", true)];
    let got = decide(&c, 2, |_| 1, Body::Unknown, 5);
    assert_eq!(
        got,
        vec![
            Some(Decision::AwaitBody),
            Some(Decision::AwaitBody),
            Some(Decision::SkipLimit)
        ]
    );
}

proptest! {
    #[test]
    fn rte_81_slots_taken_never_exceed_the_free_ones(
        mirrors in prop::collection::vec((0usize..3, any::<bool>()), 0..16),
        total in 0usize..=MAX_IN_FLIGHT_TOTAL,
        per_port in prop::collection::vec(0usize..=MAX_IN_FLIGHT, 3),
        body in prop_oneof![Just(Body::None), Just(Body::Unknown), (1u64..100).prop_map(Body::Declared)],
        share in 0u64..100,
    ) {
        let ports = ["a:80", "b:80", "c:80"];
        let c: Vec<_> = mirrors.iter().map(|(p, d)| cand(ports[*p], *d)).collect();
        let free = |p: &str| per_port[ports.iter().position(|x| *x == p).unwrap()];
        let got = decide(&c, total, free, body, share);
        prop_assert_eq!(got.len(), c.len());
        let mut taken = [0usize; 3];
        for ((p, d), g) in mirrors.iter().zip(&got) {
            prop_assert_eq!(g.is_some(), *d);
            if g.is_some_and(Decision::holds_slot) {
                taken[*p] += 1;
            }
        }
        prop_assert!(taken.iter().sum::<usize>() <= total);
        for i in 0..3 {
            prop_assert!(taken[i] <= per_port[i]);
        }
    }
}

fn timeouts(request: Option<u64>, backend: Option<u64>) -> Option<TimeoutsView> {
    Some(TimeoutsView {
        request_ms: request,
        backend_request_ms: backend,
    })
}

#[test]
fn rte_81_the_deadline_is_the_earliest_of_the_timeouts() {
    assert_eq!(TIMEOUT_MS, 10_000);
    let d = |t: Option<TimeoutsView>| deadline_ms(TIMEOUT_MS, t.as_ref());
    assert_eq!(d(None), 10_000);
    assert_eq!(d(timeouts(None, None)), 10_000);
    assert_eq!(d(timeouts(Some(0), Some(0))), 10_000);
    assert_eq!(d(timeouts(Some(300), None)), 300);
    assert_eq!(d(timeouts(Some(0), Some(200))), 200);
    assert_eq!(d(timeouts(Some(300), Some(200))), 200);
    assert_eq!(d(timeouts(Some(100), Some(200))), 100);
    assert_eq!(d(timeouts(Some(20_000), Some(30_000))), 10_000);
    assert_eq!(d(timeouts(Some(1_439_985_600_000), None)), 10_000);
    assert_eq!(
        deadline_ms(300, timeouts(Some(1_439_985_600_000), None).as_ref()),
        300
    );
    assert_eq!(
        deadline_ms(u64::MAX, timeouts(Some(1_439_985_600_000), None).as_ref()),
        1_439_985_600_000
    );
}

// RTE-82.

fn mirror_filter(name: &str) -> RouteFilter {
    RouteFilter {
        kind: "RequestMirror".into(),
        request_header_modifier: None,
        response_header_modifier: None,
        request_mirror: Some(Mirror {
            group: String::new(),
            kind: "Service".into(),
            namespace: None,
            name: name.into(),
            port: Some(80),
            percent: None,
            fraction: None,
        }),
    }
}

fn modifier_filter() -> RouteFilter {
    RouteFilter {
        kind: "RequestHeaderModifier".into(),
        request_header_modifier: Some(HeaderModifier {
            set: vec![("x-env".into(), "prod".into())],
            ..HeaderModifier::default()
        }),
        response_header_modifier: None,
        request_mirror: None,
    }
}

fn cluster() -> Vec<Service> {
    vec![
        http_svc("shop", "cart", "10.96.0.1"),
        http_svc("shop", "cart-v1", "10.96.0.2"),
        http_svc("shop", "cart-shadow", "10.96.0.3"),
        http_svc("shop", "cart-audit", "10.96.0.4"),
    ]
}

fn route_with(filters: Vec<RouteFilter>) -> Route {
    route_at(
        "shop",
        "r",
        1,
        vec![ParentRef {
            group: String::new(),
            kind: "Service".into(),
            namespace: None,
            name: "cart".into(),
            section_name: None,
            port: None,
        }],
        vec![RouteRule {
            backend_refs: vec![backend(None, "cart-v1", 80, None)],
            filters,
            ..RouteRule::default()
        }],
    )
}

fn compiled_json(filters: Vec<RouteFilter>) -> String {
    let c = compile(&route_with(filters), &cluster(), Some(&[]));
    serde_json::to_string(&c.view_for("shop/cart").expect("attached")).unwrap()
}

fn mirror_json(name: &str, before: bool) -> String {
    let b = if before {
        r#""beforeModifier":true,"#
    } else {
        ""
    };
    format!(r#"{{{b}"denominator":100,"numerator":100,"port":80,"service":"shop/{name}"}}"#)
}

#[test]
fn rte_82_a_mirror_before_the_request_modifier_is_marked() {
    let j = compiled_json(vec![
        mirror_filter("cart-shadow"),
        modifier_filter(),
        mirror_filter("cart-audit"),
    ]);
    let want = format!(
        r#""mirrors":[{},{}]"#,
        mirror_json("cart-shadow", true),
        mirror_json("cart-audit", false)
    );
    assert!(j.contains(&want), "{j}");
}

#[test]
fn rte_82_no_marker_without_a_modifier_or_after_it() {
    for filters in [
        vec![mirror_filter("cart-shadow")],
        vec![modifier_filter(), mirror_filter("cart-shadow")],
    ] {
        let j = compiled_json(filters);
        assert!(j.contains(&mirror_json("cart-shadow", false)), "{j}");
        assert!(!j.contains("beforeModifier"), "{j}");
    }
}

fn view_json(before: Option<serde_json::Value>) -> String {
    let mut s = snapshot();
    s.services = cluster();
    s.routes = vec![route_with(vec![mirror_filter("cart-shadow")])];
    let b = build(&s, "shop", "cart", Mode::Mock, &Limits::default());
    let mut v: serde_json::Value = serde_json::from_str(&b.json).unwrap();
    let m = v
        .pointer_mut("/services/shop~1cart/routes/0/rules/0/filters/mirrors/0")
        .expect("a mirror");
    if let Some(x) = before {
        m["beforeModifier"] = x;
    }
    v.to_string()
}

fn read(text: &str) -> Result<View, String> {
    View::from_json(text, &Limits::default())
}

#[test]
fn rte_82_from_json_reads_before_modifier() {
    let marked = |v: &View| {
        v.services["shop/cart"].routes[0].rules[0]
            .filters
            .as_ref()
            .expect("filters")
            .mirrors[0]
            .before_modifier
    };
    let t = read(&view_json(Some(serde_json::json!(true)))).expect("true");
    assert!(marked(&t));
    assert!(t.to_json().contains(r#""beforeModifier":true"#));
    let f = read(&view_json(Some(serde_json::json!(false)))).expect("false");
    let absent = read(&view_json(None)).expect("absent");
    assert!(!marked(&f));
    assert_eq!(f, absent, "absent and false are the same");
    assert!(!absent.to_json().contains("beforeModifier"));
}

#[test]
fn rte_82_from_json_refuses_a_non_boolean_marker() {
    for bad in [
        serde_json::json!(1),
        serde_json::json!("true"),
        serde_json::json!(null),
        serde_json::json!([]),
    ] {
        assert!(read(&view_json(Some(bad.clone()))).is_err(), "{bad}");
    }
}

#[test]
fn rte_82_a_view_with_a_marker_reads_back_with_the_same_bytes() {
    let mut s = snapshot();
    s.services = cluster();
    s.routes = vec![route_with(vec![
        mirror_filter("cart-shadow"),
        modifier_filter(),
        mirror_filter("cart-audit"),
    ])];
    let b = build(&s, "shop", "cart", Mode::Mock, &Limits::default());
    assert!(b.json.contains(r#""beforeModifier":true"#));
    let back = read(&b.json).expect("reads back");
    assert_eq!(back, b.view);
    assert_eq!(back.to_json(), b.json);
}
