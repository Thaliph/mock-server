# M3 `mesh_route` (#448, RTE-46)

`cargo bench -p mimic-core --bench mesh_route`, 100 rules (10 routes of 10 rules, each match with a
path, a method, 2 header and 1 query items), release profile, quiet machine, 2026-11-14.

| Case | Time (criterion, lower / estimate / upper) | Budget |
|---|---|---|
| `select_last_100_rules` (matches the last rule in precedence) | 717.5 / 738.5 / 764.0 ns | 10 µs |
| `select_none_100_rules` (matches none) | 530.0 / 544.6 / 562.1 ns | 10 µs |
| `build_100_rules` (`Table::new`) | 54.5 / 55.7 / 57.0 µs | none |
