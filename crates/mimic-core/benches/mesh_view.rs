//! MSH-14: the cost of building the view of one deployment from a cluster of 1000 Services
//! with 10 000 endpoints in all (`cargo bench -p mimic-core --bench mesh_view`).

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use mimic_core::Mode;
use mimic_core::mesh::input::*;
use mimic_core::mesh::view::{Limits, build};

fn cluster(services: usize, endpoints_each: usize) -> Snapshot {
    let mut s = Snapshot {
        cluster: "local".into(),
        grants: Some(vec![]),
        ..Snapshot::default()
    };
    for i in 0..services {
        let name = format!("svc-{i:04}");
        s.services.push(Service {
            namespace: "shop".into(),
            name: name.clone(),
            cluster_ips: vec![format!("10.96.{}.{}", i / 250, i % 250 + 1)],
            ports: vec![ServicePort {
                port: 80,
                name: Some("http".into()),
                target_port: TargetPort::Name("http".into()),
            }],
        });
        s.slices.push(EndpointSlice {
            namespace: "shop".into(),
            name: format!("{name}-x7k2p"),
            service: name,
            generation: 1,
            ports: vec![SlicePort {
                name: Some("http".into()),
                port: 8080,
            }],
            endpoints: (0..endpoints_each)
                .map(|e| Endpoint {
                    ip: format!("10.244.{}.{}", i % 250, e + 1),
                    ready: true,
                    serving: true,
                    terminating: false,
                    zone: Some(["a", "b", "c"][e % 3].into()),
                })
                .collect(),
        });
    }
    s.configs.push(Config {
        namespace: "shop".into(),
        name: "slow".into(),
        created: 1,
        generation: 1,
        targets: vec![Target {
            kind: TargetKind::Service,
            name: "svc-0000".into(),
        }],
        destinations: None,
        mesh_enabled: None,
        slow_start: Some(10),
    });
    s
}

fn bench(c: &mut Criterion) {
    let snapshot = cluster(1000, 10);
    let limits = Limits::default();
    let built = build(&snapshot, "shop", "app", Mode::None, &limits);
    assert_eq!(
        (built.view.services.len(), built.view.endpoint_count()),
        (1000, 10_000)
    );
    c.bench_function("view_1000_services_10000_endpoints", |b| {
        b.iter(|| build(black_box(&snapshot), "shop", "app", Mode::None, &limits))
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
