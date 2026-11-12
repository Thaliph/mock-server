//! MSH-09: any order of the same objects gives the same bytes, ETag and statuses.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use super::input::*;
use super::policy::statuses;
use super::view::{Limits, build};
use crate::Mode;

const NS: [&str; 3] = ["a", "b", "c"];
const NAMES: [&str; 3] = ["x", "y", "z"];

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

fn ns() -> impl Strategy<Value = &'static str> {
    prop::sample::select(NS.to_vec())
}

fn name() -> impl Strategy<Value = &'static str> {
    prop::sample::select(NAMES.to_vec())
}

fn endpoint() -> impl Strategy<Value = Endpoint> {
    (
        0u8..6,
        any::<bool>(),
        any::<bool>(),
        prop::option::of(name()),
    )
        .prop_map(|(i, ready, term, zone)| Endpoint {
            ip: format!("10.0.0.{i}"),
            ready,
            serving: ready || term,
            terminating: term,
            zone: zone.map(Into::into),
        })
}

fn slice_of() -> impl Strategy<Value = EndpointSlice> {
    (
        ns(),
        name(),
        0i64..5,
        prop::collection::vec(
            prop::option::of(prop::sample::select(vec!["http", "grpc"])),
            1..3,
        ),
        prop::collection::vec(endpoint(), 0..6),
    )
        .prop_map(|(n, svc, generation, ports, endpoints)| EndpointSlice {
            namespace: n.into(),
            name: format!("{svc}-1"),
            service: svc.into(),
            generation,
            ports: ports
                .into_iter()
                .enumerate()
                .map(|(i, p)| SlicePort {
                    name: p.map(Into::into),
                    port: 8000 + i as u16,
                })
                .collect(),
            endpoints,
        })
}

fn config() -> impl Strategy<Value = Config> {
    (
        ns(),
        name(),
        0i64..3,
        prop::collection::vec((any::<bool>(), name()), 1..4),
        prop::option::of(prop::collection::vec((ns(), name()), 0..4)),
        prop::option::of(any::<bool>()),
        prop::option::of(0u32..4000),
    )
        .prop_map(
            |(n, nm, created, targets, destinations, mesh_enabled, slow_start)| Config {
                namespace: n.into(),
                name: nm.into(),
                created,
                generation: created + 1,
                targets: targets
                    .into_iter()
                    .map(|(d, t)| Target {
                        kind: if d {
                            TargetKind::Deployment
                        } else {
                            TargetKind::Service
                        },
                        name: t.into(),
                    })
                    .collect(),
                destinations: destinations.map(|d| {
                    d.into_iter()
                        .map(|(namespace, name)| Destination {
                            namespace: namespace.into(),
                            name: name.into(),
                        })
                        .collect()
                }),
                mesh_enabled,
                slow_start,
            },
        )
}

fn grant_of() -> impl Strategy<Value = ReferenceGrant> {
    (ns(), ns(), any::<bool>(), prop::option::of(name())).prop_map(|(n, from_ns, good, to_name)| {
        ReferenceGrant {
            namespace: n.into(),
            from: vec![
                GrantFrom {
                    group: MIMIC_GROUP.into(),
                    kind: "MimicConfig".into(),
                    namespace: from_ns.into(),
                },
                GrantFrom {
                    group: "x".into(),
                    kind: "MimicConfig".into(),
                    namespace: "a".into(),
                },
            ],
            to: vec![GrantTo {
                group: String::new(),
                kind: if good { "Service" } else { "Pod" }.into(),
                name: to_name.map(Into::into),
            }],
        }
    })
}

fn snapshot_strategy() -> impl Strategy<Value = Snapshot> {
    (
        prop::collection::btree_map((ns(), name()), prop::collection::vec(0u8..4, 0..3), 0..9),
        prop::collection::vec(slice_of(), 0..8),
        prop::collection::vec(config(), 0..8),
        prop::option::of(prop::collection::vec(grant_of(), 0..5)),
        prop::collection::btree_set((ns(), name()), 0..5),
    )
        .prop_map(|(services, slices, configs, grants, deployments)| {
            let slices: BTreeMap<_, _> = slices
                .into_iter()
                .map(|s| ((s.namespace.clone(), s.name.clone()), s))
                .collect();
            let configs: BTreeMap<_, _> = configs
                .into_iter()
                .map(|c| ((c.namespace.clone(), c.name.clone()), c))
                .collect();
            let deployments: BTreeSet<_> = deployments;
            Snapshot {
                cluster: "local".into(),
                services: services
                    .into_iter()
                    .map(|((n, name), ips)| Service {
                        namespace: n.into(),
                        name: name.into(),
                        cluster_ips: ips.iter().map(|i| format!("10.96.0.{i}")).collect(),
                        ports: vec![
                            ServicePort {
                                port: 80,
                                name: Some("http".into()),
                                target_port: TargetPort::Number(8000),
                            },
                            ServicePort {
                                port: 81,
                                name: None,
                                target_port: TargetPort::Name("grpc".into()),
                            },
                        ],
                    })
                    .collect(),
                slices: slices.into_values().collect(),
                configs: configs.into_values().collect(),
                grants,
                deployments: deployments
                    .into_iter()
                    .map(|(n, d)| (n.into(), d.into()))
                    .collect(),
            }
        })
}

/// The same objects with every list and nested list reordered.
fn reordered(s: &Snapshot, seed: u64) -> Snapshot {
    Snapshot {
        cluster: s.cluster.clone(),
        services: shuffled(
            &s.services
                .iter()
                .map(|v| Service {
                    cluster_ips: shuffled(&v.cluster_ips, seed ^ 1),
                    ports: shuffled(&v.ports, seed ^ 2),
                    ..v.clone()
                })
                .collect::<Vec<_>>(),
            seed ^ 3,
        ),
        slices: shuffled(
            &s.slices
                .iter()
                .map(|v| EndpointSlice {
                    ports: shuffled(&v.ports, seed ^ 4),
                    endpoints: shuffled(&v.endpoints, seed ^ 5),
                    ..v.clone()
                })
                .collect::<Vec<_>>(),
            seed ^ 6,
        ),
        configs: shuffled(
            &s.configs
                .iter()
                .map(|v| Config {
                    targets: shuffled(&v.targets, seed ^ 7),
                    destinations: v.destinations.as_ref().map(|d| shuffled(d, seed ^ 8)),
                    ..v.clone()
                })
                .collect::<Vec<_>>(),
            seed ^ 9,
        ),
        grants: s.grants.as_ref().map(|g| {
            shuffled(
                &g.iter()
                    .map(|v| ReferenceGrant {
                        from: shuffled(&v.from, seed ^ 10),
                        to: shuffled(&v.to, seed ^ 11),
                        ..v.clone()
                    })
                    .collect::<Vec<_>>(),
                seed ^ 12,
            )
        }),
        deployments: shuffled(&s.deployments, seed ^ 13),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn msh_09_any_order_of_the_same_objects_gives_the_same_view_etag_and_statuses(
        s in snapshot_strategy(),
        seed in any::<u64>(),
        max_services in 1usize..6,
        max_endpoints in 1usize..20,
        max_view_bytes in 100usize..1500,
    ) {
        let limits = Limits { max_services, max_endpoints, max_view_bytes };
        let other = reordered(&s, seed);
        prop_assert_eq!(statuses(&s), statuses(&other));
        for n in NS {
            for d in NAMES {
                let a = build(&s, n, d, Mode::Discovery, &limits);
                let b = build(&other, n, d, Mode::Discovery, &limits);
                prop_assert_eq!(&a.json, &b.json);
                prop_assert_eq!(&a.etag, &b.etag);
                prop_assert_eq!(a.truncated, b.truncated);
                prop_assert_eq!(&a.skipped, &b.skipped);
                prop_assert_eq!(a.skipped.len(), a.truncated.min(16));
                prop_assert!(a.json.len() <= max_view_bytes || a.view.services.is_empty());
                prop_assert!(a.view.services.len() <= max_services);
                prop_assert!(a.view.endpoint_count() <= max_endpoints);
            }
        }
    }
}
