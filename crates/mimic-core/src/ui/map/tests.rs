//! EXP-18, EXP-19, EXP-21 and EXP-22: the graph, the resolution of authorities, the flag "not
//! declared", the escaping of the SVG and its bounds (spec §7.5, §12.17).

use std::time::Instant;

use super::*;
use crate::mesh::input::{
    Config, Destination, GrantFrom, GrantTo, MIMIC_GROUP, ReferenceGrant, Service, ServicePort,
    Snapshot, Target, TargetKind, TargetPort,
};

pub(super) fn call(authority: &str, exchanges: u64, errors: u64) -> Call {
    Call {
        authority: authority.to_string(),
        exchanges,
        errors,
    }
}

pub(super) fn source(ns: &str, deployment: &str, calls: Vec<Call>) -> Source {
    Source {
        namespace: ns.to_string(),
        deployment: deployment.to_string(),
        calls,
    }
}

pub(super) fn service(ns: &str, name: &str, ip: &str) -> Service {
    Service {
        namespace: ns.to_string(),
        name: name.to_string(),
        cluster_ips: vec![ip.to_string()],
        ports: vec![ServicePort {
            port: 80,
            name: None,
            target_port: TargetPort::Number(8080),
        }],
    }
}

pub(super) fn config(ns: &str, name: &str, created: i64, dests: &[(&str, &str)]) -> Config {
    Config {
        namespace: ns.to_string(),
        name: name.to_string(),
        created,
        uid: format!("uid-{name}"),
        generation: 1,
        targets: vec![Target {
            kind: TargetKind::Deployment,
            name: "front".to_string(),
        }],
        destinations: Some(
            dests
                .iter()
                .map(|(namespace, name)| Destination {
                    namespace: (*namespace).to_string(),
                    name: (*name).to_string(),
                })
                .collect(),
        ),
        mesh_enabled: None,
        slow_start: None,
    }
}

pub(super) fn grant(ns: &str, from: &str, name: Option<&str>) -> ReferenceGrant {
    ReferenceGrant {
        namespace: ns.to_string(),
        from: vec![GrantFrom {
            group: MIMIC_GROUP.to_string(),
            kind: "MimicConfig".to_string(),
            namespace: from.to_string(),
        }],
        to: vec![GrantTo {
            group: String::new(),
            kind: "Service".to_string(),
            name: name.map(str::to_string),
        }],
    }
}

pub(super) fn snapshot(
    services: Vec<Service>,
    configs: Vec<Config>,
    grants: Option<Vec<ReferenceGrant>>,
) -> Snapshot {
    Snapshot {
        services,
        configs,
        grants,
        ..Snapshot::default()
    }
}

fn edge<'a>(map: &'a Map, from: &str, to: &str) -> Option<&'a Edge> {
    map.edges.iter().find(|e| e.from == from && e.to == to)
}

fn ids(map: &Map) -> Vec<&str> {
    map.nodes.iter().map(|n| n.id.as_str()).collect()
}

#[test]
fn exp_18_the_authorities_of_one_service_sum_into_one_edge() {
    let sources = [
        source(
            "shop",
            "front",
            vec![
                call("users.shop.svc.cluster.local:8080", 10, 1),
                call("users.shop.svc:8080", 5, 2),
                call("10.96.0.7:8080", 3, 0),
                call("USERS.shop.svc.:8080", 2, 1),
            ],
        ),
        source("shop", "users", vec![]),
    ];
    let mesh = snapshot(vec![service("shop", "users", "10.96.0.7")], vec![], None);
    let map = build(&sources, Some(&mesh), &Limits::default());
    assert_eq!(ids(&map), ["dep:shop/front", "dep:shop/users"]);
    assert_eq!(map.edges.len(), 1, "{:?}", map.edges);
    let e = edge(&map, "dep:shop/front", "dep:shop/users").expect("the edge");
    assert_eq!((e.exchanges, e.errors, e.error_permille), (20, 4, 200));
    assert!(e.declared);
    assert_eq!(map.dropped, Dropped::default());
}

#[test]
fn exp_18_the_error_share_is_rounded_down() {
    let sources = [
        source("a", "f", vec![call("u.a.svc:80", 3, 1)]),
        source("a", "u", vec![]),
    ];
    let map = build(&sources, None, &Limits::default());
    assert_eq!(map.edges[0].error_permille, 333);
}

#[test]
fn exp_18_an_ipv6_cluster_ip_resolves_with_and_without_brackets() {
    let sources = [
        source(
            "shop",
            "front",
            vec![call("[fd00::7]:80", 4, 0), call("fd00::7", 6, 0)],
        ),
        source("shop", "users", vec![]),
    ];
    let mesh = snapshot(vec![service("shop", "users", "fd00::7")], vec![], None);
    let map = build(&sources, Some(&mesh), &Limits::default());
    let e = edge(&map, "dep:shop/front", "dep:shop/users").expect("the edge");
    assert_eq!(e.exchanges, 10);
    // Without the Service an IP address is external.
    let map = build(&sources, None, &Limits::default());
    assert!(edge(&map, "dep:shop/front", "ext:[fd00::7]:80").is_some());
}

#[test]
fn exp_18_short_names_resolve_only_when_known() {
    let sources = [
        source(
            "shop",
            "front",
            vec![
                call("users:8080", 1, 0),
                call("cart.shop:8080", 2, 0),
                call("nobody:8080", 4, 0),
                call("ghost.shop:8080", 8, 0),
                call("stranger.other:8080", 16, 0),
            ],
        ),
        source("shop", "users", vec![]),
    ];
    let mesh = snapshot(vec![service("shop", "cart", "10.0.0.2")], vec![], None);
    let map = build(&sources, Some(&mesh), &Limits::default());
    assert!(edge(&map, "dep:shop/front", "dep:shop/users").is_some());
    assert!(edge(&map, "dep:shop/front", "svc:shop/cart").is_some());
    for external in ["nobody:8080", "ghost.shop:8080", "stranger.other:8080"] {
        assert!(
            edge(&map, "dep:shop/front", &format!("ext:{external}")).is_some(),
            "{external}: {:?}",
            ids(&map)
        );
    }
    // A fully qualified name is a Service even when nothing watches it.
    let sources = [source("shop", "front", vec![call("pay.risk.svc:80", 1, 0)])];
    let map = build(&sources, None, &Limits::default());
    assert!(edge(&map, "dep:shop/front", "svc:risk/pay").is_some());
}

#[test]
fn exp_18_everything_else_is_an_external_leaf() {
    let long = format!("{}:80", "a".repeat(300));
    let calls = [
        "<redacted>:443",
        "api.example.com:443",
        "Bad_Label.shop.svc:80",
        "-x.shop.svc:80",
        "x..svc:80",
        long.as_str(),
    ];
    let sources = [source(
        "shop",
        "front",
        calls.iter().map(|a| call(a, 1, 0)).collect(),
    )];
    let map = build(&sources, None, &Limits::default());
    for n in map.nodes.iter().skip(1) {
        assert_eq!(n.kind, Kind::External, "{}", n.id);
        assert!(n.href.is_none());
    }
    assert!(ids(&map).contains(&"ext:<redacted>:443"));
    assert!(ids(&map).contains(&"ext:api.example.com:443"));
    assert!(ids(&map).contains(&"ext:Bad_Label.shop.svc:80"));
    let cut = map
        .nodes
        .iter()
        .find(|n| n.id.starts_with("ext:aaaa"))
        .expect("the long authority");
    assert_eq!(cut.id.chars().count(), 4 + 255);
    assert_eq!(cut.name.chars().count(), 255);
}

#[test]
fn exp_18_a_service_without_deployment_is_a_service_node() {
    let sources = [source(
        "shop",
        "front",
        vec![call("cart.shop.svc:80", 7, 0)],
    )];
    let map = build(&sources, None, &Limits::default());
    let node = map.nodes.iter().find(|n| n.id == "svc:shop/cart").unwrap();
    assert_eq!(node.kind, Kind::Service);
    assert_eq!(
        (node.namespace.as_str(), node.name.as_str()),
        ("shop", "cart")
    );
    assert!(node.href.is_none());
}

#[test]
fn exp_18_a_call_to_itself_draws_no_edge() {
    let sources = [source(
        "shop",
        "front",
        vec![call("front.shop.svc:80", 7, 0)],
    )];
    let map = build(&sources, None, &Limits::default());
    assert_eq!(ids(&map), ["dep:shop/front"]);
    assert!(map.edges.is_empty());
    assert_eq!(map.dropped, Dropped::default());
}

#[test]
fn exp_18_duplicated_and_reordered_inputs_give_the_same_graph() {
    let a = source(
        "shop",
        "front",
        vec![call("u.shop.svc:80", 2, 1), call("x.com:1", 1, 0)],
    );
    let b = source("shop", "u", vec![]);
    let one = build(&[a.clone(), b.clone()], None, &Limits::default());
    let two = build(&[b.clone(), a.clone()], None, &Limits::default());
    assert_eq!(one.to_json(), two.to_json());
    assert_eq!(one.to_svg(), two.to_svg());
    // The same deployment twice adds up.
    let twice = build(&[a.clone(), a, b], None, &Limits::default());
    assert_eq!(twice.edges[0].exchanges, 4);
    assert!(!one.to_json().is_empty());
}

fn declared(mesh: Option<Snapshot>, authority: &str) -> bool {
    let sources = [source("shop", "front", vec![call(authority, 1, 0)])];
    let map = build(&sources, mesh.as_ref(), &Limits::default());
    assert_eq!(map.edges.len(), 1, "{authority}: {:?}", map.edges);
    map.edges[0].declared
}

#[test]
fn exp_19_a_cross_namespace_service_is_flagged_unless_declared_and_consented() {
    let scoring = "scoring.risk.svc:80";
    let listing = || config("shop", "c", 1, &[("risk", "scoring")]);
    let svcs = || vec![service("risk", "scoring", "10.0.0.9")];
    // Nothing declares it.
    assert!(!declared(
        Some(snapshot(svcs(), vec![], Some(vec![]))),
        scoring
    ));
    // Listed, but no consent: no ReferenceGrant, an empty list, or one for another namespace.
    assert!(!declared(
        Some(snapshot(svcs(), vec![listing()], None)),
        scoring
    ));
    assert!(!declared(
        Some(snapshot(svcs(), vec![listing()], Some(vec![]))),
        scoring
    ));
    let other = Some(vec![grant("risk", "bank", None)]);
    assert!(!declared(
        Some(snapshot(svcs(), vec![listing()], other)),
        scoring
    ));
    // A grant for another Service name.
    let named = Some(vec![grant("risk", "shop", Some("other"))]);
    assert!(!declared(
        Some(snapshot(svcs(), vec![listing()], named)),
        scoring
    ));
    // Listed and consented, with the name or without.
    for name in [None, Some(""), Some("scoring")] {
        let grants = Some(vec![grant("risk", "shop", name)]);
        assert!(
            declared(Some(snapshot(svcs(), vec![listing()], grants)), scoring),
            "grant name {name:?}"
        );
    }
}

#[test]
fn exp_19_only_the_winning_config_of_the_caller_declares() {
    let older = config("shop", "a-older", 1, &[]);
    let newer = config("shop", "b-newer", 2, &[("risk", "scoring")]);
    let grants = Some(vec![grant("risk", "shop", None)]);
    let mesh = snapshot(vec![], vec![older, newer], grants.clone());
    assert!(!declared(Some(mesh), "scoring.risk.svc:80"));
    let winner = config("shop", "a-older", 1, &[("risk", "scoring")]);
    let loser = config("shop", "b-newer", 2, &[]);
    let mesh = snapshot(vec![], vec![loser, winner], grants);
    assert!(declared(Some(mesh), "scoring.risk.svc:80"));
}

#[test]
fn exp_19_same_namespace_and_external_edges_are_never_flagged() {
    let mesh = || Some(snapshot(vec![], vec![], Some(vec![])));
    assert!(declared(mesh(), "cart.shop.svc:80"));
    assert!(declared(mesh(), "api.example.com:443"));
    assert!(declared(mesh(), "<redacted>:443"));
}

#[test]
fn exp_19_without_a_snapshot_nothing_is_flagged() {
    assert!(declared(None, "scoring.risk.svc:80"));
}

// --- EXP-21: the SVG ---------------------------------------------------------------------

/// The elements and attributes the SVG may hold (spec §7.5, EXP-21).
const ELEMENTS: [&str; 9] = [
    "svg", "defs", "marker", "path", "g", "a", "rect", "text", "title",
];

/// A small well-formedness check: tags nest, attribute values are quoted and hold no `<`, text
/// holds no `<` and only the five predefined entities.
pub(super) fn well_formed(svg: &str) -> Result<Vec<String>, String> {
    let mut stack: Vec<String> = Vec::new();
    let mut seen = Vec::new();
    let mut rest = svg;
    let mut first = true;
    while !rest.is_empty() {
        let Some(open) = rest.find('<') else {
            return Err("text outside the root".to_string());
        };
        let (text, after) = rest.split_at(open);
        if first && !text.is_empty() {
            return Err("text before the root".to_string());
        }
        first = false;
        entities(text)?;
        // The end of the tag: a `>` outside quotes.
        let mut quote = false;
        let mut end = None;
        for (i, c) in after.char_indices().skip(1) {
            match c {
                '"' => quote = !quote,
                '<' if !quote => return Err(format!("a `<` inside a tag: {after:.80}")),
                '>' if !quote => {
                    end = Some(i);
                    break;
                }
                _ => {}
            }
        }
        let end = end.ok_or("an unclosed tag")?;
        let tag = &after[1..end];
        rest = &after[end + 1..];
        if let Some(name) = tag.strip_prefix('/') {
            if stack.pop().as_deref() != Some(name) {
                return Err(format!("a closing tag out of order: {name}"));
            }
            continue;
        }
        let self_closing = tag.ends_with('/');
        let tag = tag.trim_end_matches('/');
        let (name, attrs) = tag.split_once(' ').unwrap_or((tag, ""));
        if !ELEMENTS.contains(&name) {
            return Err(format!("an element outside the allowlist: {name}"));
        }
        seen.push(name.to_string());
        attributes(attrs)?;
        if !self_closing {
            stack.push(name.to_string());
        }
    }
    if stack.is_empty() {
        Ok(seen)
    } else {
        Err(format!("unclosed: {stack:?}"))
    }
}

fn entities(text: &str) -> Result<(), String> {
    for (i, _) in text.match_indices('&') {
        let tail = &text[i..];
        if !["&amp;", "&lt;", "&gt;", "&quot;", "&apos;"]
            .iter()
            .any(|e| tail.starts_with(e))
        {
            return Err(format!("a bare `&`: {tail:.30}"));
        }
    }
    if text.contains('>') {
        return Err("a bare `>` in text".to_string());
    }
    Ok(())
}

fn attributes(mut attrs: &str) -> Result<(), String> {
    attrs = attrs.trim();
    while !attrs.is_empty() {
        let (name, tail) = attrs.split_once("=\"").ok_or("an unquoted attribute")?;
        let (value, tail) = tail.split_once('"').ok_or("an unclosed value")?;
        if name.starts_with("on") || name == "style" || name.contains(':') && name != "xmlns" {
            return Err(format!("a forbidden attribute: {name}"));
        }
        entities(value)?;
        if value.contains("http://") && name != "xmlns" {
            return Err(format!("a URL in {name}"));
        }
        attrs = tail.trim_start();
    }
    Ok(())
}

fn hostile_map() -> Map {
    let hostile = [
        "<img src=x onerror=alert(1)>:80",
        "\"><script>alert(1)</script>:80",
        "a&b.example.com:80",
        "it's.example.com:80",
        "ctl\u{1}char.example.com:80",
        "</text><text>:80",
    ];
    let sources = [source(
        "shop",
        "front",
        hostile.iter().map(|a| call(a, 3, 1)).collect(),
    )];
    build(&sources, None, &Limits::default())
}

#[test]
fn exp_21_hostile_authorities_appear_only_escaped() {
    let svg = hostile_map().to_svg();
    well_formed(&svg).expect("well-formed");
    for raw in ["<img", "<script", "</text><text>", "\u{1}"] {
        assert!(!svg.contains(raw), "an unescaped sequence in the SVG");
    }
    assert!(svg.contains("&lt;img src=x onerror=alert(1)&gt;:80"));
    assert!(svg.contains("&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;:80"));
    assert!(svg.contains("a&amp;b.example.com:80"));
    assert!(svg.contains("it&apos;s.example.com:80"));
    assert!(svg.contains('\u{FFFD}'));
    // The JSON keeps the text as it is, as a JSON string.
    let json: serde_json::Value = serde_json::from_str(&hostile_map().to_json()).unwrap();
    let names: Vec<&str> = json["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["name"].as_str())
        .collect();
    assert!(names.contains(&"<img src=x onerror=alert(1)>:80"));
}

#[test]
fn exp_21_a_long_label_is_cut_on_a_character_boundary() {
    for unit in ["a", "é", "日", "🦀"] {
        let host = format!("{}.example.com:80", unit.repeat(100));
        let sources = [source("shop", "front", vec![call(&host, 1, 0)])];
        let svg = build(&sources, None, &Limits::default()).to_svg();
        well_formed(&svg).expect("well-formed");
        let cut: String = host.chars().take(47).collect::<String>() + "…";
        assert!(svg.contains(&format!(">{cut}<")), "{unit}: no cut label");
        assert!(!svg.contains(&host), "{unit}: the whole authority is shown");
    }
}

#[test]
fn exp_21_links_are_the_two_routes_with_encoded_parts() {
    let sources = [
        source("shop", "front", vec![call("users.shop.svc:80", 1, 0)]),
        source("shop", "users", vec![]),
    ];
    let map = build(&sources, None, &Limits::default());
    let svg = map.to_svg();
    well_formed(&svg).expect("well-formed");
    let hrefs: Vec<&str> = svg
        .split("href=\"")
        .skip(1)
        .filter_map(|t| t.split('"').next())
        .collect();
    assert_eq!(hrefs.len(), 3, "{hrefs:?}");
    for href in &hrefs {
        assert!(
            *href == "/ui/workloads/shop/front"
                || *href == "/ui/workloads/shop/users"
                || *href == "/ui/endpoints?deployment=shop%2Ffront",
            "{href}"
        );
    }
    assert_eq!(map.edges[0].href, "/ui/endpoints?deployment=shop%2Ffront");
    let front = map.nodes.iter().find(|n| n.id == "dep:shop/front").unwrap();
    assert_eq!(front.href.as_deref(), Some("/ui/workloads/shop/front"));
}

#[test]
fn exp_21_an_empty_map_is_a_valid_svg() {
    let svg = build(&[], None, &Limits::default()).to_svg();
    assert_eq!(well_formed(&svg).expect("well-formed")[0], "svg");
    assert!(svg.contains("xmlns=\"http://www.w3.org/2000/svg\""));
}

// --- EXP-22: bounds ----------------------------------------------------------------------

#[test]
fn exp_22_deployments_are_capped_by_name_and_their_edges_dropped() {
    let sources: Vec<Source> = (0..1100)
        .map(|i| {
            source(
                "ns",
                &format!("d{i:04}"),
                vec![call(
                    &format!("d{:04}.ns.svc:80", (i + 1) % 1100),
                    1 + i as u64,
                    0,
                )],
            )
        })
        .collect();
    let map = build(&sources, None, &Limits::default());
    assert_eq!(map.nodes.len(), 1000);
    assert!(
        map.nodes.iter().all(|n| n.id.as_str() <= "dep:ns/d0999"),
        "the first 1000 by name"
    );
    // The ring's edges i -> i+1 with i in 0..=998 stay; 999 -> 1000 and the rest lose an end.
    assert_eq!(map.edges.len(), 999);
    assert_eq!(
        map.dropped,
        Dropped {
            nodes: 100,
            edges: 101
        }
    );
    let svg = map.to_svg();
    well_formed(&svg).expect("well-formed");
    assert!(
        svg.contains("100 nodes and 101 edges left out"),
        "the SVG says what was dropped"
    );
}

#[test]
fn exp_22_other_nodes_are_capped_by_weight() {
    let calls = (1..=300u64).map(|w| call(&format!("x{w:03}.example.com:443"), w, 0));
    let sources = [source("ns", "hub", calls.collect())];
    let map = build(&sources, None, &Limits::default());
    assert_eq!(map.nodes.len(), 201);
    assert_eq!(map.edges.len(), 200);
    assert_eq!(
        map.dropped,
        Dropped {
            nodes: 100,
            edges: 100
        }
    );
    // The heaviest 200 stay: x101 to x300.
    assert!(map.edges.iter().all(|e| e.exchanges >= 101));
    assert!(ids(&map).contains(&"ext:x300.example.com:443"));
    assert!(!ids(&map).contains(&"ext:x100.example.com:443"));
}

#[test]
fn exp_22_edges_are_capped_by_weight_then_ids() {
    // 60 deployments calling each other: 3540 edges with distinct weights.
    let sources: Vec<Source> = (0..60u64)
        .map(|i| {
            let calls = (0..60u64)
                .filter(|j| *j != i)
                .map(|j| call(&format!("d{j:02}.ns.svc:80"), 1 + i * 60 + j, 0))
                .collect();
            source("ns", &format!("d{i:02}"), calls)
        })
        .collect();
    let map = build(&sources, None, &Limits::default());
    assert_eq!(map.nodes.len(), 60);
    assert_eq!(map.edges.len(), 2000);
    assert_eq!(
        map.dropped,
        Dropped {
            nodes: 0,
            edges: 1540
        }
    );
    let lightest = map.edges.iter().map(|e| e.exchanges).min().unwrap();
    // 3540 weights, the top 2000 start at the 1541st smallest.
    let mut all: Vec<u64> = (0..60u64)
        .flat_map(|i| {
            (0..60u64)
                .filter(move |j| *j != i)
                .map(move |j| 1 + i * 60 + j)
        })
        .collect();
    all.sort_unstable();
    assert_eq!(lightest, all[1540]);
}

#[test]
fn exp_22_two_hundred_deployments_render_within_200_ms() {
    let sources: Vec<Source> = (0..200)
        .map(|i| {
            let calls = (1..=5)
                .map(|k| {
                    call(
                        &format!("d{:03}.ns.svc:80", (i * 7 + k * 13) % 200),
                        10 + k,
                        k % 2,
                    )
                })
                .chain([call(&format!("ext{}.example.com:443", i % 20), 5, 1)])
                .collect();
            source("ns", &format!("d{i:03}"), calls)
        })
        .collect();
    let started = Instant::now();
    let map = build(&sources, None, &Limits::default());
    let json = map.to_json();
    let svg = map.to_svg();
    let took = started.elapsed();
    assert!(map.nodes.len() >= 200 && !json.is_empty() && !svg.is_empty());
    assert!(took.as_millis() <= 200, "took {took:?}");
}
