//! EXP-20: the layout is a pure function of the data, not of its order (spec §7.5, §12.17).

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use super::tests::{call, config, grant, service, snapshot, source};
use super::*;

const NS: [&str; 2] = ["a", "b"];
const NAMES: [&str; 4] = ["w", "x", "y", "z"];
const AUTHORITIES: [&str; 9] = [
    "w.a.svc:80",
    "x.a.svc.cluster.local:80",
    "y.b.svc:80",
    "z.b:80",
    "10.0.0.1:80",
    "10.0.0.2:80",
    "api.example.com:443",
    "<redacted>:443",
    "x",
];

/// Deterministic shuffle: sorts by a mixed key, so every seed is one permutation.
fn shuffled<T: Clone>(items: &[T], seed: u64) -> Vec<T> {
    let mix = |i: usize| {
        let mut x = seed ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        x ^= x >> 31;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^ (x >> 29)
    };
    let mut keyed: Vec<(u64, usize)> = (0..items.len()).map(|i| (mix(i), i)).collect();
    keyed.sort();
    keyed.into_iter().map(|(_, i)| items[i].clone()).collect()
}

fn calls() -> impl Strategy<Value = Vec<Call>> {
    prop::collection::vec(
        (0..AUTHORITIES.len(), 1u64..50, 0u64..3)
            .prop_map(|(a, n, e)| call(AUTHORITIES[a], n, e.min(n))),
        0..6,
    )
}

fn sources() -> impl Strategy<Value = Vec<Source>> {
    prop::collection::vec(
        (0..NS.len(), 0..NAMES.len(), calls())
            .prop_map(|(n, d, calls)| source(NS[n], NAMES[d], calls)),
        0..9,
    )
}

fn snapshot_of() -> Snapshot {
    snapshot(
        vec![service("a", "w", "10.0.0.1"), service("b", "y", "10.0.0.2")],
        vec![config("a", "c", 1, &[("b", "y")])],
        Some(vec![grant("b", "a", None)]),
    )
}

fn is_acyclic(map: &Map) -> bool {
    let mut indegree: BTreeMap<&str, usize> =
        map.nodes.iter().map(|n| (n.id.as_str(), 0)).collect();
    for e in &map.edges {
        *indegree.get_mut(e.to.as_str()).unwrap() += 1;
    }
    let mut ready: Vec<&str> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(n, _)| *n)
        .collect();
    let mut done = 0;
    while let Some(n) = ready.pop() {
        done += 1;
        for e in map.edges.iter().filter(|e| e.from == n) {
            let d = indegree.get_mut(e.to.as_str()).unwrap();
            *d -= 1;
            if *d == 0 {
                ready.push(e.to.as_str());
            }
        }
    }
    done == map.nodes.len()
}

proptest! {
    #[test]
    fn exp_20_any_order_gives_the_same_bytes(sources in sources(), seed in any::<u64>()) {
        let mesh = snapshot_of();
        let mut other = shuffled(&sources, seed);
        for s in &mut other {
            s.calls = shuffled(&s.calls, seed ^ 7);
        }
        let mut mesh2 = mesh.clone();
        mesh2.services = shuffled(&mesh.services, seed);
        let one = build(&sources, Some(&mesh), &Limits::default());
        let two = build(&other, Some(&mesh2), &Limits::default());
        prop_assert_eq!(one.to_json(), two.to_json());
        prop_assert_eq!(one.to_svg(), two.to_svg());
    }

    #[test]
    fn exp_20_layers_and_boxes(sources in sources()) {
        let map = build(&sources, Some(&snapshot_of()), &Limits::default());
        let layer: BTreeMap<&str, usize> = map.nodes.iter().map(|n| (n.id.as_str(), n.layer)).collect();
        if is_acyclic(&map) {
            for e in &map.edges {
                prop_assert!(layer[e.from.as_str()] < layer[e.to.as_str()], "{} -> {}", e.from, e.to);
            }
        }
        let mut seen = BTreeSet::new();
        for a in &map.nodes {
            prop_assert!(seen.insert((a.x, a.y)));
            for b in &map.nodes {
                if a.id != b.id && a.x == b.x {
                    prop_assert!((a.y - b.y).abs() >= NODE_H, "{} and {} overlap", a.id, b.id);
                }
            }
            prop_assert!(a.x >= 0 && a.y >= 0 && a.x + NODE_W <= map.width && a.y + NODE_H <= map.height);
        }
        let svg = map.to_svg();
        prop_assert!(super::tests::well_formed(&svg).is_ok());
    }
}

#[test]
fn exp_20_a_cycle_a_self_call_and_an_isolated_node_are_laid_out() {
    let sources = [
        source(
            "a",
            "x",
            vec![call("y.a.svc:80", 1, 0), call("x.a.svc:80", 1, 0)],
        ),
        source("a", "y", vec![call("z.a.svc:80", 1, 0)]),
        source("a", "z", vec![call("x.a.svc:80", 1, 0)]),
        source("a", "w", vec![]),
    ];
    let map = build(&sources, None, &Limits::default());
    assert_eq!(map.nodes.len(), 4);
    assert_eq!(map.edges.len(), 3, "the self call is not drawn");
    let first = map.to_svg();
    assert!(super::tests::well_formed(&first).is_ok());
    // The cycle is broken at the lowest id: x is in layer 0, y in 1, z in 2.
    let layers: Vec<(&str, usize)> = map
        .nodes
        .iter()
        .map(|n| (n.name.as_str(), n.layer))
        .collect();
    assert_eq!(layers, [("w", 0), ("x", 0), ("y", 1), ("z", 2)]);
}

#[test]
fn exp_20_an_empty_input_is_an_empty_graph() {
    let map = build(&[], None, &Limits::default());
    assert!(map.nodes.is_empty() && map.edges.is_empty());
    assert!(map.width > 0 && map.height > 0);
    let json: serde_json::Value = serde_json::from_str(&map.to_json()).unwrap();
    assert_eq!(json["nodes"], serde_json::json!([]));
    assert_eq!(json["dropped"], serde_json::json!({"nodes": 0, "edges": 0}));
}
