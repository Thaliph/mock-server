---
name: bench
description: Run, rerun or extend mimic's load benchmarks on a local kind cluster (scripts/bench.sh, fortio) and judge a result against its spec budget (BEN, RTE-94, CBR-34…). Use when a criterion has a latency or throughput budget, when a mesh setup is added, or when a bench number looks wrong.
---

# Benchmarks

`scripts/bench.sh` creates the kind cluster `mimic-bench`, installs mimic and fortio, runs every
`SETUPS × PROTOCOLS × RATES` combination and writes `<date>-<setup>.json` plus rows of
`docs/bench/README.md`. Its header lists every variable; `--dry-run` prints the plan.

## Rules

- The main session runs it, never an agent. One bench **or** one e2e at a time on the machine
  (shared docker cache, CPU noise), only when the load average is below 8 and no agent is
  stress-testing. Gentle by design: `RATES` at most 200, `CONNECTIONS` at most 64.
- It refuses to overwrite results of the day: a rerun to investigate goes to
  `BENCH_OUT_DIR=<scratchpad dir>`; only the run that the PR commits goes to `docs/bench`.
- Check the bench measures what the criterion says before trusting it: fortio warm-up requests
  (an offset of `CONNECTIONS` calls), in-flight requests at the moment of an ejection, and
  `-allow-initial-errors` for setups that start refusing.
- A budget is never loosened to make a run pass. Change it only with data from at least two runs,
  the reason written in the spec next to the criterion, and the decision recorded on the issue.
  A missed budget is a performance bug with its own issue (e.g. #542 for RTE-94).
- Prefer a percentile (p50 for cost, p99 for the tail) over a maximum: one GC pause or scheduling
  hiccup must not decide a verdict.

## Typical runs

```bash
SETUPS="plain none discovery" scripts/bench.sh                  # the M0 baseline
SETUPS=mesh-mirror RATES=200 scripts/bench.sh                   # RTE-94 mirror cost
SETUPS="mesh-eject mesh-breaker" RATES=200 scripts/bench.sh      # CBR-34
BENCH_OUT_DIR=<scratchpad>/bench SETUPS=mesh-lb scripts/bench.sh   # rerun
```

Run it in the background with a `timeout`, chained in one command; never wait on `pgrep -f`.

## Extend

A new setup is a `case` branch in `scripts/bench.sh` plus its line in the header, a verdict
computed with `jq` from the fortio JSON (the budget from the spec, not a literal invented in the
script), and a section in `docs/bench/README.md` that says what it measures and how to read it.
