# P2C pick with ejected endpoints and outcome recording (M4, #451)

Criterion micro-benchmark `mesh_lb` of `mimic-core`, group `mesh_outlier` ([SPEC CBR-14](../SPEC.md#1227-m4--outlier-detection-and-circuit-breaker-451)):
one `Endpoints::pick_unejected` over 1000 endpoints with 10 % of them ejected (the predicate is
`Outlier::is_ejected`), and one `Outlier::record` of an outcome that changes nothing but the
counters. Budget (design §14, M1): at most 2 µs per pick.

```
cargo bench -p mimic-core --bench mesh_lb -- mesh_outlier
```

| Date | Machine | Load during the run | Result |
|---|---|---|---|
| 2026-11-15 | same machine as the M1 run, rustc 1.99.1, release profile, `--warm-up-time 1 --measurement-time 2` | not controlled (other builds of other agents may run) | see below |

| Case | time (95 % interval) |
|---|---|
| pick, 1000 endpoints, 10 % ejected | 194 to 196 ns |
| record one outcome (1000 endpoints held) | 24.7 to 25.4 ns |

An ejected draw is drawn again (at most 8 times for the pair) instead of scanning the set, so the
pick stays O(1) and about ten times under the budget. The first version, which scanned the set
for every pair that met an ejected endpoint, took 22 µs and was dropped.
