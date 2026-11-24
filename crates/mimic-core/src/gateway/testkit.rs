//! Builders for the gateway tests. Fixture: Gateway `shop/web` of class `mimic` with one
//! listener `http` HTTP 80; `namespaces = {shop}`, `max_replicas = 10`.

use proptest::prelude::*;

use super::Settings;
use super::input::*;
use crate::mesh::input::GATEWAY_GROUP;

pub fn listener(name: &str, protocol: &str, port: u16) -> Listener {
    Listener {
        name: name.into(),
        hostname: None,
        port,
        protocol: protocol.into(),
        allowed_from: None,
        kinds: None,
        unknown_fields: vec![],
    }
}

pub fn http(name: &str, port: u16) -> Listener {
    listener(name, "HTTP", port)
}

pub fn kind(group: &str, kind: &str) -> RouteKind {
    RouteKind {
        group: group.into(),
        kind: kind.into(),
    }
}

pub fn http_route() -> RouteKind {
    kind(GATEWAY_GROUP, "HTTPRoute")
}

pub fn gw() -> Gateway {
    Gateway {
        namespace: "shop".into(),
        name: "web".into(),
        uid: "uid-web".into(),
        generation: 1,
        class: "mimic".into(),
        listeners: vec![http("http", 80)],
        has_addresses: false,
        has_tls: false,
        allowed_listeners_from: None,
        infrastructure: Infrastructure::default(),
        unknown_fields: vec![],
        oversize: false,
        observed: ObservedStatus::default(),
    }
}

pub fn class() -> GatewayClass {
    GatewayClass {
        name: "mimic".into(),
        generation: 1,
        has_parameters_ref: false,
        observed: vec![],
    }
}

pub fn settings() -> Settings {
    Settings {
        namespaces: ["shop".to_string()].into(),
        max_replicas: 10,
    }
}

fn name_of(max: usize) -> impl Strategy<Value = String> {
    prop_oneof![
        2 => prop::sample::select(vec!["web", "a.b", "1web", "web.prod.example"]).prop_map(String::from),
        1 => prop::string::string_regex(&format!("[a-z0-9][a-z0-9.-]{{0,{}}}", max - 1))
            .expect("name regex"),
    ]
}

fn any_listener(i: usize) -> impl Strategy<Value = Listener> {
    let protocols = vec![
        "HTTP",
        "HTTP",
        "HTTP",
        "HTTPS",
        "TLS",
        "TCP",
        "UDP",
        "http",
        "example.com/proto",
    ];
    let ports = prop_oneof![
        prop::sample::select(vec![0u16, 1, 80, 8080, 15100, 65535]),
        any::<u16>(),
    ];
    let from = prop::option::of(prop::sample::select(vec![
        "Same", "All", "Selector", "None", "x",
    ]));
    let kinds = prop::option::of(prop::collection::vec(
        prop::sample::select(vec![
            http_route(),
            kind(GATEWAY_GROUP, "TCPRoute"),
            kind(GATEWAY_GROUP, "GRPCRoute"),
            kind("", "HTTPRoute"),
        ]),
        0..3,
    ));
    let unknown = prop::collection::vec(prop::sample::select(vec!["filters", "x"]), 0..2);
    (
        prop::sample::select(protocols),
        ports,
        from,
        kinds,
        prop::option::of("[a-z]{1,10}"),
        prop::bool::weighted(0.8),
        unknown,
    )
        .prop_map(
            move |(protocol, port, from, kinds, hostname, clean, unknown)| Listener {
                name: format!("l{i}"),
                hostname,
                port,
                protocol: protocol.into(),
                allowed_from: from.map(Into::into),
                kinds,
                unknown_fields: if clean {
                    vec![]
                } else {
                    unknown
                        .into_iter()
                        .map(|f| format!("spec.listeners[{i}].{f}"))
                        .collect()
                },
            },
        )
}

fn any_listeners() -> impl Strategy<Value = Vec<Listener>> {
    (0usize..=70).prop_flat_map(|n| (0..n).map(any_listener).collect::<Vec<_>>())
}

/// Any converted Gateway, its class and the settings: names up to 90 characters, 0 to 70
/// listeners of every protocol, port, `from` and kinds, every top-level fault (each rare).
pub fn any_gateway() -> impl Strategy<Value = (Gateway, GatewayClass, Settings)> {
    let infrastructure = (
        prop::bool::weighted(0.05),
        prop::collection::vec(
            prop::sample::select(vec!["example.com/x", super::REPLICAS_ANNOTATION]),
            0..2,
        ),
        prop::option::of(prop::sample::select(vec![
            "1", "2", "3", "10", "11", "100", "0", "03", "+3", "two", "",
        ])),
        prop::bool::weighted(0.05),
    )
        .prop_map(|(has_labels, keys, replicas, has_parameters_ref)| {
            let mut annotation_keys: Vec<String> = keys.into_iter().map(Into::into).collect();
            annotation_keys.sort();
            annotation_keys.dedup();
            let replicas = annotation_keys
                .iter()
                .any(|k| k == super::REPLICAS_ANNOTATION)
                .then(|| replicas.map(String::from))
                .flatten();
            Infrastructure {
                has_labels,
                annotation_keys,
                replicas,
                has_parameters_ref,
            }
        });
    let faults = (
        prop::bool::weighted(0.05),
        prop::bool::weighted(0.05),
        prop::option::of(prop::sample::select(vec!["None", "Same", "All"])),
        prop::bool::weighted(0.05),
        prop::bool::weighted(0.05),
    );
    let gateway = (
        prop_oneof![3 => Just("shop"), 1 => Just("other")],
        name_of(90),
        name_of(80),
        any_listeners(),
        infrastructure,
        faults,
    )
        .prop_map(
            |(namespace, name, class, listeners, infrastructure, faults)| {
                let (has_addresses, has_tls, allowed_from, unknown, oversize) = faults;
                Gateway {
                    namespace: namespace.into(),
                    name,
                    uid: "uid".into(),
                    generation: 1,
                    class,
                    listeners: if oversize { vec![] } else { listeners },
                    has_addresses,
                    has_tls,
                    allowed_listeners_from: allowed_from.map(Into::into),
                    infrastructure: if oversize {
                        Infrastructure::default()
                    } else {
                        infrastructure
                    },
                    unknown_fields: if unknown && !oversize {
                        vec!["spec.defaultScope".into()]
                    } else {
                        vec![]
                    },
                    oversize,
                    observed: ObservedStatus::default(),
                }
            },
        );
    (gateway, prop::bool::weighted(0.1), 1u32..=100).prop_map(|(gw, refs, max)| {
        let class = GatewayClass {
            name: gw.class.clone(),
            generation: 1,
            has_parameters_ref: refs,
            observed: vec![],
        };
        let settings = Settings {
            namespaces: ["shop".to_string()].into(),
            max_replicas: max,
        };
        (gw, class, settings)
    })
}
