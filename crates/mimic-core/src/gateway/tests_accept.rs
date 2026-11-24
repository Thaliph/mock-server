//! GW-03 and GW-04: the acceptance of a GatewayClass, a Gateway and its listeners (spec §7.10
//! "Acceptance").

use super::REPLICAS_ANNOTATION;
use super::accept::{Acceptance, class as accept_class, gateway};
use super::input::*;
use super::testkit::*;
use crate::mesh::input::GATEWAY_GROUP;
use crate::mesh::policy::Condition;

fn cond(ok: bool, reason: &str, message: &str) -> Condition {
    Condition {
        ok,
        reason: reason.into(),
        message: message.into(),
    }
}

fn accept(g: &Gateway) -> Acceptance {
    gateway(g, &class(), &settings())
}

fn with_replicas(value: &str) -> Gateway {
    let mut g = gw();
    g.infrastructure.annotation_keys = vec![REPLICAS_ANNOTATION.into()];
    g.infrastructure.replicas = Some(value.into());
    g
}

fn refs_class() -> GatewayClass {
    GatewayClass {
        has_parameters_ref: true,
        ..class()
    }
}

const ANNOTATION_RANGE: &str =
    "spec.infrastructure.annotations[mimic.platform.io/replicas] must be 1 to 10";

#[test]
fn gw_03_each_case_gives_its_status_reason_and_message() {
    let unsupported = |path: &str| cond(false, "UnsupportedValue", &format!("unsupported: {path}"));
    let mut cases: Vec<(&str, Gateway, GatewayClass, Condition)> = vec![];

    // 1, with other faults too.
    let mut g = gw();
    g.namespace = "other".into();
    g.has_tls = true;
    g.has_addresses = true;
    cases.push((
        "namespace",
        g,
        refs_class(),
        cond(
            false,
            "NamespaceNotAllowed",
            "namespace other is not in gateway.namespaces",
        ),
    ));
    // 2.
    cases.push((
        "class",
        gw(),
        refs_class(),
        cond(
            false,
            "InvalidParameters",
            "GatewayClass mimic is not accepted",
        ),
    ));
    // 3.
    let mut g = gw();
    g.name = "a".repeat(64);
    cases.push((
        "long name",
        g,
        class(),
        unsupported("name longer than 63 characters"),
    ));
    let mut g = gw();
    g.oversize = true;
    g.listeners = vec![];
    cases.push((
        "oversize",
        g,
        class(),
        unsupported("gateway larger than 64 KiB"),
    ));
    let mut g = gw();
    g.unknown_fields = vec!["spec.defaultScope".into()];
    cases.push(("defaultScope", g, class(), unsupported("spec.defaultScope")));
    let mut g = gw();
    g.has_tls = true;
    cases.push(("tls", g, class(), unsupported("spec.tls")));
    let mut g = gw();
    g.infrastructure.has_labels = true;
    cases.push((
        "labels",
        g,
        class(),
        unsupported("spec.infrastructure.labels"),
    ));
    for from in ["All", "Same", "Selector"] {
        let mut g = gw();
        g.allowed_listeners_from = Some(from.into());
        cases.push((
            "allowedListeners",
            g,
            class(),
            unsupported("spec.allowedListeners.namespaces.from"),
        ));
    }
    let mut g = gw();
    g.infrastructure.annotation_keys = vec!["example.com/x".into()];
    cases.push((
        "annotation",
        g,
        class(),
        unsupported("spec.infrastructure.annotations[example.com/x]"),
    ));
    let mut g = gw();
    g.listeners = (0..65).map(|i| http(&format!("l{i}"), 80)).collect();
    cases.push(("65 listeners", g, class(), unsupported("spec.listeners")));
    // 4.
    let mut g = gw();
    g.has_addresses = true;
    cases.push((
        "addresses",
        g,
        class(),
        cond(
            false,
            "UnsupportedAddress",
            "spec.addresses is not supported",
        ),
    ));
    // 5.
    let mut g = gw();
    g.infrastructure.has_parameters_ref = true;
    cases.push((
        "infrastructure parametersRef",
        g,
        class(),
        cond(
            false,
            "InvalidParameters",
            "spec.infrastructure.parametersRef is not supported",
        ),
    ));
    let mut g = with_replicas("0");
    g.infrastructure.has_parameters_ref = true;
    cases.push((
        "replicas and parametersRef",
        g,
        class(),
        cond(false, "InvalidParameters", ANNOTATION_RANGE),
    ));
    // 6.
    let mut g = gw();
    g.listeners = vec![];
    cases.push((
        "no listener",
        g,
        class(),
        cond(false, "ListenersNotValid", "no valid listener"),
    ));
    let mut g = gw();
    g.listeners = vec![listener("tcp", "TCP", 9000)];
    cases.push((
        "only tcp",
        g,
        class(),
        cond(false, "ListenersNotValid", "no valid listener"),
    ));
    // 7.
    let mut g = gw();
    g.listeners.push(listener("tcp", "TCP", 9000));
    cases.push((
        "http and tcp",
        g,
        class(),
        cond(true, "ListenersNotValid", "listeners not valid: tcp"),
    ));
    // 8.
    cases.push(("valid", gw(), class(), cond(true, "Accepted", "")));
    let mut g = gw();
    g.allowed_listeners_from = Some("None".into());
    cases.push(("from None", g, class(), cond(true, "Accepted", "")));

    for (case, g, c, want) in cases {
        let got = gateway(&g, &c, &settings());
        assert_eq!(got.accepted, want, "case {case}");
        assert_eq!(got.is_accepted(), want.ok, "case {case}");
    }
}

#[test]
fn gw_03_the_first_case_wins() {
    let faulty = || {
        let mut g = gw();
        g.namespace = "other".into();
        g.has_tls = true;
        g.has_addresses = true;
        g.infrastructure.has_parameters_ref = true;
        g.listeners = vec![listener("tcp", "TCP", 9000)];
        g
    };
    let mut g = faulty();
    let mut c = refs_class();
    let mut got = vec![gateway(&g, &c, &settings()).accepted];
    g.namespace = "shop".into();
    got.push(gateway(&g, &c, &settings()).accepted);
    c = class();
    got.push(gateway(&g, &c, &settings()).accepted);
    g.has_tls = false;
    got.push(gateway(&g, &c, &settings()).accepted);
    g.has_addresses = false;
    got.push(gateway(&g, &c, &settings()).accepted);
    g.infrastructure.has_parameters_ref = false;
    got.push(gateway(&g, &c, &settings()).accepted);
    assert_eq!(
        got,
        vec![
            cond(
                false,
                "NamespaceNotAllowed",
                "namespace other is not in gateway.namespaces"
            ),
            cond(
                false,
                "InvalidParameters",
                "GatewayClass mimic is not accepted"
            ),
            cond(false, "UnsupportedValue", "unsupported: spec.tls"),
            cond(
                false,
                "UnsupportedAddress",
                "spec.addresses is not supported"
            ),
            cond(
                false,
                "InvalidParameters",
                "spec.infrastructure.parametersRef is not supported"
            ),
            cond(false, "ListenersNotValid", "no valid listener"),
        ]
    );
}

#[test]
fn gw_03_case_3_names_the_first_field_in_document_order() {
    let message = |g: &Gateway| accept(g).accepted.message;

    let mut g = gw();
    g.has_tls = true;
    g.infrastructure.has_labels = true;
    assert_eq!(message(&g), "unsupported: spec.infrastructure.labels");

    let mut g = gw();
    g.unknown_fields = vec!["spec.defaultScope".into()];
    g.has_tls = true;
    assert_eq!(message(&g), "unsupported: spec.defaultScope");

    let mut g = gw();
    g.unknown_fields = vec!["spec.zeta".into()];
    g.has_tls = true;
    assert_eq!(message(&g), "unsupported: spec.tls");

    let mut g = gw();
    g.allowed_listeners_from = Some("All".into());
    g.unknown_fields = vec!["spec.defaultScope".into()];
    assert_eq!(
        message(&g),
        "unsupported: spec.allowedListeners.namespaces.from"
    );

    let mut g = gw();
    g.infrastructure.annotation_keys = vec![
        "a.example/x".into(),
        REPLICAS_ANNOTATION.into(),
        "z.example/y".into(),
    ];
    g.infrastructure.replicas = Some("3".into());
    assert_eq!(
        message(&g),
        "unsupported: spec.infrastructure.annotations[a.example/x]"
    );

    let mut g = gw();
    g.listeners = (0..65).map(|i| http(&format!("l{i}"), 80)).collect();
    g.has_tls = true;
    assert_eq!(message(&g), "unsupported: spec.listeners");

    let mut g = gw();
    g.name = "a".repeat(64);
    g.has_tls = true;
    assert_eq!(message(&g), "unsupported: name longer than 63 characters");
}

#[test]
fn gw_03_replicas_values() {
    let digits20 = "1".repeat(20);
    for value in [
        "0",
        "11",
        "+3",
        "03",
        "2.0",
        "two",
        "",
        " 3",
        digits20.as_str(),
    ] {
        assert_eq!(
            accept(&with_replicas(value)).accepted,
            cond(false, "InvalidParameters", ANNOTATION_RANGE),
            "replicas {value:?}"
        );
    }
    for (value, want) in [("1", 1), ("3", 3), ("10", 10)] {
        let got = accept(&with_replicas(value));
        assert_eq!(got.accepted, cond(true, "Accepted", ""), "replicas {value}");
        assert_eq!(got.replicas, want, "replicas {value}");
    }
}

#[test]
fn gw_03_default_replicas_is_capped_by_the_maximum() {
    for (max, want) in [(1, 1), (2, 2), (10, 2)] {
        let mut s = settings();
        s.max_replicas = max;
        let got = gateway(&gw(), &class(), &s);
        assert!(got.is_accepted(), "max {max}");
        assert_eq!(got.replicas, want, "max {max}");
    }
}

#[test]
fn gw_03_no_message_holds_a_hostname_or_annotation_value() {
    const CANARY: &str = "zqxcanaryzq";
    let base = || {
        let mut g = gw();
        let mut tcp = listener("tcp", "TCP", 9000);
        tcp.hostname = Some(format!("{CANARY}.example"));
        g.listeners[0].hostname = Some(format!("{CANARY}.example"));
        g.listeners.push(tcp);
        g.infrastructure.annotation_keys = vec![REPLICAS_ANNOTATION.into()];
        g.infrastructure.replicas = Some(CANARY.into());
        g
    };
    let mut gateways = vec![base()];
    let mut g = base();
    g.namespace = "other".into();
    gateways.push(g);
    let mut g = base();
    g.has_tls = true;
    gateways.push(g);
    let mut g = base();
    g.infrastructure
        .annotation_keys
        .insert(0, "example.com/x".into());
    gateways.push(g);
    let mut g = base();
    g.has_addresses = true;
    gateways.push(g);
    let mut g = base();
    g.infrastructure.replicas = Some("2".into());
    gateways.push(g);
    let mut g = base();
    g.infrastructure.replicas = Some("2".into());
    g.listeners[0].port = 15100;
    g.listeners[1].port = 0;
    g.listeners[1].protocol = "HTTP".into();
    gateways.push(g);
    for (i, g) in gateways.iter().enumerate() {
        for c in [class(), refs_class()] {
            let got = gateway(g, &c, &settings());
            let mut messages = vec![got.accepted.message.clone()];
            for l in &got.listeners {
                messages.push(l.accepted.message.clone());
                messages.push(l.resolved_refs.message.clone());
                messages.push(l.conflicted.message.clone());
            }
            assert!(
                messages.iter().all(|m| !m.contains(CANARY)),
                "a message of gateway {i} holds the listener hostname or the annotation value"
            );
        }
    }
}

#[test]
fn gw_03_gateway_class_acceptance() {
    assert_eq!(accept_class(&class()), cond(true, "Accepted", ""));
    assert_eq!(
        accept_class(&refs_class()),
        cond(false, "InvalidParameters", "parametersRef is not supported")
    );
    let long = GatewayClass {
        name: "a".repeat(64),
        ..refs_class()
    };
    assert_eq!(
        accept_class(&long),
        cond(false, "Unsupported", "name longer than 63 characters")
    );
    let edge = GatewayClass {
        name: "a".repeat(63),
        ..class()
    };
    assert_eq!(accept_class(&edge), cond(true, "Accepted", ""));
    // A Gateway of a class that is not accepted gets case 2.
    let long = GatewayClass {
        has_parameters_ref: false,
        ..long
    };
    assert_eq!(
        gateway(&gw(), &long, &settings()).accepted,
        cond(
            false,
            "InvalidParameters",
            &format!("GatewayClass {} is not accepted", "a".repeat(64))
        )
    );
}

#[test]
fn gw_04_listener_protocols_and_port() {
    let first = |l: Listener| {
        let mut g = gw();
        g.listeners = vec![l];
        accept(&g).listeners[0].accepted.clone()
    };
    assert_eq!(first(http("http", 80)), cond(true, "Accepted", ""));
    for p in ["HTTPS", "TLS", "TCP", "UDP", "example.com/proto", "http"] {
        assert_eq!(
            first(listener("l", p, 80)),
            cond(
                false,
                "UnsupportedProtocol",
                &format!("protocol {p} is not supported")
            ),
            "protocol {p}"
        );
    }
    assert_eq!(
        first(http("admin", 15100)),
        cond(
            false,
            "PortUnavailable",
            "port 15100 is the gateway admin port"
        )
    );
    assert_eq!(
        first(listener("tcp", "TCP", 15100)),
        cond(
            false,
            "UnsupportedProtocol",
            "protocol TCP is not supported"
        )
    );
}

#[test]
fn gw_04_unsupported_listener_fields_name_their_path() {
    // The listener under test is the second one, so that its index is checked.
    let second = |edit: &dyn Fn(&mut Listener)| {
        let mut l = http("b", 8080);
        edit(&mut l);
        let mut g = gw();
        g.listeners.push(l);
        accept(&g).listeners[1].accepted.clone()
    };
    let unsupported = |path: &str| cond(false, "UnsupportedValue", &format!("unsupported: {path}"));
    assert_eq!(
        second(&|l| l.unknown_fields = vec!["spec.listeners[1].filters".into()]),
        unsupported("spec.listeners[1].filters")
    );
    assert_eq!(
        second(&|l| l.unknown_fields = vec!["spec.listeners[1].x".into()]),
        unsupported("spec.listeners[1].x")
    );
    for from in ["Selector", "Other"] {
        assert_eq!(
            second(&|l| l.allowed_from = Some(from.into())),
            unsupported("spec.listeners[1].allowedRoutes.namespaces.from"),
            "from {from}"
        );
    }
    assert_eq!(
        second(&|l| l.port = 0),
        unsupported("spec.listeners[1].port")
    );
    // Document order.
    assert_eq!(
        second(&|l| {
            l.unknown_fields = vec!["spec.listeners[1].filters".into()];
            l.allowed_from = Some("Selector".into());
        }),
        unsupported("spec.listeners[1].allowedRoutes.namespaces.from")
    );
    assert_eq!(
        second(&|l| {
            l.unknown_fields = vec!["spec.listeners[1].x".into()];
            l.port = 0;
        }),
        unsupported("spec.listeners[1].port")
    );
    for from in ["Same", "All"] {
        assert_eq!(
            second(&|l| l.allowed_from = Some(from.into())),
            cond(true, "Accepted", ""),
            "from {from}"
        );
    }
}

#[test]
fn gw_04_supported_kinds_and_resolved_refs() {
    let invalid = cond(false, "InvalidRouteKinds", "only HTTPRoute is supported");
    let resolved = cond(true, "ResolvedRefs", "");
    let cases = [
        (None, vec![http_route()], resolved.clone()),
        (Some(vec![]), vec![http_route()], resolved.clone()),
        (
            Some(vec![http_route(), kind(GATEWAY_GROUP, "TCPRoute")]),
            vec![http_route()],
            invalid.clone(),
        ),
        (
            Some(vec![kind(GATEWAY_GROUP, "GRPCRoute")]),
            vec![],
            invalid.clone(),
        ),
        (Some(vec![kind("", "HTTPRoute")]), vec![], invalid.clone()),
        (
            Some(vec![http_route(), http_route()]),
            vec![http_route()],
            resolved.clone(),
        ),
    ];
    for (kinds, supported, refs) in cases {
        let mut l = http("http", 80);
        l.kinds = kinds.clone();
        let mut g = gw();
        g.listeners = vec![l];
        let got = accept(&g);
        let l = &got.listeners[0];
        assert_eq!(l.supported_kinds, supported, "kinds {kinds:?}");
        assert_eq!(l.resolved_refs, refs, "kinds {kinds:?}");
        assert_eq!(l.accepted, cond(true, "Accepted", ""), "kinds {kinds:?}");
        assert!(got.is_accepted(), "kinds {kinds:?}");
    }
}

#[test]
fn gw_04_conflicted_and_attached_routes_everywhere() {
    let mut g = gw();
    g.listeners = vec![
        http("http", 80),
        listener("tcp", "TCP", 9000),
        http("admin", 15100),
    ];
    let mut refused = g.clone();
    refused.namespace = "other".into();
    for g in [g, refused] {
        let got = accept(&g);
        let names: Vec<&str> = got.listeners.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["http", "tcp", "admin"]);
        for l in &got.listeners {
            assert_eq!(l.conflicted, cond(false, "NoConflicts", ""), "{}", l.name);
            assert_eq!(l.attached_routes, 0, "{}", l.name);
        }
        let ports: Vec<(u16, &str)> = got
            .listeners
            .iter()
            .map(|l| (l.port, l.protocol.as_str()))
            .collect();
        assert_eq!(ports, [(80, "HTTP"), (9000, "TCP"), (15100, "HTTP")]);
    }
}

#[test]
fn gw_04_some_or_all_listeners_invalid() {
    let mut g = gw();
    g.listeners = vec![
        listener("tcp", "TCP", 9000),
        http("http", 80),
        listener("udp", "UDP", 9001),
    ];
    assert_eq!(
        accept(&g).accepted,
        cond(true, "ListenersNotValid", "listeners not valid: tcp, udp")
    );

    g.listeners = vec![listener("tcp", "TCP", 9000)];
    assert_eq!(
        accept(&g).accepted,
        cond(false, "ListenersNotValid", "no valid listener")
    );

    g.listeners = (0..64)
        .map(|i| {
            let name = format!("{i:02}{}", "n".repeat(251));
            if i == 0 {
                http(&name, 80)
            } else {
                listener(&name, "TCP", 9000)
            }
        })
        .collect();
    let got = accept(&g).accepted;
    assert!(got.ok);
    assert_eq!(got.reason, "ListenersNotValid");
    assert!(got.message.starts_with("listeners not valid: 01nnn"));
    assert_eq!(got.message.len(), 1024);
}

#[test]
fn gw_07_ports_are_the_distinct_accepted_ports_ascending() {
    let mut g = gw();
    g.listeners = vec![
        http("alt", 8080),
        listener("tcp", "TCP", 9000),
        http("http", 80),
        http("other", 80),
        http("admin", 15100),
    ];
    let got = accept(&g);
    assert!(got.is_accepted());
    assert_eq!(got.ports(), [80, 8080]);
    g.has_tls = true;
    assert_eq!(accept(&g).ports(), Vec::<u16>::new());
}
