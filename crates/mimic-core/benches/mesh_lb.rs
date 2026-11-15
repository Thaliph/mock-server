//! MSH-46: the cost of one P2C pick (`cargo bench -p mimic-core --bench mesh_lb`); the budget
//! is 2 µs (design §14).

use std::hint::black_box;
use std::net::SocketAddr;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use mimic_core::mesh::lb::Endpoints;
use mimic_core::mesh::outlier::{Outcome, Outlier};
use mimic_core::mesh::policy::OutlierDetection;

fn endpoints(n: usize, ramping: bool) -> Endpoints {
    let addrs: Vec<SocketAddr> = (0..n)
        .map(|i| SocketAddr::from(([10, 0, (i / 250) as u8, (i % 250) as u8 + 1], 8080)))
        .collect();
    let mut set = Endpoints::new(Duration::from_secs(30));
    if ramping {
        set.update(&addrs[..1], Duration::ZERO);
    }
    set.update(&addrs, Duration::ZERO);
    set
}

fn pick(c: &mut Criterion) {
    let mut group = c.benchmark_group("mesh_lb_pick");
    for n in [3usize, 100, 1000] {
        for ramping in [false, true] {
            let set = endpoints(n, ramping);
            let load: Vec<u32> = (0..n).map(|i| (i % 7) as u32).collect();
            let mut state = 0x2545_F491_4F6C_DD1Du64;
            let name = if ramping { "slow-start" } else { "steady" };
            group.bench_with_input(BenchmarkId::new(name, n), &n, |b, _| {
                b.iter(|| {
                    let mut draw = |m: usize| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        (state % m as u64) as usize
                    };
                    black_box(set.pick(black_box(Duration::from_secs(5)), |i| load[i], &mut draw))
                });
            });
        }
    }
    group.finish();
}

/// CBR-14: a pick over 1000 endpoints with 10 % ejected (the budget is the same 2 µs), and the
/// cost of recording one outcome.
fn eject(c: &mut Criterion) {
    let n = 1000usize;
    let set = endpoints(n, false);
    let addrs: Vec<SocketAddr> = (0..n).map(|i| set.addr(i).expect("an endpoint")).collect();
    let policy = OutlierDetection {
        consecutive_failures: 1,
        consecutive_5xx: 0,
        base_ejection_seconds: 300,
        max_ejection_percent: 100,
    };
    let now = Duration::from_secs(5);
    let mut outlier = Outlier::new(&policy);
    outlier.reconcile(&policy, &addrs, now);
    for addr in addrs.iter().step_by(10) {
        outlier.record(*addr, Outcome::Reset, now);
    }
    assert_eq!(outlier.ejected_count(now), n / 10);
    let load: Vec<u32> = (0..n).map(|i| (i % 7) as u32).collect();
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut group = c.benchmark_group("mesh_outlier");
    group.bench_function("pick-1000-10pct-ejected", |b| {
        b.iter(|| {
            let mut draw = |m: usize| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state % m as u64) as usize
            };
            black_box(set.pick_unejected(
                black_box(now),
                |i| load[i],
                &mut draw,
                |a| outlier.is_ejected(a, now),
            ))
        });
    });
    // A healthy endpoint answering: the counters reset, nothing is ejected.
    let target = addrs[1];
    group.bench_function("record-1000", |b| {
        b.iter(|| black_box(outlier.record(black_box(target), Outcome::Head(200), black_box(now))));
    });
    group.finish();
}

criterion_group!(benches, pick, eject);
criterion_main!(benches);
