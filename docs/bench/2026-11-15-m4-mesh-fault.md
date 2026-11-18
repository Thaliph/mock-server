# Fault decision (M4, #452)

Criterion micro-benchmark `mesh_route` of `mimic-core`, group `mesh_fault` ([SPEC FLT-10](../SPEC.md#1228-m4--fault-injection-452)):
one `mesh::fault::decide` over 16 faults of 16 header items each (and a method and a path
prefix) where none holds, and over `faults: []`. Budget: at most 2 µs and at most 50 ns.

```
cargo bench -p mimic-core --bench mesh_route -- mesh_fault --warm-up-time 1 --measurement-time 2
```

| Date | Machine | Load during the run | Result |
|---|---|---|---|
| 2026-11-15 | same machine as the M1 run, rustc 1.99.1, release profile | not controlled (other builds of other agents may run) | see below |

| Case | time (95 % interval) |
|---|---|
| 16 faults x 16 header items, the request has none of the headers (the first item of each fault fails) | 345 to 355 ns |
| `faults: []` | 8.7 to 8.8 ns |
| 16 faults x 16 header items, 15 items of each fault hold and the last fails (worst case, no budget) | 14.4 to 14.6 µs |

The budget case is the one of a request that carries none of the headers the faults ask for.
The worst case costs a scan of the request's fields per item (15 x 15 name comparisons per
fault); a repeated header name is looked for only when an item does not hold. Real policies hold
a few items, so that case is a ceiling to know, not a budget.
