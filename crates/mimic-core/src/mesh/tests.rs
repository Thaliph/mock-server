//! MSH-01, MSH-02, MSH-04, MSH-05, MSH-06, MSH-10, MSH-11 (spec §12.21).

use sha2::{Digest, Sha256};

use super::input::*;
use super::testkit::*;
use super::view::{Limits, build, etag};
use crate::Mode;

fn shop() -> Snapshot {
    let mut s = snapshot();
    s.services = vec![
        http_svc("shop", "cart", "10.96.12.7"),
        svc(
            "shop",
            "db",
            "None",
            &[(5432, None, TargetPort::Number(5432))],
        ),
        http_svc("risk", "scoring", "10.96.50.1"),
    ];
    s.slices = vec![
        slice(
            "shop",
            "cart-x7k2p",
            "cart",
            41,
            8080,
            vec![
                zoned("10.244.1.17", "a"),
                ep("10.244.1.16", true),
                ep("10.244.1.9", false),
            ],
        ),
        slice(
            "risk",
            "scoring-a",
            "scoring",
            3,
            9000,
            vec![ep("10.244.9.1", true)],
        ),
    ];
    s.deployments = vec![("shop".into(), "cart".into())];
    s
}

#[test]
fn msh_01_the_view_holds_the_services_of_its_namespace_only() {
    let b = view_of(&shop(), "shop", "cart");
    assert_eq!(keys(&b), ["shop/cart", "shop/db"]);
    assert_eq!(b.truncated, 0);
}

#[test]
fn msh_01_a_service_has_its_ips_ports_and_ready_endpoints_sorted() {
    let b = view_of(&shop(), "shop", "cart");
    let cart = &b.view.services["shop/cart"];
    assert_eq!(
        cart.ips,
        ["10.96.12.7".parse::<std::net::IpAddr>().unwrap()]
    );
    assert_eq!(cart.ports[&80].name.as_deref(), Some("http"));
    assert_eq!(cart.ports[&80].target_port, TargetPort::Name("http".into()));
    let eps: Vec<_> = cart.endpoints[&80]
        .iter()
        .map(|e| (e.ip.to_string(), e.port, e.ready, e.zone.clone()))
        .collect();
    assert_eq!(
        eps,
        [
            ("10.244.1.16".to_string(), 8080, true, None),
            ("10.244.1.17".to_string(), 8080, true, Some("a".to_string())),
        ]
    );
}

#[test]
fn msh_01_a_headless_service_has_no_ip_and_an_unnamed_port_matches_an_unnamed_slice_port() {
    let mut s = shop();
    s.slices.push(EndpointSlice {
        ports: vec![SlicePort {
            name: None,
            port: 5432,
        }],
        ..slice("shop", "db-1", "db", 1, 0, vec![ep("10.244.2.2", true)])
    });
    let b = view_of(&s, "shop", "cart");
    let db = &b.view.services["shop/db"];
    assert!(db.ips.is_empty());
    assert_eq!(db.endpoints[&5432].len(), 1);
    assert_eq!(db.endpoints[&5432][0].port, 5432);
}

#[test]
fn msh_01_terminating_endpoints_serve_only_when_none_is_ready() {
    let mut s = shop();
    let terminating = Endpoint {
        serving: true,
        terminating: true,
        ..ep("10.244.1.50", false)
    };
    let gone = Endpoint {
        serving: false,
        terminating: true,
        ..ep("10.244.1.51", false)
    };
    s.slices[0].endpoints = vec![ep("10.244.1.52", false), terminating.clone(), gone.clone()];
    let b = view_of(&s, "shop", "cart");
    let eps = &b.view.services["shop/cart"].endpoints[&80];
    assert_eq!(eps.len(), 1);
    assert_eq!(
        (eps[0].ip.to_string(), eps[0].ready),
        ("10.244.1.50".to_string(), false)
    );
    s.slices[0].endpoints.push(ep("10.244.1.53", true));
    let b = view_of(&s, "shop", "cart");
    let eps = &b.view.services["shop/cart"].endpoints[&80];
    assert_eq!(eps.len(), 1);
    assert!(eps[0].ready);
}

#[test]
fn msh_01_bad_addresses_duplicates_other_port_names_and_other_services_are_left_out() {
    let mut s = shop();
    s.slices[0].endpoints = vec![
        ep("not-an-ip", true),
        ep("10.244.1.1", true),
        ep("10.244.1.1", true),
    ];
    s.slices[0].ports.push(SlicePort {
        name: Some("metrics".into()),
        port: 9102,
    });
    s.slices.push(slice(
        "shop",
        "other-1",
        "db",
        1,
        7777,
        vec![ep("10.244.7.7", true)],
    ));
    s.slices.push(slice(
        "risk",
        "cart-1",
        "cart",
        1,
        7777,
        vec![ep("10.244.8.8", true)],
    ));
    let b = view_of(&s, "shop", "cart");
    let eps = &b.view.services["shop/cart"].endpoints[&80];
    assert_eq!(eps.len(), 1);
    assert_eq!(
        (eps[0].ip.to_string(), eps[0].port),
        ("10.244.1.1".to_string(), 8080)
    );
    assert!(!b.json.contains("7777") && !b.json.contains("9102"));
}

#[test]
fn msh_02_same_namespace_and_consented_destinations_are_in_the_view() {
    let mut s = shop();
    s.configs = vec![dep_cfg(
        "shop",
        "c",
        1,
        "cart",
        &["shop/db", "risk/scoring"],
    )];
    s.grants = Some(vec![grant("risk", "shop")]);
    let b = view_of(&s, "shop", "cart");
    assert_eq!(keys(&b), ["risk/scoring", "shop/cart", "shop/db"]);
    assert_eq!(b.view.services["risk/scoring"].endpoints[&80][0].port, 9000);
}

#[test]
fn msh_02_destinations_of_a_loser_an_invalid_config_or_another_deployment_add_nothing() {
    let mut s = shop();
    s.grants = Some(vec![grant("risk", "shop")]);
    s.deployments.push(("shop".into(), "other".into()));
    // The winner declares nothing; the newer config declares the Service and loses.
    let mut targets: Vec<(TargetKind, String)> = vec![(TargetKind::Deployment, "cart".into())];
    targets.extend((0..16).map(|i| (TargetKind::Service, format!("s{i}"))));
    let refs: Vec<(TargetKind, &str)> = targets.iter().map(|(k, n)| (*k, n.as_str())).collect();
    s.configs = vec![
        cfg("shop", "a-winner", 1, &[(TargetKind::Deployment, "cart")]),
        dep_cfg("shop", "b-loser", 2, "cart", &["risk/scoring"]),
        dep_cfg("shop", "c-other", 0, "other", &["risk/scoring"]),
        Config {
            destinations: Some(vec![Destination {
                namespace: "risk".into(),
                name: "scoring".into(),
            }]),
            ..cfg("shop", "d-invalid", 0, &refs)
        },
    ];
    let b = view_of(&s, "shop", "cart");
    assert_eq!(keys(&b), ["shop/cart", "shop/db"]);
    let b = view_of(&s, "shop", "other");
    assert_eq!(keys(&b), ["risk/scoring", "shop/cart", "shop/db"]);
}

#[test]
fn msh_02_a_missing_service_adds_nothing_even_with_a_grant() {
    let mut s = shop();
    s.configs = vec![dep_cfg("shop", "c", 1, "cart", &["risk/absent"])];
    s.grants = Some(vec![grant("risk", "shop")]);
    assert_eq!(keys(&view_of(&s, "shop", "cart")), ["shop/cart", "shop/db"]);
}

fn secret_namespace(s: &mut Snapshot) {
    s.services.push(svc(
        "secret",
        "vault",
        "10.99.9.9",
        &[(9999, Some("http"), TargetPort::Number(9999))],
    ));
    s.slices.push(slice(
        "secret",
        "vault-1",
        "vault",
        7,
        9998,
        vec![zoned("10.99.8.8", "zone-secret-x")],
    ));
}

#[test]
fn msh_04_a_namespace_without_a_grant_for_the_source_is_absent_from_the_view() {
    let mut s = shop();
    secret_namespace(&mut s);
    // `secret` is declared but grants only `other`; `risk` grants `shop`.
    s.configs = vec![dep_cfg(
        "shop",
        "c",
        1,
        "cart",
        &["secret/vault", "risk/scoring"],
    )];
    s.grants = Some(vec![grant("secret", "other"), grant("risk", "shop")]);
    let b = view_of(&s, "shop", "cart");
    for leak in [
        "10.99.9.9",
        "10.99.8.8",
        "9999",
        "9998",
        "zone-secret-x",
        "vault",
        "secret",
    ] {
        assert!(!b.json.contains(leak), "the view names another namespace");
    }
    assert!(b.view.services.contains_key("risk/scoring"));
}

#[test]
fn msh_04_a_config_of_another_namespace_never_changes_the_view() {
    let mut s = shop();
    secret_namespace(&mut s);
    s.deployments.push(("secret".into(), "cart".into()));
    let before = view_of(&s, "shop", "cart");
    let mut foreign = dep_cfg("secret", "cart", 0, "cart", &["shop/db"]);
    foreign.mesh_enabled = Some(false);
    s.configs = vec![
        foreign,
        cfg("secret", "svc", 0, &[(TargetKind::Service, "cart")]),
    ];
    s.grants = Some(vec![grant("shop", "secret")]);
    assert_eq!(view_of(&s, "shop", "cart"), before);
}

#[test]
fn msh_05_mesh_is_true_unless_the_winning_config_says_false() {
    let mut s = shop();
    assert!(view_of(&s, "shop", "cart").view.mesh);
    let mut off = cfg("shop", "off", 5, &[(TargetKind::Deployment, "cart")]);
    off.mesh_enabled = Some(false);
    s.configs = vec![off.clone()];
    let b = view_of(&s, "shop", "cart");
    assert!(!b.view.mesh);
    assert!(b.view.services.is_empty() && b.view.generations.is_empty());
    // An older config that does not say wins as a whole: still on.
    s.configs
        .push(cfg("shop", "on", 1, &[(TargetKind::Deployment, "cart")]));
    assert!(view_of(&s, "shop", "cart").view.mesh);
    // An older config that says false wins.
    s.configs = vec![
        Config {
            created: 1,
            name: "older".into(),
            ..off
        },
        {
            let mut on = cfg("shop", "newer", 9, &[(TargetKind::Deployment, "cart")]);
            on.mesh_enabled = Some(true);
            on
        },
    ];
    assert!(!view_of(&s, "shop", "cart").view.mesh);
}

#[test]
fn msh_05_the_mode_and_cluster_are_in_the_view() {
    let b = build(&shop(), "shop", "cart", Mode::Mock, &Limits::default());
    assert_eq!(
        (b.view.mode.as_str(), b.view.cluster.as_str()),
        ("mock", "local")
    );
    assert_eq!(b.view.format, 1);
}

#[test]
fn msh_06_generations_list_the_slices_of_each_service_and_the_winning_config() {
    let mut s = shop();
    s.configs = vec![
        cfg("shop", "w", 1, &[(TargetKind::Deployment, "cart")]),
        cfg("shop", "l", 2, &[(TargetKind::Deployment, "cart")]),
    ];
    s.configs[0].generation = 3;
    let b = view_of(&s, "shop", "cart");
    assert_eq!(b.view.generations.len(), 1);
    assert_eq!(b.view.generations["mimicconfig/shop/w@uid-w"], 3);
    let cart = &b.view.services["shop/cart"];
    assert_eq!(cart.generations.len(), 1);
    assert_eq!(
        cart.generations["endpointslice/shop/cart-x7k2p@uid-cart-x7k2p"],
        41
    );
    assert!(b.view.services["shop/db"].generations.is_empty());
    s.slices[0].generation = 42;
    assert_ne!(view_of(&s, "shop", "cart").etag, b.etag);
}

const GOLDEN: &str = concat!(
    r#"{"cluster":"local","format":1,"generations":{},"mesh":true,"mode":"none","services":{"shop/cart":{"#,
    r#""endpoints":{"80":[{"ip":"10.244.1.17","port":8080,"ready":true,"zone":"a"}]},"#,
    r#""generations":{"endpointslice/shop/cart-x7k2p@uid-cart-x7k2p":41},"ips":["10.96.12.7"],"#,
    r#""policy":{"loadBalancer":{"slowStartSeconds":30},"retries":3,"retryBudget":{"minPerSecond":10,"percent":20,"windowSeconds":10}},"ports":{"80":{"name":"http","targetPort":"http"}}}}}"#
);

#[test]
fn msh_10_the_json_is_canonical_and_the_etag_is_the_sha256_of_it() {
    let mut s = snapshot();
    s.services = vec![http_svc("shop", "cart", "10.96.12.7")];
    s.slices = vec![slice(
        "shop",
        "cart-x7k2p",
        "cart",
        41,
        8080,
        vec![zoned("10.244.1.17", "a")],
    )];
    let b = view_of(&s, "shop", "cart");
    assert_eq!(b.json, GOLDEN);
    assert_eq!(b.view.to_json(), GOLDEN);
    let digest: String = Sha256::digest(GOLDEN.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(b.etag, format!("\"1-{digest}\""));
    assert_eq!(etag(GOLDEN), b.etag);
    assert_eq!(view_of(&s, "shop", "cart"), b);
}

#[test]
fn msh_10_content_mode_and_cluster_change_the_etag() {
    let s = shop();
    let base = view_of(&s, "shop", "cart").etag;
    let mode = build(&s, "shop", "cart", Mode::Discovery, &Limits::default()).etag;
    let mut other = s.clone();
    other.cluster = "east".into();
    let mut moved = s.clone();
    moved.slices[0].endpoints.push(ep("10.244.1.99", true));
    for e in [
        mode,
        view_of(&other, "shop", "cart").etag,
        view_of(&moved, "shop", "cart").etag,
    ] {
        assert_ne!(e, base);
    }
}

fn many(n: usize, endpoints: usize) -> Snapshot {
    let mut s = snapshot();
    for i in 0..n {
        let name = format!("s{i:02}");
        s.services
            .push(http_svc("shop", &name, &format!("10.96.0.{}", i + 1)));
        let eps = (0..endpoints)
            .map(|e| ep(&format!("10.244.{i}.{}", e + 1), true))
            .collect();
        s.slices
            .push(slice("shop", &format!("{name}-1"), &name, 1, 8080, eps));
    }
    s
}

#[test]
fn msh_11_the_services_of_the_namespace_come_first_then_destinations_in_key_order() {
    let mut s = many(5, 1);
    for ns in ["alpha", "risk"] {
        s.services.push(http_svc(ns, "x", "10.97.0.1"));
        s.grants.as_mut().unwrap().push(grant(ns, "shop"));
    }
    s.configs = vec![dep_cfg("shop", "c", 1, "s00", &["risk/x", "alpha/x"])];
    let limits = |max_services| Limits {
        max_services,
        ..Limits::default()
    };
    let b = build(&s, "shop", "s00", Mode::None, &limits(4));
    assert_eq!(keys(&b), ["shop/s00", "shop/s01", "shop/s02", "shop/s03"]);
    assert_eq!(b.truncated, 3);
    let b = build(&s, "shop", "s00", Mode::None, &limits(6));
    assert_eq!(
        keys(&b),
        [
            "alpha/x", "shop/s00", "shop/s01", "shop/s02", "shop/s03", "shop/s04"
        ]
    );
    assert_eq!(b.truncated, 1);
    let b = build(&s, "shop", "s00", Mode::None, &limits(7));
    assert_eq!(b.truncated, 0);
}

#[test]
fn msh_11_the_first_service_over_the_endpoint_bound_ends_the_list() {
    let mut s = many(4, 3);
    s.slices[2].endpoints.clear(); // s02 adds none and fits; s03 would make 9 > 7
    let limits = Limits {
        max_endpoints: 7,
        ..Limits::default()
    };
    let b = build(&s, "shop", "s00", Mode::None, &limits);
    assert_eq!(keys(&b), ["shop/s00", "shop/s01", "shop/s02"]);
    assert_eq!(b.truncated, 1);
    assert!(b.view.endpoint_count() <= 7);
}

#[test]
fn msh_11_the_view_never_exceeds_the_byte_limit() {
    let s = many(5, 2);
    let full = view_of(&s, "shop", "s00");
    let exact = Limits {
        max_view_bytes: full.json.len(),
        ..Limits::default()
    };
    assert_eq!(build(&s, "shop", "s00", Mode::None, &exact).truncated, 0);
    let under = Limits {
        max_view_bytes: full.json.len() - 1,
        ..Limits::default()
    };
    let b = build(&s, "shop", "s00", Mode::None, &under);
    assert_eq!((b.view.services.len(), b.truncated), (4, 1));
    assert!(b.json.len() <= under.max_view_bytes);
    let tiny = Limits {
        max_view_bytes: 10,
        ..Limits::default()
    };
    let b = build(&s, "shop", "s00", Mode::None, &tiny);
    assert_eq!((b.view.services.len(), b.truncated), (0, 5));
}

#[test]
fn msh_11_below_the_bounds_nothing_is_truncated() {
    assert_eq!(view_of(&many(20, 5), "shop", "s00").truncated, 0);
}

/// `many` with the endpoints of `big` multiplied.
fn with_big(n: usize, big: usize, big_endpoints: usize) -> Snapshot {
    let mut s = many(n, 1);
    let name = format!("s{big:02}");
    let eps = (0..big_endpoints)
        .map(|e| ep(&format!("10.250.0.{}", e + 1), true))
        .collect();
    s.slices.retain(|sl| sl.service != name);
    s.slices
        .push(slice("shop", &format!("{name}-1"), &name, 1, 8080, eps));
    s
}

#[test]
fn msh_11_a_large_service_is_skipped_and_the_small_ones_after_it_still_fit() {
    let s = with_big(5, 1, 50); // s01 alone has 50 endpoints
    let limits = Limits {
        max_endpoints: 8,
        ..Limits::default()
    };
    let b = build(&s, "shop", "s00", Mode::None, &limits);
    assert_eq!(keys(&b), ["shop/s00", "shop/s02", "shop/s03", "shop/s04"]);
    assert_eq!(b.truncated, 1);
    assert_eq!(b.skipped, ["shop/s01"]);
    assert!(b.view.endpoint_count() <= 8);
}

#[test]
fn msh_11_a_service_over_the_byte_budget_is_skipped_and_the_etag_is_stable() {
    let s = with_big(5, 2, 40);
    let small = view_of(&many(5, 1), "shop", "s00");
    let limits = Limits {
        max_view_bytes: small.json.len() + 100,
        ..Limits::default()
    };
    let b = build(&s, "shop", "s00", Mode::None, &limits);
    assert!(!keys(&b).contains(&"shop/s02"));
    assert!(keys(&b).contains(&"shop/s03"));
    assert_eq!(b.skipped, ["shop/s02"]);
    assert!(b.json.len() <= limits.max_view_bytes);
    let again = build(&s, "shop", "s00", Mode::None, &limits);
    assert_eq!((&b.json, &b.etag), (&again.json, &again.etag));
}

#[test]
fn msh_11_the_skipped_names_are_bounded_and_the_count_is_not() {
    let s = many(40, 1);
    let limits = Limits {
        max_services: 2,
        ..Limits::default()
    };
    let b = build(&s, "shop", "s00", Mode::None, &limits);
    assert_eq!(b.truncated, 38);
    assert_eq!(b.skipped.len(), 16);
    assert_eq!(
        (b.skipped[0].as_str(), b.skipped[15].as_str()),
        ("shop/s02", "shop/s17")
    );
}

#[test]
fn rty_45_the_policy_holds_retries_and_every_budget_field_and_a_change_moves_the_etag() {
    let mut s = snapshot();
    s.services = vec![http_svc("shop", "cart", "10.96.12.7")];
    let base = view_of(&s, "shop", "cart");
    assert!(
        base.json.contains(
            r#""policy":{"loadBalancer":{"slowStartSeconds":30},"retries":3,"retryBudget":{"minPerSecond":10,"percent":20,"windowSeconds":10}}"#
        ),
        "{}",
        base.json
    );
    assert!(base.view.services["shop/cart"].generations.is_empty());
    let mut c = cfg("shop", "c", 1, &[(TargetKind::Service, "cart")]);
    c.retries = Some(1);
    s.configs = vec![c.clone()];
    let first = view_of(&s, "shop", "cart");
    assert!(first.json.contains(r#""retries":1,"retryBudget":{"minPerSecond":10"#));
    assert_ne!(first.etag, base.etag);
    assert_eq!(
        first.view.services["shop/cart"].generations["mimicconfig/shop/c@uid-c"],
        1
    );
    c.retries = Some(2);
    c.generation = 2;
    s.configs = vec![c];
    let second = view_of(&s, "shop", "cart");
    assert_ne!(second.etag, first.etag);
    assert_eq!(
        second.view.services["shop/cart"].generations["mimicconfig/shop/c@uid-c"],
        2
    );
}
