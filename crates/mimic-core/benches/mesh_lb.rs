//! MSH-46: the cost of one P2C pick (`cargo bench -p mimic-core --bench mesh_lb`); the budget
//! is 2 µs (design §14).

use std::hint::black_box;
use std::net::SocketAddr;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use mimic_core::mesh::lb::Endpoints;

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
                    black_box(set.pick(
                        black_box(Duration::from_secs(5)),
                        |i| load[i],
                        &mut draw,
                    ))
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, pick);
criterion_main!(benches);
