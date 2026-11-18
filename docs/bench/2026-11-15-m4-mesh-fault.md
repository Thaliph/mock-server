# Fault decision (M4, #452)

Criterion micro-benchmark `mesh_route` of `mimic-core`, group `mesh_fault` ([SPEC FLT-10](../SPEC.md#1228-m4--fault-injection-452)):
one `mesh::fault::Faults::decide` over 16 faults of 16 header items each (and a method and a path
prefix) where none holds, and over `faults: []`. Budget: at most 2 µs and at most 50 ns.

```
cargo bench -p mimic-core --bench mesh_route -- mesh_fault --warm-up-time 1 --measurement-time 3
```

"None holding" is measured in its worst case (PM decision of 2026-11-18 on #452): every fault
holds its first 15 items and fails on the last one, so all 256 items are evaluated. The request
has 20 lines: 4 common ones and `X-H0` to `X-H15`, with the names in upper case so that each
name lookup folds the case.

## 2026-11-18 (the budget case is the worst case)

| Date | Machine | Load during the run | Result |
|---|---|---|---|
| 2026-11-18 | Intel i7-8750H @ 2.20 GHz, 12 threads, rustc 1.99.1, release profile | not controlled: load average 2 to 5.7 (other builds of other agents may run) | see below |

| Case | time (95 % interval) | Budget |
|---|---|---|
| 16 faults x 16 header items, 15 items of each fault hold and the last fails (worst case) | 761 to 818 ns | 2 µs |
| `faults: []` | 7.6 to 7.9 ns | 50 ns |
| 16 faults x 16 header items, the request has none of the headers (the first item of each fault fails) | 338 to 441 ns | none |
| 16 faults x 16 header items, no item shared by two faults, so the request carries 244 lines | 7.2 to 7.3 µs | none |

`Faults::new` prepares the faults once, when the view is built: the repeated names of a match
are dropped (only the first item of a name counts), each header name is held once, and each
`(name, value)` item once. A request then looks each of its lines up once in a table of those
names, and compares each item once, however many faults hold it. The worst case costs 20
lookups and 31 item comparisons, not 256 scans of the request's lines.

The last case has no budget: the 240 lines it needs for 15 items of every fault to hold are
each looked up once (about 25 ns a line, with the case folded), and that cost is the request's
size, not the number of faults.

## 2026-11-15 (superseded)

The first run measured "none holding" with the request carrying none of the headers, the cheap
case: 345 to 355 ns, `faults: []` 8.7 to 8.8 ns, and the worst case 14.4 to 14.6 µs, over the
budget. Each item then scanned the request's fields for its name (15 x 15 name comparisons per
fault).
