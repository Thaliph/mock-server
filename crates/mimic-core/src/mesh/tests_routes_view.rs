//! RTE-07 to RTE-10: routes in the scoped view (spec §7.9 "Routes").

use super::input::*;
use super::testkit::*;
use super::view::{Limits, View, build};
use crate::Mode;

/// `shop` holds cart, cart-v1 and cart-shadow; `risk` holds a and b; `web` holds app and app2;
/// `web`'s deployment `front` declares `shop/cart` (and `shop` consents).
fn cluster() -> Snapshot {
    let mut s = snapshot();
    s.services = vec![
        http_svc("shop", "cart", "10.96.0.1"),
        http_svc("shop", "cart-v1", "10.96.0.2"),
        http_svc("shop", "cart-shadow", "10.96.0.3"),
        http_svc("risk", "a", "10.96.0.4"),
        http_svc("risk", "b", "10.96.0.5"),
        http_svc("web", "app", "10.96.0.6"),
        http_svc("web", "app2", "10.96.0.7"),
        http_svc("other", "x", "10.96.0.8"),
    ];
    s.slices = s
        .services
        .iter()
        .enumerate()
        .map(|(i, v)| {
            slice(
                &v.namespace,
                &format!("{}-1", v.name),
                &v.name,
                1,
                8080,
                vec![zoned(&format!("10.244.0.{}", i + 1), "zone-a")],
            )
        })
        .collect();
    s.configs = vec![dep_cfg("web", "c", 1, "front", &["shop/cart"])];
    s.grants = Some(vec![grant("shop", "web")]);
    s
}

fn on_cart(ns: &str, name: &str, created: i64) -> Route {
    route_at(
        ns,
        name,
        created,
        vec![parent(Some("shop"), "cart", None)],
        vec![],
    )
}

fn json_has(b: &super::view::Built, text: &str) -> bool {
    b.json.contains(text)
}

#[test]
fn rte_07_a_caller_of_the_namespace_sees_its_producer_routes() {
    let mut s = cluster();
    s.routes = vec![on_cart("shop", "p", 1)];
    let b = view_of(&s, "shop", "cart");
    assert_eq!(route_names(&b, "shop/cart"), ["shop/p"]);
    // A caller of another namespace without a consumer route sees the producer route too.
    let b = view_of(&s, "web", "front");
    assert_eq!(route_names(&b, "shop/cart"), ["shop/p"]);
}

#[test]
fn rte_07_consumer_routes_replace_producer_routes_for_their_namespace_only() {
    let mut s = cluster();
    s.routes = vec![on_cart("shop", "p", 1), on_cart("web", "c", 2)];
    let web = view_of(&s, "web", "front");
    assert_eq!(route_names(&web, "shop/cart"), ["web/c"]);
    let shop = view_of(&s, "shop", "cart");
    assert_eq!(route_names(&shop, "shop/cart"), ["shop/p"]);
    assert!(!json_has(&shop, "web/c"));
}

#[test]
fn rte_07_a_route_of_a_third_namespace_applies_to_nobody_else() {
    let mut s = cluster();
    s.routes = vec![on_cart("other", "t", 1)];
    for (n, d) in [("shop", "cart"), ("web", "front"), ("other", "x")] {
        let b = view_of(&s, n, d);
        assert!(!json_has(&b, "other/t"), "{n}");
        assert!(!json_has(&b, "\"routes\""), "{n}");
    }
}

#[test]
fn rte_07_an_unprogrammed_consumer_route_does_not_hide_the_producer_routes() {
    let mut s = cluster();
    let mut bad = on_cart("web", "c", 2);
    bad.unknown_fields = vec!["rules[0].sessionPersistence".into()];
    s.routes = vec![on_cart("shop", "p", 1), bad];
    let b = view_of(&s, "web", "front");
    assert_eq!(route_names(&b, "shop/cart"), ["shop/p"]);
}

#[test]
fn rte_08_routes_are_ordered_by_creation_time_then_name_and_put_their_generation() {
    let mut s = cluster();
    s.routes = vec![
        on_cart("shop", "b", 5),
        on_cart("shop", "a", 5),
        on_cart("shop", "z", 3),
    ];
    let b = view_of(&s, "shop", "cart");
    assert_eq!(route_names(&b, "shop/cart"), ["shop/z", "shop/a", "shop/b"]);
    let gens = &b.view.services["shop/cart"].generations;
    for name in ["a", "b", "z"] {
        assert_eq!(gens[&format!("httproute/shop/{name}@uid-{name}")], 3);
    }
    assert!(
        !b.view
            .generations
            .keys()
            .any(|k| k.starts_with("httproute"))
    );
}

#[test]
fn rte_08_rules_keep_their_written_order() {
    let mut s = cluster();
    let named = |n: &str| RouteRule {
        name: Some(n.into()),
        ..RouteRule::default()
    };
    s.routes = vec![route_at(
        "shop",
        "p",
        1,
        vec![parent(None, "cart", None)],
        vec![named("zz"), named("aa"), named("mm")],
    )];
    let b = view_of(&s, "shop", "cart");
    let names: Vec<_> = b.view.services["shop/cart"].routes[0]
        .rules
        .iter()
        .map(|r| r.name.clone().unwrap())
        .collect();
    assert_eq!(names, ["zz", "aa", "mm"]);
}

#[test]
fn rte_08_a_view_without_routes_is_byte_identical_to_the_view_without_any_route_object() {
    let without = view_of(&cluster(), "shop", "cart");
    assert!(!json_has(&without, "routes"));
    // Routes that apply to nothing in this view change no byte.
    let mut s = cluster();
    s.routes = vec![
        on_cart("other", "t", 1),
        route_at(
            "shop",
            "gw",
            1,
            vec![ParentRef {
                kind: "Gateway".into(),
                group: GATEWAY_GROUP.into(),
                ..parent(None, "gw", None)
            }],
            vec![],
        ),
        route_at(
            "shop",
            "gone",
            1,
            vec![parent(None, "nothere", None)],
            vec![],
        ),
    ];
    let with = view_of(&s, "shop", "cart");
    assert_eq!(with.json, without.json);
    assert_eq!(with.etag, without.etag);
}

#[test]
fn rte_09_backends_and_mirrors_are_added_once_with_endpoints_and_without_their_routes() {
    let mut s = cluster();
    let mut rule = to_backends(vec![
        backend(None, "cart-v1", 80, Some(1)),
        backend(None, "cart-v1", 80, Some(2)),
    ]);
    rule.filters = vec![RouteFilter {
        kind: "RequestMirror".into(),
        request_header_modifier: None,
        response_header_modifier: None,
        request_mirror: Some(Mirror {
            group: String::new(),
            kind: "Service".into(),
            namespace: None,
            name: "cart-shadow".into(),
            port: Some(80),
            percent: None,
            fraction: None,
        }),
    }];
    s.routes = vec![
        route_at("shop", "p", 1, vec![parent(None, "cart", None)], vec![rule]),
        // A producer route of a closure Service is not followed.
        route_at("shop", "v1", 1, vec![parent(None, "cart-v1", None)], vec![]),
    ];
    // `web/front` declares only `shop/cart`: the other two come from the closure.
    let b = view_of(&s, "web", "front");
    assert_eq!(
        keys(&b),
        [
            "shop/cart",
            "shop/cart-shadow",
            "shop/cart-v1",
            "web/app",
            "web/app2"
        ]
    );
    for k in ["shop/cart-v1", "shop/cart-shadow"] {
        let sv = &b.view.services[k];
        assert_eq!(sv.endpoints[&80].len(), 1, "{k}");
        assert!(sv.routes.is_empty(), "{k}");
    }
    assert_eq!(b.json.matches("\"shop/cart-v1\":{").count(), 1);
}

#[test]
fn rte_09_a_closure_service_of_the_callers_namespace_carries_its_own_routes() {
    let mut s = cluster();
    s.routes = vec![
        route_at(
            "web",
            "c",
            1,
            vec![parent(Some("shop"), "cart", None)],
            vec![to_backends(vec![backend(None, "app2", 80, None)])],
        ),
        route_at("web", "own", 1, vec![parent(None, "app2", None)], vec![]),
    ];
    let b = view_of(&s, "web", "front");
    assert_eq!(route_names(&b, "web/app2"), ["web/own"]);
    assert_eq!(route_names(&b, "shop/cart"), ["web/c"]);
}

#[test]
fn rte_09_a_backend_of_another_namespace_without_the_grant_adds_nothing_of_it() {
    let mut s = cluster();
    s.routes = vec![route_at(
        "shop",
        "p",
        1,
        vec![parent(None, "cart", None)],
        vec![to_backends(vec![backend(Some("risk"), "a", 80, Some(1))])],
    )];
    for caller in [("shop", "cart"), ("web", "front")] {
        let b = view_of(&s, caller.0, caller.1);
        for secret in ["risk", "10.96.0.4", "10.244.0.4"] {
            assert!(!json_has(&b, secret), "{caller:?} {secret}");
        }
        let rule = &b.view.services["shop/cart"].routes[0].rules[0];
        assert_eq!(rule.backends[0].invalid, Some(true));
    }
    // A MimicConfig grant does not count; an HTTPRoute one does.
    s.grants = Some(vec![grant("risk", "shop")]);
    assert!(!json_has(&view_of(&s, "shop", "cart"), "risk/a"));
    s.grants = Some(vec![route_grant("risk", "shop")]);
    let b = view_of(&s, "shop", "cart");
    assert!(b.view.services.contains_key("risk/a"));
    assert!(b.view.services["risk/a"].routes.is_empty());
}

#[test]
fn rte_10_closure_services_come_last_and_a_route_naming_one_left_out_is_dropped() {
    let mut s = cluster();
    s.grants = Some(vec![route_grant("risk", "shop")]);
    let to = |n: &str| to_backends(vec![backend(Some("risk"), n, 80, None)]);
    s.routes = vec![
        route_at(
            "shop",
            "r1",
            1,
            vec![parent(None, "cart", None)],
            vec![to("a")],
        ),
        route_at(
            "shop",
            "r2",
            2,
            vec![parent(None, "cart", None)],
            vec![to("b")],
        ),
    ];
    // Scope (3 Services of shop) plus one closure Service.
    let limits = Limits {
        max_services: 4,
        ..Limits::default()
    };
    let b = build(&s, "shop", "cart", Mode::None, &limits);
    assert_eq!(
        keys(&b),
        ["risk/a", "shop/cart", "shop/cart-shadow", "shop/cart-v1"]
    );
    assert_eq!(route_names(&b, "shop/cart"), ["shop/r1"]);
    assert_eq!(b.skipped_routes, ["shop/r2"]);
    assert!(
        !b.view.services["shop/cart"]
            .generations
            .contains_key("httproute/shop/r2@uid-r2")
    );
    assert!(!json_has(&b, "risk/b"));
    // The view reads back with the same limits.
    assert!(View::from_json(&b.json, &limits).is_ok());
    // With room for both nothing is dropped and `skipped_routes` is empty.
    let b = view_of(&s, "shop", "cart");
    assert_eq!(route_names(&b, "shop/cart"), ["shop/r1", "shop/r2"]);
    assert!(b.skipped_routes.is_empty());
}

#[test]
fn rte_10_a_route_naming_a_scope_service_left_out_is_dropped_and_listed() {
    let mut s = cluster();
    s.routes = vec![route_at(
        "shop",
        "r1",
        1,
        vec![parent(None, "cart", None)],
        vec![to_backends(vec![backend(None, "cart-v1", 80, None)])],
    )];
    // Only cart fits: cart-shadow and cart-v1 are skipped by the limit.
    let limits = Limits {
        max_services: 1,
        ..Limits::default()
    };
    let b = build(&s, "shop", "cart", Mode::None, &limits);
    assert_eq!(keys(&b), ["shop/cart"]);
    assert!(b.view.services["shop/cart"].routes.is_empty());
    assert_eq!(b.skipped_routes, ["shop/r1"]);
}

#[test]
fn rte_10_the_endpoint_and_byte_bounds_hold_for_closure_services() {
    let mut s = cluster();
    s.grants = Some(vec![route_grant("risk", "shop")]);
    s.routes = vec![route_at(
        "shop",
        "r1",
        1,
        vec![parent(None, "cart", None)],
        vec![to_backends(vec![backend(Some("risk"), "a", 80, None)])],
    )];
    let full = view_of(&s, "shop", "cart");
    assert!(full.view.services.contains_key("risk/a"));
    for limits in [
        Limits {
            max_endpoints: full.view.endpoint_count() - 1,
            ..Limits::default()
        },
        Limits {
            max_view_bytes: full.json.len() - 1,
            ..Limits::default()
        },
    ] {
        let b = build(&s, "shop", "cart", Mode::None, &limits);
        assert!(!b.view.services.contains_key("risk/a"));
        assert_eq!(b.skipped_routes, ["shop/r1"]);
        assert!(b.json.len() <= limits.max_view_bytes);
        assert!(View::from_json(&b.json, &limits).is_ok());
    }
}

#[test]
fn rte_10_at_most_16_skipped_routes_are_listed() {
    let mut s = cluster();
    s.routes = (0..20)
        .map(|i| {
            route_at(
                "shop",
                &format!("r{i:02}"),
                1,
                vec![parent(None, "cart", None)],
                vec![to_backends(vec![backend(None, "cart-v1", 80, None)])],
            )
        })
        .collect();
    let limits = Limits {
        max_services: 1,
        ..Limits::default()
    };
    let b = build(&s, "shop", "cart", Mode::None, &limits);
    assert_eq!(b.skipped_routes.len(), 16);
    assert_eq!(b.skipped_routes[0], "shop/r00");
}
