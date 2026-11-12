# Sidecar baseline (B0)

Results of `scripts/bench.sh` (#436, [SPEC §12.19](../SPEC.md#1219-benchmarks-436)): what the sidecar
costs on a quiet machine, before the mesh milestones. Each setup writes `<date>-<setup>.json` (the
machine description, every run, the sidecar CPU and RSS); each run adds one row to the table at the
end of this file. How to run it and read the numbers: [Developing](../wiki/Developing.md#benchmarks).

Status: **B0 measured on 2026-11-12** (v0.2.0-alpha.3 sources, commit `1438bc6` of #478), on a quiet
machine: no other build, test or cluster load during the runs.

Findings:

- **HTTP/1.1:** the discovery sidecar (caller and callee) adds about 2 ms at p50 and 4 ms at p99 over the
  5 ms service, from 50 to 200 rps; sidecar CPU about 0.04 core per 50 rps, RSS 13 MiB.
- **h2c:** the sidecar adds about 45 ms per request and 200 rps is not reached (162 rps with 8
  connections). This is a bug, not a cost: the sidecar's proxied sockets do not set `TCP_NODELAY`
  (Nagle and delayed ACKs, #480). The h2c budgets are taken after its fix and a re-run.
- 200 rps over HTTP/1.1 is clean on this machine (no errors, measured rate within 0.1 %).

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

From the `machine` block of the JSON files:

| Field | Value |
|---|---|
| CPU model and cores | Intel(R) Core(TM) i7-8750H CPU @ 2.20GHz, 12 threads |
| RAM | 31.0 GiB |
| Kernel | 7.2.15-1-omarchy |
| Docker server | 29.9.2 |
| kind | kind v0.33.0 go1.26.7 linux/amd64 |
| Kubernetes and node image (digest) | v1.33.12, `kindest/node:v1.33.12@sha256:3f5c8443c620245e4d355cfe09e96a91ead32ceaa569d3f1ca9edf0cb2fe2ff4` |
| Other load during the run | none (desktop idle; no build, test or other benchmark) |

## Results

Latencies in milliseconds, errors are the responses that are not 200, sidecar CPU in cores, RSS in MiB.

| Date | Setup | Protocol | Rate (rps) | Measured (rps) | p50 (ms) | p90 (ms) | p99 (ms) | p99.9 (ms) | Errors | Caller CPU (cores) | Callee CPU (cores) | Caller RSS (MiB) | Callee RSS (MiB) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-11-12 | plain | http1 | 50 | 49.99 | 6.33 | 6.88 | 7.37 | 9.28 | 0 | - | - | - | - |
| 2026-11-12 | plain | http1 | 100 | 99.98 | 6.34 | 6.89 | 7.68 | 9.67 | 0 | - | - | - | - |
| 2026-11-12 | plain | http1 | 200 | 199.95 | 6.17 | 6.85 | 7.51 | 10.25 | 0 | - | - | - | - |
| 2026-11-12 | plain | h2c | 50 | 49.99 | 6.57 | 7.33 | 10.29 | 14.9 | 0 | - | - | - | - |
| 2026-11-12 | plain | h2c | 100 | 99.97 | 6.58 | 7.43 | 8.82 | 14 | 0 | - | - | - | - |
| 2026-11-12 | plain | h2c | 200 | 199.94 | 6.55 | 7.22 | 8.74 | 11.86 | 0 | - | - | - | - |
| 2026-11-12 | discovery | http1 | 50 | 49.98 | 8.22 | 9.22 | 11.55 | 15 | 0 | 0.043 | 0.043 | 12.7 | 12.6 |
| 2026-11-12 | discovery | http1 | 100 | 99.96 | 8.23 | 9.27 | 11.81 | 23.33 | 0 | 0.085 | 0.085 | 13.1 | 13 |
| 2026-11-12 | discovery | http1 | 200 | 199.9 | 7.98 | 9 | 11.3 | 17.5 | 0 | 0.167 | 0.164 | 13.1 | 13.2 |
| 2026-11-12 | discovery | h2c | 50 | 49.98 | 10.41 | 51.61 | 53.47 | 53.65 | 0 | 0.068 | 0.063 | 14 | 14.2 |
| 2026-11-12 | discovery | h2c | 100 | 99.82 | 51.75 | 58.39 | 59.89 | 71.25 | 0 | 0.141 | 0.12 | 14.1 | 14.3 |
| 2026-11-12 | discovery | h2c | 200 | 162.22 | 48 | 53.97 | 59.41 | 59.95 | 0 | 0.244 | 0.214 | 14.2 | 14.3 |
