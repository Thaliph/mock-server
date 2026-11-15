//! FLT-01, FLT-02, FLT-07 to FLT-09: the faults of a policy, the view and the pure decision.

use proptest::prelude::*;

use super::fault::{Decision, Kind, decide, drawn};
use super::input::*;
use super::policy::{Fault, FaultAbort, FaultDelay, effective, statuses, validate};
use super::route::{MatchView, NameValue, PathView, RequestParts};
use super::testkit::*;
use super::view::{Limits, View, build};
use crate::Mode;

fn path(kind: &str, value: &str) -> RouteMatch {
    RouteMatch {
        path: Some(PathMatch {
            kind: kind.into(),
            value: value.into(),
        }),
        ..RouteMatch::default()
    }
}

fn abort(percent: u32, status: u32) -> Option<AbortSpec> {
    Some(AbortSpec { percent, status })
}

fn delay(percent: u32, fixed_ms: u64) -> Option<DelaySpec> {
    Some(DelaySpec { percent, fixed_ms })
}

fn spec(r#match: Option<RouteMatch>, a: Option<AbortSpec>, d: Option<DelaySpec>) -> FaultSpec {
    FaultSpec {
        r#match,
        abort: a,
        delay: d,
    }
}

fn ok_fault() -> FaultSpec {
    spec(None, abort(10, 503), None)
}

fn with(faults: Vec<FaultSpec>) -> Config {
    Config {
        faults: Some(faults),
        ..cfg("shop", "a", 1, &[(TargetKind::Service, "cart")])
    }
}

fn cluster() -> Snapshot {
    let mut s = snapshot();
    s.services = vec![
        http_svc("shop", "cart", "10.96.0.1"),
        http_svc("risk", "scoring", "10.50.0.1"),
    ];
    s.deployments = vec![("shop".into(), "cart".into())];
    s
}

fn view_faults(s: &Snapshot, key: &str) -> Vec<Fault> {
    view_of(s, "shop", "cart").view.services[key]
        .policy
        .faults
        .clone()
}

#[test]
fn flt_01_without_a_config_the_faults_are_empty_and_the_winners_are_taken_whole_in_order() {
    let mut s = cluster();
    assert!(effective(None).faults.is_empty());
    assert!(view_faults(&s, "shop/cart").is_empty());
    let many: Vec<FaultSpec> = (0..16)
        .map(|i| spec(Some(path("Exact", &format!("/p{i}"))), abort(i, 500 + i), None))
        .collect();
    s.configs = vec![with(many)];
    let got = view_faults(&s, "shop/cart");
    assert_eq!(got.len(), 16);
    for (i, f) in got.iter().enumerate() {
        let want = format!("/p{i}");
        assert_eq!(f.r#match.as_ref().unwrap().path.as_ref().unwrap().value, want);
        assert_eq!(f.abort.as_ref().unwrap().status, 500 + i as u16);
    }
}

#[test]
fn flt_01_a_consented_service_carries_the_faults_of_its_own_namespace() {
    let mut s = cluster();
    s.grants = Some(vec![grant("risk", "shop")]);
    s.configs = vec![
        dep_cfg("shop", "d", 1, "cart", &["risk/scoring"]),
        Config {
            faults: Some(vec![ok_fault()]),
            ..cfg("risk", "p", 1, &[(TargetKind::Service, "scoring")])
        },
        Config {
            faults: Some(vec![ok_fault(), ok_fault()]),
            ..cfg("shop", "q", 1, &[(TargetKind::Service, "scoring")])
        },
    ];
    assert_eq!(view_faults(&s, "risk/scoring").len(), 1);
    assert!(view_faults(&s, "shop/cart").is_empty());
}

fn refused(config: &Config, needle: &str) {
    let err = validate(config).expect_err(needle);
    assert!(err.contains(needle), "{err:?} lacks {needle:?}");
}

#[test]
fn flt_01_validate_refuses_each_bad_fault_naming_the_field_and_the_index() {
    refused(&with((0..17).map(|_| ok_fault()).collect()), "faults");
    refused(&with(vec![ok_fault(), spec(None, None, None)]), "faults[1]");
    refused(&with(vec![spec(None, abort(101, 503), None)]), "faults[0].abort.percent");
    refused(
        &with(vec![ok_fault(), spec(None, None, delay(101, 5))]),
        "faults[1].delay.percent",
    );
    refused(&with(vec![spec(None, abort(1, 199), None)]), "faults[0].abort.status");
    refused(&with(vec![spec(None, abort(1, 600), None)]), "faults[0].abort.status");
    refused(&with(vec![spec(None, None, delay(1, 0))]), "faults[0].delay.fixed");
    refused(&with(vec![spec(None, None, delay(1, 60_001))]), "faults[0].delay.fixed");
    refused(
        &with(vec![spec(Some(path("RegularExpression", "/a.*")), abort(1, 503), None)]),
        "faults[0].match.path",
    );
    refused(
        &with(vec![spec(Some(path("Exact", "a")), abort(1, 503), None)]),
        "faults[0].match.path",
    );
    refused(
        &with(vec![spec(
            Some(path("PathPrefix", &format!("/{}", "a".repeat(1024)))),
            abort(1, 503),
            None,
        )]),
        "faults[0].match.path",
    );
    let name = |kind: &str, n: &str| NameMatch {
        kind: kind.into(),
        name: n.into(),
        value: "v".into(),
    };
    let headers = |h: Vec<NameMatch>| RouteMatch {
        headers: h,
        ..RouteMatch::default()
    };
    let queries = |q: Vec<NameMatch>| RouteMatch {
        query_params: q,
        ..RouteMatch::default()
    };
    refused(
        &with(vec![spec(Some(headers(vec![name("RegularExpression", "x")])), abort(1, 503), None)]),
        "faults[0].match.headers",
    );
    refused(
        &with(vec![spec(Some(queries(vec![name("RegularExpression", "x")])), abort(1, 503), None)]),
        "faults[0].match.queryParams",
    );
    let many = |kind: &str| (0..17).map(|i| name("Exact", &format!("{kind}{i}"))).collect();
    refused(&with(vec![spec(Some(headers(many("h"))), abort(1, 503), None)]), "headers");
    refused(&with(vec![spec(Some(queries(many("q"))), abort(1, 503), None)]), "queryParams");
    let method = RouteMatch {
        method: Some("FETCH".into()),
        ..RouteMatch::default()
    };
    refused(&with(vec![spec(Some(method), abort(1, 503), None)]), "faults[0].match.method");
    let on_deployment = Config {
        faults: Some(vec![ok_fault()]),
        ..cfg("shop", "a", 1, &[(TargetKind::Deployment, "cart")])
    };
    refused(&on_deployment, "faults");
}

#[test]
fn flt_01_the_bounds_themselves_are_valid() {
    let sixteen = |kind: &str| -> Vec<NameMatch> {
        (0..16)
            .map(|i| NameMatch {
                kind: "Exact".into(),
                name: format!("{kind}{i}"),
                value: "v".into(),
            })
            .collect()
    };
    let m = RouteMatch {
        path: Some(PathMatch {
            kind: "PathPrefix".into(),
            value: format!("/{}", "a".repeat(1023)),
        }),
        headers: sixteen("h"),
        query_params: sixteen("q"),
        method: Some("PATCH".into()),
    };
    let faults = vec![
        spec(Some(m), abort(0, 200), delay(100, 1)),
        spec(None, abort(100, 599), delay(0, 60_000)),
    ];
    assert_eq!(validate(&with(faults)), Ok(()));
    let sixteen_faults: Vec<FaultSpec> = (0..16).map(|_| ok_fault()).collect();
    assert_eq!(validate(&with(sixteen_faults)), Ok(()));
    assert_eq!(validate(&with(vec![])), Ok(()));
}

#[test]
fn flt_01_an_invalid_fault_makes_the_config_invalid_in_its_status() {
    let mut s = cluster();
    s.configs = vec![with(vec![spec(None, abort(1, 600), None)])];
    let st = statuses(&s);
    let c = &st[&("shop".to_string(), "a".to_string())];
    assert_eq!(c.accepted.reason, "Invalid");
    assert!(c.accepted.message.contains("faults[0]"));
    assert!(view_faults(&s, "shop/cart").is_empty(), "an invalid config wins nothing");
}

fn full_fault() -> FaultSpec {
    spec(
        Some(RouteMatch {
            path: Some(PathMatch {
                kind: "PathPrefix".into(),
                value: "/checkout".into(),
            }),
            headers: vec![NameMatch {
                kind: "Exact".into(),
                name: "X-Trace".into(),
                value: "Yes".into(),
            }],
            query_params: vec![NameMatch {
                kind: "Exact".into(),
                name: "Q".into(),
                value: "1".into(),
            }],
            method: Some("POST".into()),
        }),
        abort(1, 503),
        delay(10, 500),
    )
}

fn built(faults: Vec<FaultSpec>) -> super::view::Built {
    let mut s = cluster();
    s.configs = vec![with(faults)];
    build(&s, "shop", "cart", Mode::Mock, &Limits::default())
}

#[test]
fn flt_02_the_view_holds_the_faults_with_sorted_keys_and_absent_members_left_out() {
    let b = built(vec![full_fault(), spec(None, None, delay(5, 60_000))]);
    let want = concat!(
        r#""faults":[{"abort":{"percent":1,"status":503},"delay":{"fixedMs":500,"percent":10},"#,
        r#""match":{"headers":[{"name":"x-trace","value":"Yes"}],"method":"POST","#,
        r#""path":{"type":"PathPrefix","value":"/checkout"},"queryParams":[{"name":"Q","value":"1"}]}},"#,
        r#"{"delay":{"fixedMs":60000,"percent":5}}],"#
    );
    assert!(b.json.contains(want), "{}", b.json);
    let none = built(vec![]);
    assert!(none.json.contains(r#""faults":[],"#));
}

#[test]
fn flt_02_any_change_of_any_member_or_of_the_order_gives_a_new_etag() {
    let base = built(vec![full_fault(), spec(None, abort(2, 500), None)]).etag;
    let mut seen = vec![base.clone()];
    let mut variant = |f: &dyn Fn(&mut Vec<FaultSpec>)| {
        let mut faults = vec![full_fault(), spec(None, abort(2, 500), None)];
        f(&mut faults);
        let etag = built(faults).etag;
        assert!(!seen.contains(&etag), "an etag repeated");
        seen.push(etag);
    };
    variant(&|f| f.swap(0, 1));
    variant(&|f| f[0].abort.as_mut().unwrap().percent = 2);
    variant(&|f| f[0].abort.as_mut().unwrap().status = 504);
    variant(&|f| f[0].delay.as_mut().unwrap().percent = 11);
    variant(&|f| f[0].delay.as_mut().unwrap().fixed_ms = 501);
    variant(&|f| f[0].delay = None);
    variant(&|f| f[0].r#match = None);
    variant(&|f| f[0].r#match.as_mut().unwrap().method = Some("PUT".into()));
    variant(&|f| f[0].r#match.as_mut().unwrap().path.as_mut().unwrap().kind = "Exact".into());
    variant(&|f| f[0].r#match.as_mut().unwrap().path.as_mut().unwrap().value = "/cart".into());
    variant(&|f| f[0].r#match.as_mut().unwrap().headers[0].value = "no".into());
    variant(&|f| f[0].r#match.as_mut().unwrap().query_params[0].value = "2".into());
    variant(&|f| f[1].abort.as_mut().unwrap().percent = 3);
}

fn read(text: &str) -> Result<View, String> {
    View::from_json(text, &Limits::default())
}

#[test]
fn flt_02_from_json_reads_back_what_build_wrote() {
    let b = built(vec![full_fault(), spec(None, None, delay(5, 60_000))]);
    let back = read(&b.json).expect("reads back");
    assert_eq!(back, b.view);
    assert_eq!(back.to_json(), b.json);
}

#[test]
fn flt_02_from_json_refuses_a_missing_faults_and_an_unknown_field_at_any_level() {
    let json = built(vec![full_fault()]).json;
    assert!(read(&json.replacen(r#""faults":[{"#, r#""faults_":[{"#, 1)).is_err());
    let without = json.replacen(&json[json.find(r#""faults""#).unwrap()..json.find(r#""loadBalancer""#).unwrap()], "", 1);
    assert!(read(&without).is_err(), "a missing faults");
    for (from, to) in [
        (r#""faults":[{"#, r#""faults":[{"extra":1,"#),
        (r#""abort":{"#, r#""abort":{"extra":1,"#),
        (r#""delay":{"#, r#""delay":{"extra":1,"#),
        (r#""match":{"#, r#""match":{"extra":1,"#),
        (r#""path":{"type""#, r#""path":{"extra":1,"type""#),
        (r#"{"name":"x-trace""#, r#"{"extra":1,"name":"x-trace""#),
        (r#"{"name":"Q""#, r#"{"extra":1,"name":"Q""#),
    ] {
        assert!(json.contains(from), "{from}");
        assert!(read(&json.replacen(from, to, 1)).is_err(), "{from}");
    }
    for member in [r#""percent":1,"#, r#""status":503"#, r#""fixedMs":500,"#, r#""percent":10"#] {
        assert!(json.contains(member), "{member}");
    }
    assert!(read(&json.replacen(r#""percent":1,"#, "", 1)).is_err());
    assert!(read(&json.replacen(r#""status":503"#, r#""x":0"#, 1)).is_err());
    assert!(read(&json.replacen(r#""fixedMs":500,"#, "", 1)).is_err());
}

#[test]
fn flt_02_from_json_refuses_every_value_validate_refuses() {
    let json = built(vec![full_fault()]).json;
    let bad = |from: &str, to: &str| {
        assert!(json.contains(from), "{from}");
        assert!(read(&json.replacen(from, to, 1)).is_err(), "{to}");
    };
    let good = |from: &str, to: &str| {
        assert!(read(&json.replacen(from, to, 1)).is_ok(), "{to}");
    };
    bad(r#""percent":1,"status""#, r#""percent":101,"status""#);
    bad(r#""fixedMs":500,"percent":10"#, r#""fixedMs":500,"percent":101"#);
    bad(r#""status":503"#, r#""status":199"#);
    bad(r#""status":503"#, r#""status":600"#);
    bad(r#""status":503"#, r#""status":70000"#);
    bad(r#""fixedMs":500"#, r#""fixedMs":0"#);
    bad(r#""fixedMs":500"#, r#""fixedMs":60001"#);
    bad(r#""method":"POST""#, r#""method":"FETCH""#);
    bad(r#""type":"PathPrefix""#, r#""type":"RegularExpression""#);
    bad(r#""value":"/checkout""#, r#""value":"checkout""#);
    bad(r#""name":"x-trace""#, r#""name":"X-Trace""#);
    bad(r#""value":"Yes""#, r#""value":"a\nb""#);
    good(r#""percent":1,"status""#, r#""percent":0,"status""#);
    good(r#""percent":1,"status""#, r#""percent":100,"status""#);
    good(r#""status":503"#, r#""status":200"#);
    good(r#""status":503"#, r#""status":599"#);
    good(r#""fixedMs":500"#, r#""fixedMs":1"#);
    good(r#""fixedMs":500"#, r#""fixedMs":60000"#);
}

#[test]
fn flt_02_from_json_refuses_17_faults_an_empty_fault_and_17_match_items() {
    let item = r#"{"abort":{"percent":1,"status":503}}"#;
    let json = built(vec![]).json;
    let many = |n: usize| json.replacen(r#""faults":[]"#, &format!(r#""faults":[{}]"#, vec![item; n].join(",")), 1);
    assert!(read(&many(16)).is_ok());
    assert!(read(&many(17)).is_err());
    assert!(read(&json.replacen(r#""faults":[]"#, r#""faults":[{}]"#, 1)).is_err());
    let items = |n: usize, field: &str| {
        let list: Vec<String> = (0..n).map(|i| format!(r#"{{"name":"h{i}","value":"v"}}"#)).collect();
        json.replacen(
            r#""faults":[]"#,
            &format!(r#""faults":[{{"abort":{{"percent":1,"status":503}},"match":{{"{field}":[{}]}}}}]"#, list.join(",")),
            1,
        )
    };
    for field in ["headers", "queryParams"] {
        assert!(read(&items(16, field)).is_ok(), "{field}");
        assert!(read(&items(17, field)).is_err(), "{field}");
    }
}

// ---- FLT-07 to FLT-09: the decision.

fn mv(path: Option<(&str, &str)>, method: Option<&str>) -> MatchView {
    MatchView {
        path: path.map(|(k, v)| PathView {
            kind: k.into(),
            value: v.into(),
        }),
        method: method.map(Into::into),
        ..MatchView::default()
    }
}

fn fault(m: Option<MatchView>, a: Option<(u32, u16)>, d: Option<(u32, u64)>) -> Fault {
    Fault {
        abort: a.map(|(percent, status)| FaultAbort { percent, status }),
        delay: d.map(|(percent, fixed_ms)| FaultDelay { fixed_ms, percent }),
        r#match: m,
    }
}

fn req<'a>(method: &'a str, path: &'a str) -> RequestParts<'a> {
    RequestParts {
        method,
        path,
        query: None,
        headers: &[],
    }
}

#[test]
fn flt_07_a_fault_without_match_holds_for_every_request() {
    let faults = [fault(None, Some((100, 503)), None)];
    for (method, p) in [("GET", "/"), ("POST", "/a/b"), ("GET", "*"), ("GET", "")] {
        let d = decide(&faults, &req(method, p), u64::MAX, u64::MAX);
        assert_eq!(d, Some(Decision { index: 0, kind: Kind::Abort { status: 503 } }), "{p:?}");
    }
}

#[test]
fn flt_07_the_match_is_the_route_match() {
    let headers: &[(&str, &[u8])] = &[("X-A", b"1"), ("x-a", b"2"), ("x-b", b"y")];
    let m = MatchView {
        headers: vec![
            NameValue { name: "x-a".into(), value: "1,2".into() },
            NameValue { name: "x-b".into(), value: "y".into() },
        ],
        method: Some("POST".into()),
        path: Some(PathView { kind: "PathPrefix".into(), value: "/shop/".into() }),
        query_params: vec![NameValue { name: "q".into(), value: "a b".into() }],
    };
    let faults = [fault(Some(m), Some((100, 500)), None)];
    let r = |method, p, q, h| RequestParts { method, path: p, query: q, headers: h };
    let yes = |parts: RequestParts| decide(&faults, &parts, u64::MAX, 0).is_some();
    assert!(yes(r("POST", "/shop", Some("q=a%20b&q=z"), headers)));
    assert!(yes(r("POST", "/shop/x", Some("q=a+b".replace('+', "%20").as_str()), headers)));
    assert!(!yes(r("GET", "/shop", Some("q=a%20b"), headers)), "method");
    assert!(!yes(r("POST", "/shopx", Some("q=a%20b"), headers)), "segment boundary");
    assert!(!yes(r("POST", "/shop", Some("q=a+b"), headers)), "+ stays +");
    assert!(!yes(r("POST", "/shop", Some("q=z&q=a%20b"), headers)), "first of a name");
    assert!(!yes(r("POST", "/shop", Some("q=a%20b"), &headers[..1])), "joined header");
    assert!(!yes(r("POST", "/shop", None, headers)), "query");
    let exact = [fault(Some(mv(Some(("Exact", "/a")), None)), Some((100, 500)), None)];
    assert!(decide(&exact, &req("GET", "/a"), u64::MAX, 0).is_some());
    assert!(decide(&exact, &req("GET", "/a/"), u64::MAX, 0).is_none());
    assert!(decide(&exact, &req("GET", "*"), u64::MAX, 0).is_none());
}

#[test]
fn flt_07_the_first_fault_that_holds_wins_whatever_the_precedence() {
    let prefix = fault(Some(mv(Some(("PathPrefix", "/")), None)), Some((100, 501)), None);
    let exact = fault(Some(mv(Some(("Exact", "/checkout")), None)), Some((100, 502)), None);
    let status = |faults: &[Fault], p: &str| {
        decide(faults, &req("GET", p), u64::MAX, u64::MAX).map(|d| (d.index, d.kind))
    };
    let a = [prefix.clone(), exact.clone()];
    assert_eq!(status(&a, "/checkout"), Some((0, Kind::Abort { status: 501 })));
    let b = [exact, prefix];
    assert_eq!(status(&b, "/checkout"), Some((0, Kind::Abort { status: 502 })));
    assert_eq!(status(&b, "/cart"), Some((1, Kind::Abort { status: 501 })));
    assert_eq!(decide(&[], &req("GET", "/"), u64::MAX, u64::MAX), None);
    let none = [fault(Some(mv(Some(("Exact", "/x")), None)), Some((100, 500)), None)];
    assert_eq!(decide(&none, &req("GET", "/y"), u64::MAX, u64::MAX), None);
    // The first that holds is the one decided, even when it draws nothing.
    let quiet = [
        fault(None, Some((0, 500)), None),
        fault(None, Some((100, 503)), None),
    ];
    assert_eq!(decide(&quiet, &req("GET", "/"), u64::MAX, u64::MAX), None);
}

#[test]
fn flt_08_percent_0_is_never_drawn_and_100_always() {
    assert!(!drawn(0, 0));
    assert!(!drawn(u64::MAX, 0));
    assert!(drawn(0, 100));
    assert!(drawn(u64::MAX, 100));
}

#[test]
fn flt_08_the_largest_value_drawn_at_10_percent_is_the_ceiling_minus_one() {
    let largest = (10u128 * (1u128 << 64)).div_ceil(100) - 1;
    let largest = u64::try_from(largest).unwrap();
    assert!(drawn(largest, 10));
    assert!(!drawn(largest + 1, 10));
    assert!(drawn(0, 10));
}

#[test]
fn flt_08_each_decision_reads_its_own_value_and_an_abort_wins() {
    let f = [fault(None, Some((10, 503)), Some((10, 700)))];
    let at = |r, s| decide(&f, &req("GET", "/"), r, s).map(|d| (d.index, d.kind));
    assert_eq!(at(0, u64::MAX), Some((0, Kind::Abort { status: 503 })));
    assert_eq!(at(0, 0), Some((0, Kind::Abort { status: 503 })), "abort with a delay drawn");
    assert_eq!(at(u64::MAX, 0), Some((0, Kind::Delay { ms: 700 })));
    assert_eq!(at(u64::MAX, u64::MAX), None);
    let only_delay = [fault(None, None, Some((100, 5)))];
    assert_eq!(
        decide(&only_delay, &req("GET", "/"), 0, u64::MAX).map(|d| d.kind),
        None,
        "the abort value is not read for a delay"
    );
    assert_eq!(
        decide(&only_delay, &req("GET", "/"), u64::MAX, 0).map(|d| d.kind),
        Some(Kind::Delay { ms: 5 })
    );
    assert_eq!(at(1 << 60, 1 << 60), at(1 << 60, 1 << 60), "no hidden state");
}

/// splitmix64: a fixed-seed generator with no dependency.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[test]
fn flt_09_abort_10_and_delay_20_decide_10_and_18_percent_over_100000_pairs() {
    let f = [fault(None, Some((10, 503)), Some((20, 100)))];
    let mut state = 0x4D49_4D49_43u64;
    let (mut aborts, mut delays) = (0u32, 0u32);
    for _ in 0..100_000 {
        let (r, s) = (splitmix(&mut state), splitmix(&mut state));
        match decide(&f, &req("GET", "/"), r, s).map(|d| d.kind) {
            Some(Kind::Abort { .. }) => aborts += 1,
            Some(Kind::Delay { .. }) => delays += 1,
            None => {}
        }
    }
    assert!((9_500..=10_500).contains(&aborts), "aborts {aborts}");
    assert!((17_500..=18_500).contains(&delays), "delays {delays}");
}

fn any_match() -> impl Strategy<Value = Option<MatchView>> {
    let path = proptest::option::of((
        prop_oneof![Just("Exact"), Just("PathPrefix")],
        prop_oneof![Just("/"), Just("/a"), Just("/a/b"), Just("/b/")],
    ));
    let method = proptest::option::of(prop_oneof![Just("GET"), Just("POST")]);
    let header = proptest::option::of(prop_oneof![Just("x-a"), Just("x-b")]);
    proptest::option::of((path, method, header).prop_map(|(p, m, h)| MatchView {
        headers: h
            .map(|n| vec![NameValue { name: n.into(), value: "1".into() }])
            .unwrap_or_default(),
        method: m.map(Into::into),
        path: p.map(|(k, v)| PathView { kind: k.into(), value: v.into() }),
        query_params: vec![],
    }))
}

fn any_fault() -> impl Strategy<Value = Fault> {
    (
        any_match(),
        proptest::option::of((0u32..=100, 200u16..=599)),
        proptest::option::of((0u32..=100, 1u64..=60_000)),
    )
        .prop_filter("one of the two", |(_, a, d)| a.is_some() || d.is_some())
        .prop_map(|(m, a, d)| fault(m, a, d))
}

proptest! {
    #[test]
    fn flt_09_the_decision_names_the_first_fault_that_holds_and_never_panics(
        faults in proptest::collection::vec(any_fault(), 0..=16),
        method in prop_oneof![Just("GET"), Just("POST")],
        p in prop_oneof![Just("/"), Just("/a"), Just("/a/b"), Just("/b/c"), Just("*"), Just("")],
        has_header in any::<bool>(),
        r in any::<u64>(),
        s in any::<u64>(),
    ) {
        let h: [(&str, &[u8]); 1] = [("X-A", b"1")];
        let parts = RequestParts {
            method,
            path: p,
            query: None,
            headers: if has_header { &h } else { &[] },
        };
        let holds = |f: &Fault| match &f.r#match {
            None => true,
            Some(m) => super::route::match_holds(m, &parts),
        };
        let first = faults.iter().position(holds);
        let got = decide(&faults, &parts, r, s);
        if let Some(d) = got {
            prop_assert_eq!(Some(d.index), first);
        }
        if first.is_none() {
            prop_assert_eq!(got, None);
        }
    }
}
