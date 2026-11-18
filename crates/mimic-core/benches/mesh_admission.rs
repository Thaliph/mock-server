//! ADM-08: the cost of one admission decision (`cargo bench -p mimic-core --bench
//! mesh_admission`); the budget is 200 ns for an offer admitted at once plus its release, and
//! for an offer refused `overloaded` with a full queue of 10 000.

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use mimic_core::mesh::admission::{Admission, Contract, MAX_CONCURRENCY, MAX_RPS, Offer, Refusal};

const CONTRACT: Contract = Contract {
    concurrency: MAX_CONCURRENCY,
    rps: MAX_RPS,
};

fn admission(c: &mut Criterion) {
    let mut group = c.benchmark_group("mesh_admission");
    let mut out = Vec::new();

    // An uncontended state: an empty queue, a slot and a token free at each offer.
    let mut state = Admission::new();
    state.set_contract(Some(CONTRACT), Duration::ZERO, &mut out);
    let mut now = Duration::ZERO;
    group.bench_function("admit-release", |b| {
        b.iter(|| {
            now += Duration::from_micros(1);
            let Offer::Admitted(permit) = state.offer(black_box(now), &mut out) else {
                unreachable!()
            };
            state.release(permit, black_box(now), &mut out);
        });
    });
    assert!(out.is_empty());

    // The worst case of a refusal: every slot held, a full queue of 10 000 whose head is read.
    let t = Duration::from_secs(5);
    let mut state = Admission::new();
    state.set_contract(Some(CONTRACT), Duration::ZERO, &mut out);
    let mut held = Vec::new();
    for _ in 0..CONTRACT.concurrency {
        let Offer::Admitted(permit) = state.offer(t, &mut out) else {
            unreachable!()
        };
        held.push(permit);
    }
    for _ in 0..CONTRACT.queue_size() {
        let Offer::Queued(_) = state.offer(t, &mut out) else {
            unreachable!()
        };
    }
    let counts = state.counts(t);
    assert_eq!((counts.in_flight, counts.depth), (100_000, 10_000));
    group.bench_function("overloaded-full-queue-10000", |b| {
        b.iter(|| {
            let offer = state.offer(black_box(t), &mut out);
            assert_eq!(offer, Offer::Refused(Refusal::Overloaded));
            black_box(offer)
        });
    });
    assert!(out.is_empty());
    assert_eq!(state.counts(t).depth, 10_000);
    group.finish();
}

criterion_group!(benches, admission);
criterion_main!(benches);
