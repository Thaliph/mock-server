# Sidecar baseline (B0)

Results of `scripts/bench.sh` (#436, [SPEC §12.16](../SPEC.md#1216-benchmarks-436)): what the sidecar
costs on a quiet machine, before the mesh milestones. Each setup writes `<date>-<setup>.json` (the
machine description, every run, the sidecar CPU and RSS); each run adds one row to the table at the
end of this file. How to run it and read the numbers: [Developing](../wiki/Developing.md#benchmarks).

Status: **not run yet.** The B0 run is the main session's, on a quiet machine (BEN-10).

## Conditions

- Load: fortio `1.76.0` (digest-pinned in `images/Dockerfile`), 30 s per run, 8 connections,
  10 s pause between runs, one run at a time, rates 50, 100 and 200 rps, HTTP/1.1 keep-alive and h2c.
- Service under load: a fortio echo server, answering 1 KiB after a fixed 5 ms delay.
- Setups: `plain` (no sidecar) and `discovery` (the sidecar in discovery mode, caller and callee).
- Sidecar CPU is `process_cpu_seconds_total` over the run, in cores; RSS is
  `process_resident_memory_bytes` after the run, in MiB.
- If 200 rps is not clean on this machine (errors, or a measured rate below the configured one), say so
  here and take the mesh budgets at 100 rps.

## Machine

To fill in from the `machine` block of the JSON files of the run:

| Field | Value |
|---|---|
| CPU model and cores | _to fill_ |
| RAM | _to fill_ |
| Kernel | _to fill_ |
| Docker server | _to fill_ |
| kind | _to fill_ |
| Kubernetes and node image (digest) | _to fill_ |
| Other load during the run | _to fill_ |

## Results

Latencies in milliseconds, errors are the responses that are not 200, sidecar CPU in cores, RSS in MiB.

| Date | Setup | Protocol | Rate (rps) | p50 (ms) | p90 (ms) | p99 (ms) | p99.9 (ms) | Errors | Caller CPU (cores) | Callee CPU (cores) | Caller RSS (MiB) | Callee RSS (MiB) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
