# Admission decision (M5, #453)

Criterion micro-benchmark `mesh_admission` of `mimic-core`, group `mesh_admission`
([SPEC ADM-08](../SPEC.md#1229-m5--admission-contract-in-the-inbound-sidecar-453)): the cost of
one decision of `mesh::admission::Admission` under the largest contract, 100 000 / 1 000 000.
Budget: at most 200 ns for each case.

```
cargo bench -p mimic-core --bench mesh_admission -- --warm-up-time 1 --measurement-time 3
```

- `admit-release`: an uncontended state (an empty queue, a slot and a token free), the clock
  1 µs later at each iteration; one `offer` admitted at once, then its `release`.
- `overloaded-full-queue-10000`: the worst case of a refusal. Every one of the 100 000 slots is
  held and the queue holds its 10 000 entries (asserted before measuring); one `offer` refused
  `overloaded`.

Both cases reuse one `out` buffer, so no call allocates.

## 2026-11-18

| Date | Machine | Load during the run | Result |
|---|---|---|---|
| 2026-11-18 | Intel i7-8750H @ 2.20 GHz, 12 threads, rustc 1.99.1, release profile | not controlled: load average 8.8 to 12.1 (other builds of other agents may run) | see below |

| Case | time (95 % interval) | Budget |
|---|---|---|
| `admit-release` (offer admitted at once plus its release) | 22.6 to 23.3 ns | 200 ns |
| `overloaded-full-queue-10000` (offer refused with a full queue of 10 000) | 18.7 to 19.1 ns | 200 ns |

The offer applies the queue first, but it reads only the head: the deadlines never decrease, so
an unexpired head means no entry behind it has expired, and the head's admission depends on the
slots and tokens alone. A refusal therefore costs the same with 1 or 10 000 entries queued.

A mutant whose apply counted the expired entries over the whole queue (the same decisions, in
O(n)) took 14.9 to 16.6 µs for the refusal with 10 000 queued: the bench tells it apart.
