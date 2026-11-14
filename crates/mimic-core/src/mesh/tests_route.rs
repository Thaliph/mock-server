//! RTE-01 to RTE-06: HTTPRoute compilation and conditions (spec §7.9 "Routes").

use super::input::*;
use super::route::{Compiled, compile};
use super::testkit::*;

fn parent(ns: Option<&str>, name: &str, port: Option<u16>) -> ParentRef {
    ParentRef {
        group: String::new(),
        kind: "Service".into(),
        namespace: ns.map(Into::into),
        name: name.into(),
        section_name: None,
        port,
    }
}

fn backend(ns: Option<&str>, name: &str, port: u16, weight: Option<u32>) -> BackendRef {
    BackendRef {
        group: String::new(),
        kind: "Service".into(),
        namespace: ns.map(Into::into),
        name: name.into(),
        port: Some(port),
        weight,
        has_filters: false,
    }
}

fn rule(backends: Vec<BackendRef>) -> RouteRule {
    RouteRule {
        backend_refs: backends,
        ..RouteRule::default()
    }
}

fn route(ns: &str, name: &str, parents: Vec<ParentRef>, rules: Vec<RouteRule>) -> Route {
    Route {
        namespace: ns.into(),
        name: name.into(),
        uid: format!("uid-{name}"),
        generation: 3,
        created: 100,
        parents,
        rules,
        unknown_fields: vec![],
        oversize: false,
    }
}

fn cluster() -> Vec<Service> {
    vec![
        http_svc("shop", "cart", "10.96.0.1"),
        http_svc("shop", "cart-v1", "10.96.0.2"),
        http_svc("shop", "cart-shadow", "10.96.0.3"),
        http_svc("risk", "scoring", "10.96.0.4"),
    ]
}

fn plain(r: &Route) -> Compiled {
    compile(r, &cluster(), Some(&[]))
}

fn simple(rules: Vec<RouteRule>) -> Route {
    route("shop", "r", vec![parent(None, "cart", None)], rules)
}

fn accepted(c: &Compiled) -> (bool, String, String) {
    let a = &c.parents[0].accepted;
    (a.ok, a.reason.clone(), a.message.clone())
}

fn json(c: &Compiled, svc: &str) -> String {
    serde_json::to_string(&c.view_for(svc).expect("attached")).unwrap()
}

fn pair(n: &str, v: &str) -> (String, String) {
    (n.into(), v.into())
}

fn modifier_filter(kind: &str, m: HeaderModifier) -> RouteFilter {
    RouteFilter {
        kind: kind.into(),
        request_header_modifier: (kind == "RequestHeaderModifier").then(|| m.clone()),
        response_header_modifier: (kind == "ResponseHeaderModifier").then_some(m),
        request_mirror: None,
    }
}

fn other_filter(kind: &str) -> RouteFilter {
    RouteFilter {
        kind: kind.into(),
        request_header_modifier: None,
        response_header_modifier: None,
        request_mirror: None,
    }
}

fn mirror(name: &str, percent: Option<u32>, fraction: Option<(u32, Option<u32>)>) -> RouteFilter {
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
            percent,
            fraction,
        }),
    }
}

#[test]
fn rte_01_every_supported_item_compiles_to_the_example_byte_for_byte() {
    let full = RouteRule {
        name: Some("canary".into()),
        matches: vec![RouteMatch {
            path: Some(PathMatch {
                kind: "PathPrefix".into(),
                value: "/cart".into(),
            }),
            headers: vec![NameMatch {
                kind: "Exact".into(),
                name: "X-Canary".into(),
                value: "1".into(),
            }],
            query_params: vec![NameMatch {
                kind: "Exact".into(),
                name: "v".into(),
                value: "2".into(),
            }],
            method: Some("GET".into()),
        }],
        backend_refs: vec![
            backend(None, "cart-v1", 80, Some(90)),
            backend(None, "missing", 80, Some(10)),
        ],
        filters: vec![
            modifier_filter(
                "RequestHeaderModifier",
                HeaderModifier {
                    set: vec![pair("X-Env", "prod")],
                    add: vec![pair("x-from", "mesh")],
                    remove: vec!["X-Debug".into()],
                },
            ),
            modifier_filter(
                "ResponseHeaderModifier",
                HeaderModifier {
                    remove: vec!["Server".into()],
                    ..HeaderModifier::default()
                },
            ),
            mirror("cart-shadow", Some(10), None),
        ],
        request_timeout: Some("2s".into()),
        backend_request_timeout: Some("500ms".into()),
        has_retry: false,
    };
    let r = route(
        "shop",
        "cart-canary",
        vec![parent(None, "cart", Some(80))],
        vec![full],
    );
    let c = plain(&r);
    let want = concat!(
        r#"{"name":"shop/cart-canary","ports":[80],"rules":[{"#,
        r#""backends":[{"port":80,"service":"shop/cart-v1","weight":90},{"invalid":true,"weight":10}],"#,
        r#""filters":{"mirrors":[{"denominator":100,"numerator":10,"port":80,"service":"shop/cart-shadow"}],"#,
        r#""requestHeaders":{"add":[{"name":"x-from","value":"mesh"}],"remove":["x-debug"],"#,
        r#""set":[{"name":"x-env","value":"prod"}]},"responseHeaders":{"remove":["server"]}},"#,
        r#""matches":[{"headers":[{"name":"x-canary","value":"1"}],"method":"GET","#,
        r#""path":{"type":"PathPrefix","value":"/cart"},"queryParams":[{"name":"v","value":"2"}]}],"#,
        r#""name":"canary","timeouts":{"backendRequestMs":500,"requestMs":2000}}]}"#
    );
    assert_eq!(json(&c, "shop/cart"), want);
}

#[test]
fn rte_01_defaults_weights_values_and_order() {
    let r = simple(vec![
        rule(vec![
            backend(None, "cart-v1", 80, None),
            backend(None, "cart-shadow", 80, Some(0)),
        ]),
        rule(vec![]),
    ]);
    let c = plain(&r);
    let want = concat!(
        r#"{"name":"shop/r","rules":["#,
        r#"{"backends":[{"port":80,"service":"shop/cart-v1","weight":1},"#,
        r#"{"port":80,"service":"shop/cart-shadow","weight":0}],"#,
        r#""matches":[{"path":{"type":"PathPrefix","value":"/"}}]},"#,
        r#"{"backends":[],"matches":[{"path":{"type":"PathPrefix","value":"/"}}]}]}"#
    );
    assert_eq!(json(&c, "shop/cart"), want);
    let mut a = rule(vec![]);
    a.filters = vec![modifier_filter(
        "RequestHeaderModifier",
        HeaderModifier {
            set: vec![pair("B", "Mixed Case"), pair("a", "2")],
            ..HeaderModifier::default()
        },
    )];
    let c = plain(&simple(vec![a]));
    assert!(
        json(&c, "shop/cart")
            .contains(r#""set":[{"name":"b","value":"Mixed Case"},{"name":"a","value":"2"}]"#)
    );
}

#[test]
fn rte_01_durations_and_mirror_fractions() {
    for (text, ms) in [
        ("1h30m", 5_400_000u64),
        ("500ms", 500),
        ("2s", 2000),
        ("1m1s1ms", 61_001),
        ("0s", 0),
    ] {
        let mut a = rule(vec![]);
        a.request_timeout = Some(text.into());
        let c = plain(&simple(vec![a]));
        assert!(c.programmed, "timeout programmed");
        let want = format!(r#""timeouts":{{"requestMs":{ms}}}"#);
        assert!(json(&c, "shop/cart").contains(&want), "timeout in ms");
    }
    let mut a = rule(vec![]);
    a.filters = vec![
        mirror("cart-shadow", Some(10), None),
        mirror("cart-shadow", None, Some((1, None))),
        mirror("cart-shadow", None, None),
        mirror("cart-shadow", None, Some((3, Some(7)))),
    ];
    let c = plain(&simple(vec![a]));
    let j = json(&c, "shop/cart");
    let fr = |n: u32, d: u32| {
        format!(r#"{{"denominator":{d},"numerator":{n},"port":80,"service":"shop/cart-shadow"}}"#)
    };
    let want = format!(
        r#""mirrors":[{},{},{},{}]"#,
        fr(10, 100),
        fr(1, 100),
        fr(100, 100),
        fr(3, 7)
    );
    assert!(j.contains(&want), "{j}");
}

fn first_unsupported(mut r: Route) -> (bool, String, String) {
    r.name = "x".into();
    let c = plain(&r);
    assert!(!c.programmed);
    assert!(c.attachments.is_empty());
    assert!(c.view_for("shop/cart").is_none());
    accepted(&c)
}

#[test]
fn rte_02_each_unsupported_item_names_its_path() {
    let reg = |t: &str| NameMatch {
        kind: t.into(),
        name: "SECRET-NAME".into(),
        value: "SECRET-VALUE".into(),
    };
    let mut cases: Vec<(RouteRule, &str)> = vec![];
    cases.push((
        RouteRule {
            matches: vec![RouteMatch {
                path: Some(PathMatch {
                    kind: "RegularExpression".into(),
                    value: "/SECRET-PATH".into(),
                }),
                ..RouteMatch::default()
            }],
            ..RouteRule::default()
        },
        "rules[0].matches[0].path.type: RegularExpression",
    ));
    cases.push((
        RouteRule {
            matches: vec![RouteMatch {
                headers: vec![reg("Exact"), reg("RegularExpression")],
                ..RouteMatch::default()
            }],
            ..RouteRule::default()
        },
        "rules[0].matches[0].headers[1].type: RegularExpression",
    ));
    cases.push((
        RouteRule {
            matches: vec![RouteMatch {
                query_params: vec![reg("RegularExpression")],
                ..RouteMatch::default()
            }],
            ..RouteRule::default()
        },
        "rules[0].matches[0].queryParams[0].type: RegularExpression",
    ));
    for kind in [
        "URLRewrite",
        "RequestRedirect",
        "CORS",
        "ExtensionRef",
        "Made-Up",
    ] {
        cases.push((
            RouteRule {
                filters: vec![other_filter("RequestMirror").clone(), other_filter(kind)],
                ..RouteRule::default()
            },
            Box::leak(format!("rules[0].filters[1].type: {kind}").into_boxed_str()),
        ));
    }
    let mut with_filters = backend(None, "cart-v1", 80, None);
    with_filters.has_filters = true;
    cases.push((
        rule(vec![backend(None, "cart-v1", 80, None), with_filters]),
        "rules[0].backendRefs[1].filters",
    ));
    for (rule, path) in cases {
        let mut r = simple(vec![rule]);
        // "RequestMirror" without a body in the cases above is not a path of its own.
        for f in &mut r.rules[0].filters {
            if f.kind == "RequestMirror" {
                *f = mirror("cart-shadow", None, None);
            }
        }
        let (ok, reason, message) = first_unsupported(r);
        assert!(!ok);
        assert_eq!(reason, "UnsupportedValue");
        assert_eq!(message, format!("unsupported: {path}"));
        assert!(!message.contains("SECRET"));
    }
}

#[test]
fn rte_02_unknown_fields_and_the_first_one_wins() {
    let mut r = simple(vec![rule(vec![])]);
    r.unknown_fields = vec!["rules[0].sessionPersistence".into(), "madeUp".into()];
    let (_, reason, message) = first_unsupported(r);
    assert_eq!(reason, "UnsupportedValue");
    assert_eq!(message, "unsupported: rules[0].sessionPersistence");

    let two = RouteRule {
        filters: vec![other_filter("URLRewrite"), other_filter("CORS")],
        ..RouteRule::default()
    };
    let (_, _, message) = first_unsupported(simple(vec![two]));
    assert_eq!(message, "unsupported: rules[0].filters[0].type: URLRewrite");
}

#[test]
fn rte_02_another_route_of_the_service_is_still_programmed() {
    let bad = RouteRule {
        filters: vec![other_filter("URLRewrite")],
        ..RouteRule::default()
    };
    assert!(!plain(&simple(vec![bad])).programmed);
    let good = plain(&simple(vec![rule(vec![backend(
        None, "cart-v1", 80, None,
    )])]));
    assert!(good.programmed);
    assert!(good.view_for("shop/cart").is_some());
}

#[test]
fn rte_03_refused_modifier_names() {
    let names = [
        "host",
        "Content-Length",
        "transfer-encoding",
        "connection",
        "keep-alive",
        "proxy-connection",
        "te",
        "trailer",
        "upgrade",
        "x-mimic-error",
        "X-Mimic-Anything",
    ];
    for kind in ["RequestHeaderModifier", "ResponseHeaderModifier"] {
        let field = if kind == "RequestHeaderModifier" {
            "requestHeaderModifier"
        } else {
            "responseHeaderModifier"
        };
        for name in names {
            for (list, at) in [("set", 0), ("add", 0), ("remove", 0)] {
                let m = match list {
                    "set" => HeaderModifier {
                        set: vec![pair(name, "v")],
                        ..HeaderModifier::default()
                    },
                    "add" => HeaderModifier {
                        add: vec![pair(name, "v")],
                        ..HeaderModifier::default()
                    },
                    _ => HeaderModifier {
                        remove: vec![name.into()],
                        ..HeaderModifier::default()
                    },
                };
                let r = simple(vec![RouteRule {
                    filters: vec![modifier_filter(kind, m)],
                    ..RouteRule::default()
                }]);
                let (ok, reason, message) = first_unsupported(r);
                assert!(!ok);
                assert_eq!(reason, "UnsupportedValue");
                let tail = if list == "remove" {
                    String::new()
                } else {
                    ".name".into()
                };
                assert_eq!(
                    message,
                    format!("unsupported: rules[0].filters[0].{field}.{list}[{at}]{tail}"),
                    "{kind} {list} {name}"
                );
            }
        }
    }
    let ok = RouteRule {
        filters: vec![modifier_filter(
            "RequestHeaderModifier",
            HeaderModifier {
                set: vec![pair("x-mimic", "1"), pair("hostname", "1")],
                add: vec![pair("x-content-length", "1")],
                remove: vec![],
            },
        )],
        ..RouteRule::default()
    };
    assert!(plain(&simple(vec![ok])).programmed);
}

#[test]
fn rte_04_retry_is_ignored_and_said() {
    let mut a = rule(vec![backend(None, "cart-v1", 80, None)]);
    a.has_retry = true;
    let c = plain(&simple(vec![a]));
    assert!(c.programmed);
    assert_eq!(
        accepted(&c),
        (
            true,
            "Accepted".into(),
            "retry is configured by MimicConfig".into()
        )
    );
    assert!(!json(&c, "shop/cart").contains("retry"));
    let c = plain(&simple(vec![rule(vec![])]));
    assert_eq!(accepted(&c), (true, "Accepted".into(), String::new()));
}

#[test]
fn rte_04_hostnames_change_nothing() {
    // The input has no `hostnames` field at all: two routes that differ only there convert equal.
    let a = plain(&simple(vec![rule(vec![])]));
    let b = plain(&simple(vec![rule(vec![])]));
    assert_eq!(a, b);
}

#[test]
fn rte_05_ports_of_a_parent() {
    let ports = |parents: Vec<ParentRef>| {
        let c = plain(&route("shop", "r", parents, vec![rule(vec![])]));
        c.attachments.get("shop/cart").cloned()
    };
    assert_eq!(
        ports(vec![parent(None, "cart", Some(80))]),
        Some(Some(vec![80]))
    );
    assert_eq!(ports(vec![parent(None, "cart", None)]), Some(None));
    assert_eq!(
        ports(vec![
            parent(None, "cart", Some(80)),
            parent(None, "cart", None)
        ]),
        Some(None)
    );
    let mut two = http_svc("shop", "two", "10.96.0.9");
    two.ports.push(ServicePort {
        port: 81,
        name: Some("b".into()),
        target_port: TargetPort::Number(81),
    });
    let r = route(
        "shop",
        "r",
        vec![parent(None, "two", Some(80)), parent(None, "two", Some(81))],
        vec![rule(vec![])],
    );
    let c = compile(&r, &[two], Some(&[]));
    assert_eq!(c.attachments.get("shop/two"), Some(&Some(vec![80, 81])));
    assert!(json(&c, "shop/two").contains(r#""ports":[80,81]"#));
    let c = plain(&simple(vec![rule(vec![])]));
    assert!(!json(&c, "shop/cart").contains("ports"));
}

#[test]
fn rte_05_unmatched_parents() {
    let mut section = parent(None, "cart", None);
    section.section_name = Some("web".into());
    let r = route(
        "shop",
        "r",
        vec![
            parent(None, "cart", Some(81)),
            section,
            parent(None, "cart", Some(80)),
        ],
        vec![rule(vec![])],
    );
    let c = plain(&r);
    assert!(c.programmed);
    assert_eq!(c.parents.len(), 3);
    assert_eq!(
        c.parents.iter().map(|p| p.index).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(!c.parents[0].accepted.ok);
    assert_eq!(c.parents[0].accepted.reason, "NoMatchingParent");
    assert!(!c.parents[1].accepted.ok);
    assert_eq!(c.parents[1].accepted.reason, "NoMatchingParent");
    assert_eq!(
        c.parents[1].accepted.message,
        "sectionName is not supported for a Service parent"
    );
    assert!(c.parents[2].accepted.ok);
    assert_eq!(c.attachments.get("shop/cart"), Some(&Some(vec![80])));
}

#[test]
fn rte_05_parents_that_get_no_entry() {
    let mut gateway = parent(None, "gw", None);
    gateway.group = GATEWAY_GROUP.into();
    gateway.kind = "Gateway".into();
    let mut other_group = parent(None, "cart", None);
    other_group.group = "example.com".into();
    let mut other_kind = parent(None, "cart", None);
    other_kind.kind = "ServiceImport".into();
    let r = route(
        "shop",
        "r",
        vec![
            gateway,
            other_group,
            other_kind,
            parent(None, "ghost", None),
            parent(Some("risk"), "scoring", None),
        ],
        vec![rule(vec![])],
    );
    let c = plain(&r);
    assert_eq!(c.parents.len(), 1);
    assert_eq!(c.parents[0].index, 4);
    assert_eq!(
        c.attachments.keys().collect::<Vec<_>>(),
        vec!["risk/scoring"]
    );
    // A parent without namespace is in the route's namespace.
    let c = plain(&simple(vec![rule(vec![])]));
    assert_eq!(c.attachments.keys().collect::<Vec<_>>(), vec!["shop/cart"]);
}

fn grant_for(ns: &str, from_ns: &str, name: Option<&str>) -> ReferenceGrant {
    ReferenceGrant {
        namespace: ns.into(),
        from: vec![GrantFrom {
            group: GATEWAY_GROUP.into(),
            kind: "HTTPRoute".into(),
            namespace: from_ns.into(),
        }],
        to: vec![GrantTo {
            group: String::new(),
            kind: "Service".into(),
            name: name.map(Into::into),
        }],
    }
}

fn cross(grants: Option<&[ReferenceGrant]>, target: &str) -> Compiled {
    let r = simple(vec![rule(vec![
        backend(None, "cart-v1", 80, Some(5)),
        backend(Some("risk"), target, 80, Some(7)),
    ])]);
    compile(&r, &cluster(), grants)
}

fn refs(c: &Compiled) -> (bool, String, String) {
    let x = &c.parents[0].resolved_refs;
    (x.ok, x.reason.clone(), x.message.clone())
}

#[test]
fn rte_06_a_grant_makes_a_cross_namespace_backend_valid() {
    for name in [None, Some(""), Some("scoring")] {
        let g = [grant_for("risk", "shop", name)];
        let c = cross(Some(&g), "scoring");
        assert_eq!(
            refs(&c),
            (true, "ResolvedRefs".into(), String::new()),
            "{name:?}"
        );
        assert!(
            json(&c, "shop/cart").contains(r#"{"port":80,"service":"risk/scoring","weight":7}"#)
        );
    }
}

#[test]
fn rte_06_each_wrong_grant_is_ref_not_permitted_whether_or_not_the_service_exists() {
    let mut wrong_from_ns = grant_for("risk", "other", None);
    let mut wrong_kind = grant_for("risk", "shop", None);
    wrong_kind.from[0].kind = "MimicConfig".into();
    wrong_kind.from[0].group = MIMIC_GROUP.into();
    let mut wrong_group = grant_for("risk", "shop", None);
    wrong_group.from[0].group = "example.com".into();
    let mut to_kind = grant_for("risk", "shop", None);
    to_kind.to[0].kind = "Secret".into();
    let mut to_group = grant_for("risk", "shop", None);
    to_group.to[0].group = "example.com".into();
    let to_name = grant_for("risk", "shop", Some("other"));
    let third = grant_for("elsewhere", "shop", None);
    wrong_from_ns.namespace = "risk".into();
    let cases = [
        Some(vec![wrong_from_ns]),
        Some(vec![wrong_kind]),
        Some(vec![wrong_group]),
        Some(vec![to_kind]),
        Some(vec![to_group]),
        Some(vec![to_name]),
        Some(vec![third]),
        Some(vec![]),
        None,
    ];
    for (i, grants) in cases.iter().enumerate() {
        let mut seen = vec![];
        for target in ["scoring", "ghost"] {
            let c = cross(grants.as_deref(), target);
            let (ok, reason, message) = refs(&c);
            assert!(!ok, "case {i}");
            assert_eq!(reason, "RefNotPermitted", "case {i}");
            assert_eq!(message, format!("risk/{target}"), "case {i}");
            let j = json(&c, "shop/cart");
            assert!(j.contains(r#"{"invalid":true,"weight":7}"#), "{j}");
            assert!(!j.contains("risk/"), "{j}");
            assert!(accepted(&c).0, "the route stays accepted");
            seen.push(j.replace("ghost", "scoring"));
        }
        assert_eq!(seen[0], seen[1], "case {i}: same answer whether it exists");
    }
}

#[test]
fn rte_06_a_consumer_route_needs_the_grant_for_its_own_parent_too() {
    // Route of `shop` whose parent and backend are `risk/scoring`.
    let r = route(
        "shop",
        "r",
        vec![parent(Some("risk"), "scoring", None)],
        vec![rule(vec![backend(Some("risk"), "scoring", 80, None)])],
    );
    let c = compile(&r, &cluster(), Some(&[]));
    assert_eq!(refs(&c).1, "RefNotPermitted");
    let g = [grant_for("risk", "shop", None)];
    let c = compile(&r, &cluster(), Some(&g));
    assert!(refs(&c).0);
}

#[test]
fn rte_06_invalid_kind_and_backend_not_found() {
    let mut import = backend(None, "cart-v1", 80, Some(2));
    import.group = "multicluster.x-k8s.io".into();
    import.kind = "ServiceImport".into();
    let c = plain(&simple(vec![rule(vec![import])]));
    assert_eq!(
        refs(&c),
        (false, "InvalidKind".into(), "shop/cart-v1".into())
    );
    assert!(json(&c, "shop/cart").contains(r#""backends":[{"invalid":true,"weight":2}]"#));

    let c = plain(&simple(vec![rule(vec![
        backend(None, "ghost", 80, None),
        backend(None, "cart-v1", 8080, None),
    ])]));
    assert_eq!(
        refs(&c),
        (
            false,
            "BackendNotFound".into(),
            "shop/ghost, shop/cart-v1".into()
        )
    );
}

#[test]
fn rte_06_several_invalid_refs_take_the_first_reason_and_list_all() {
    let mut import = backend(None, "cart-v1", 80, None);
    import.kind = "ServiceImport".into();
    let r = simple(vec![RouteRule {
        backend_refs: vec![
            backend(None, "ghost", 80, None),
            import,
            backend(Some("risk"), "scoring", 80, None),
        ],
        filters: vec![mirror("nope", Some(5), None)],
        ..RouteRule::default()
    }]);
    let c = compile(&r, &cluster(), Some(&[]));
    let (ok, reason, message) = refs(&c);
    assert!(!ok);
    assert_eq!(reason, "RefNotPermitted");
    for each in ["shop/ghost", "shop/cart-v1", "risk/scoring", "shop/nope"] {
        assert!(message.contains(each), "{message}");
    }
    assert!(accepted(&c).0);
    // An invalid mirror is absent.
    assert!(!json(&c, "shop/cart").contains("mirrors"));
}

#[test]
fn rte_06_the_message_is_bounded() {
    let backends = (0..16)
        .map(|i| backend(None, &format!("{}{i}", "n".repeat(60)), 80, None))
        .collect();
    let c = plain(&simple(vec![rule(backends)]));
    assert!(refs(&c).2.len() <= 1024);
    let many = (0..16)
        .map(|_| rule(vec![backend(None, &"m".repeat(60), 80, None)]))
        .collect();
    let c = plain(&simple(many));
    assert!(refs(&c).2.len() <= 1024);
}

/// The view refuses (RTE-12) what the compiler must not write: RTE-03 for the value checks.
#[test]
fn rte_03_values_the_view_refuses_make_the_route_unsupported() {
    let nm = |name: &str, value: &str| NameMatch {
        kind: "Exact".into(),
        name: name.into(),
        value: value.into(),
    };
    let with_match = |m: RouteMatch| RouteRule {
        matches: vec![m],
        ..RouteRule::default()
    };
    let long = "a".repeat(4097);
    let long_path = format!("/{}", "a".repeat(1024));
    let mut cases: Vec<(RouteRule, &str)> = vec![];
    for bad in ["a\nb", "a\rb", "a\0b", long.as_str()] {
        cases.push((
            with_match(RouteMatch {
                headers: vec![nm("x-a", bad)],
                ..RouteMatch::default()
            }),
            "rules[0].matches[0].headers[0].value",
        ));
        cases.push((
            with_match(RouteMatch {
                query_params: vec![nm("q", "ok"), nm("q", bad)],
                ..RouteMatch::default()
            }),
            "rules[0].matches[0].queryParams[1].value",
        ));
        cases.push((
            RouteRule {
                filters: vec![modifier_filter(
                    "RequestHeaderModifier",
                    HeaderModifier {
                        add: vec![pair("x-a", bad)],
                        ..HeaderModifier::default()
                    },
                )],
                ..RouteRule::default()
            },
            "rules[0].filters[0].requestHeaderModifier.add[0].value",
        ));
    }
    let path = |value: &str| {
        with_match(RouteMatch {
            path: Some(PathMatch {
                kind: "PathPrefix".into(),
                value: value.into(),
            }),
            ..RouteMatch::default()
        })
    };
    cases.push((path(&long_path), "rules[0].matches[0].path.value"));
    cases.push((path("/a\nb"), "rules[0].matches[0].path.value"));
    cases.push((path("a"), "rules[0].matches[0].path.value"));
    cases.push((
        with_match(RouteMatch {
            headers: vec![nm("bad name", "v")],
            ..RouteMatch::default()
        }),
        "rules[0].matches[0].headers[0].name",
    ));
    cases.push((
        with_match(RouteMatch {
            method: Some("get".into()),
            ..RouteMatch::default()
        }),
        "rules[0].matches[0].method",
    ));
    for (fraction, percent) in [(Some((3, Some(2))), None), (Some((1, Some(0))), None)] {
        cases.push((
            RouteRule {
                filters: vec![mirror("cart-shadow", percent, fraction)],
                ..RouteRule::default()
            },
            "rules[0].filters[0].requestMirror",
        ));
    }
    cases.push((
        RouteRule {
            filters: vec![mirror("cart-shadow", Some(101), None)],
            ..RouteRule::default()
        },
        "rules[0].filters[0].requestMirror",
    ));
    for (rule, path) in cases {
        let (ok, reason, message) = first_unsupported(simple(vec![rule]));
        assert!(!ok);
        assert_eq!(reason, "UnsupportedValue");
        assert_eq!(message, format!("unsupported: {path}"));
    }
    // The bounds themselves are accepted.
    let mut edge = with_match(RouteMatch {
        headers: vec![nm("x-a", &"a".repeat(4096))],
        path: Some(PathMatch {
            kind: "PathPrefix".into(),
            value: format!("/{}", "a".repeat(1023)),
        }),
        method: Some("GET".into()),
        ..RouteMatch::default()
    });
    edge.backend_refs = vec![];
    edge.filters = vec![mirror("cart-shadow", None, Some((2, Some(2))))];
    assert!(plain(&simple(vec![edge])).programmed);
}

/// One item over each CRD maximum the view enforces makes the route unsupported (RTE-03, RTE-12);
/// the maximum itself is programmed.
#[test]
fn rte_03_one_item_over_each_crd_maximum_makes_the_route_unsupported() {
    let nm = |i: usize| NameMatch {
        kind: "Exact".into(),
        name: format!("x-{i}"),
        value: "v".into(),
    };
    let backend = |weight: Option<u32>| BackendRef {
        group: String::new(),
        kind: "Service".into(),
        namespace: None,
        name: "cart".into(),
        port: Some(80),
        weight,
        has_filters: false,
    };
    let modifier = |kind: &str, set: usize, add: usize, remove: usize| {
        modifier_filter(
            kind,
            HeaderModifier {
                set: (0..set).map(|i| pair(&format!("x-s{i}"), "v")).collect(),
                add: (0..add).map(|i| pair(&format!("x-a{i}"), "v")).collect(),
                remove: (0..remove).map(|i| format!("x-r{i}")).collect(),
            },
        )
    };
    let matches = |m: RouteMatch, n: usize| RouteRule {
        matches: vec![m; n],
        ..RouteRule::default()
    };
    let headers = |n: usize| RouteMatch {
        headers: (0..n).map(nm).collect(),
        ..RouteMatch::default()
    };
    let queries = |n: usize| RouteMatch {
        query_params: (0..n).map(nm).collect(),
        ..RouteMatch::default()
    };
    let backends = |n: usize| RouteRule {
        backend_refs: vec![backend(Some(1)); n],
        ..RouteRule::default()
    };
    let mirrors = |n: usize| RouteRule {
        filters: vec![mirror("cart-shadow", None, None); n],
        ..RouteRule::default()
    };
    let filter_rule = |f: RouteFilter| RouteRule {
        filters: vec![f],
        ..RouteRule::default()
    };
    let weighted = |w: u32| RouteRule {
        backend_refs: vec![backend(Some(w))],
        ..RouteRule::default()
    };
    // (rule at its maximum, rule one over, the path named)
    let cases: Vec<(RouteRule, RouteRule, &str)> = vec![
        (
            matches(RouteMatch::default(), 64),
            matches(RouteMatch::default(), 65),
            "rules[0].matches",
        ),
        (
            matches(headers(16), 1),
            matches(headers(17), 1),
            "rules[0].matches[0].headers",
        ),
        (
            matches(queries(16), 1),
            matches(queries(17), 1),
            "rules[0].matches[0].queryParams",
        ),
        (backends(16), backends(17), "rules[0].backendRefs"),
        (
            weighted(1_000_000),
            weighted(1_000_001),
            "rules[0].backendRefs[0].weight",
        ),
        (mirrors(16), mirrors(17), "rules[0].filters"),
        (
            filter_rule(modifier("RequestHeaderModifier", 16, 16, 16)),
            filter_rule(modifier("RequestHeaderModifier", 17, 0, 0)),
            "rules[0].filters[0].requestHeaderModifier.set",
        ),
        (
            filter_rule(modifier("ResponseHeaderModifier", 16, 16, 16)),
            filter_rule(modifier("ResponseHeaderModifier", 0, 17, 0)),
            "rules[0].filters[0].responseHeaderModifier.add",
        ),
        (
            filter_rule(modifier("RequestHeaderModifier", 16, 16, 16)),
            filter_rule(modifier("RequestHeaderModifier", 0, 0, 17)),
            "rules[0].filters[0].requestHeaderModifier.remove",
        ),
    ];
    for (at_max, over, path) in cases {
        let c = plain(&simple(vec![at_max]));
        assert!(c.programmed, "at the maximum: {path}");
        let (ok, reason, message) = first_unsupported(simple(vec![over]));
        assert!(!ok, "{path}");
        assert_eq!(reason, "UnsupportedValue");
        assert_eq!(message, format!("unsupported: {path}"));
    }
    // Two modifiers of one type merge into one list, which the view bounds.
    let two = RouteRule {
        filters: vec![
            modifier("RequestHeaderModifier", 9, 0, 0),
            modifier("RequestHeaderModifier", 8, 0, 0),
        ],
        ..RouteRule::default()
    };
    assert!(!plain(&simple(vec![two])).programmed);
    let rules = |n: usize| vec![RouteRule::default(); n];
    assert!(plain(&simple(rules(16))).programmed);
    let (ok, reason, message) = first_unsupported(simple(rules(17)));
    assert!(!ok);
    assert_eq!(reason, "UnsupportedValue");
    assert_eq!(message, "unsupported: rules");
}
