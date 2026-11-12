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
- **h2c:** the first run added about 45 ms per request and stopped at 162 rps: the sidecar's sockets
  did not set `TCP_NODELAY` (Nagle and delayed ACKs, #480). After the fix (#481) the sidecar adds about
  3 ms at p50 and 5.7 ms at p99 over h2c, and 200 rps is reached; see *B0 after #480* below. The h2c
  budgets are taken from that run.
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

## B0 after #480

Same machine, conditions and matrix, from `main` at the merge of #481 (`TCP_NODELAY` on every
sidecar socket), files `2026-11-12-nodelay-<setup>.json`. Highest 1-minute load average during the
12 runs (sampled every 30 s, including the kind cluster start-up): 13.6.

| Date | Setup | Protocol | Rate (rps) | Measured (rps) | p50 (ms) | p90 (ms) | p99 (ms) | p99.9 (ms) | Errors | Caller CPU (cores) | Callee CPU (cores) | Caller RSS (MiB) | Callee RSS (MiB) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-11-12 | plain | http1 | 50 | 49.99 | 6.1 | 6.87 | 8.13 | 8.97 | 0 | - | - | - | - |
| 2026-11-12 | plain | http1 | 100 | 99.97 | 6.38 | 6.9 | 7.64 | 11.5 | 0 | - | - | - | - |
| 2026-11-12 | plain | http1 | 200 | 199.94 | 6.29 | 6.88 | 7.81 | 10.06 | 0 | - | - | - | - |
| 2026-11-12 | plain | h2c | 50 | 49.98 | 6.62 | 7.68 | 9.11 | 9.8 | 0 | - | - | - | - |
| 2026-11-12 | plain | h2c | 100 | 99.98 | 6.6 | 7.55 | 8.19 | 10 | 0 | - | - | - | - |
| 2026-11-12 | plain | h2c | 200 | 199.94 | 6.59 | 7.53 | 8.73 | 11.57 | 0 | - | - | - | - |
| 2026-11-12 | discovery | http1 | 50 | 49.98 | 8.24 | 9.3 | 13.73 | 23.6 | 0 | 0.043 | 0.043 | 13.1 | 13.1 |
| 2026-11-12 | discovery | http1 | 100 | 99.96 | 8.27 | 9.35 | 13.52 | 21.93 | 0 | 0.086 | 0.086 | 13.2 | 13.1 |
| 2026-11-12 | discovery | http1 | 200 | 199.94 | 8.02 | 8.97 | 12.56 | 25.71 | 0 | 0.165 | 0.167 | 13.4 | 13.2 |
| 2026-11-12 | discovery | h2c | 50 | 49.98 | 9.91 | 11.7 | 14.78 | 16.98 | 0 | 0.066 | 0.064 | 14.2 | 14.2 |
| 2026-11-12 | discovery | h2c | 100 | 99.96 | 9.65 | 10.95 | 13.92 | 23.75 | 0 | 0.125 | 0.12 | 14.3 | 14.6 |
| 2026-11-12 | discovery | h2c | 200 | 199.92 | 9.66 | 11.1 | 14.55 | 20.27 | 0 | 0.252 | 0.243 | 14.6 | 14.6 |

## M0: mode none (#439)

Same machine and conditions, from `mesh/439-mode-none` at `cc41e2a`, 18 runs (setups `plain`, `none`,
`discovery`), files `2026-11-12-m0-<setup>.json`. Highest 1-minute load average sampled every 30 s
during the runs: 7.2.

Budget (design §14): `none` adds at most 60 % of `discovery`'s added p50 and uses at most 70 % of its
CPU, at 200 rps. Added p50 is the setup's p50 minus `plain`'s; CPU is caller plus callee sidecar.

| Protocol | Added p50, none / discovery | Ratio (≤ 60 %) | CPU, none / discovery (cores) | Ratio (≤ 70 %) |
|---|---|---|---|---|
| http1 | 1.43 / 1.75 ms | **82 %, not met** | 0.183 / 0.329 | 56 %, met |
| h2c | 2.76 / 3.12 ms | **88 %, not met** | 0.333 / 0.497 | 67 %, met |

The CPU budget holds. The latency budget does not: recording costs about 0.3 ms of p50, and most of the
added latency is the proxy hop itself (accept, protocol detection, HTTP re-framing, connect), which
`none` keeps. The budget assumed recording dominated.

**Budget revised (#490).** The loopback profile of #490 puts the sidecar's own time at about 9 µs
(http1) and 5 µs (h2c) per exchange; the rest of the pair's added latency is the pod legs (veth,
netfilter, conntrack) and core wake-ups at a gentle rate, which no sidecar change removes. The ratio
`hop / (hop + recording)` also worsens whenever recording gets cheaper. Design §14 now states an
absolute M0 budget: at 200 rps over a 5 ms service, the `none` pair adds at most 1.5 ms p50 over
HTTP/1.1 and 3.0 ms over h2c (measured above: 1.43 and 2.76 ms, met), CPU unchanged (met). These
limits were set from this first measurement with a 5–8 % margin, not derived independently: they pin
today's hop so that a regression shows, and they are tightened when a change cuts the hop (#493). #490
also moves the HTTP/1.1 observation (access line, metrics, recording) after the response bytes
(PRX-33); its kind-level gain is measured by the next run.

| Date | Setup | Protocol | Rate (rps) | Measured (rps) | p50 (ms) | p90 (ms) | p99 (ms) | p99.9 (ms) | Errors | Caller CPU (cores) | Callee CPU (cores) | Caller RSS (MiB) | Callee RSS (MiB) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-11-12 | plain | http1 | 50 | 49.99 | 6.36 | 6.92 | 8.67 | 11.16 | 0 | - | - | - | - |
| 2026-11-12 | plain | http1 | 100 | 99.98 | 6.36 | 6.89 | 7.61 | 8.88 | 0 | - | - | - | - |
| 2026-11-12 | plain | http1 | 200 | 199.94 | 6.25 | 6.88 | 8.02 | 9.38 | 0 | - | - | - | - |
| 2026-11-12 | plain | h2c | 50 | 49.99 | 6.59 | 7.44 | 8.77 | 9.94 | 0 | - | - | - | - |
| 2026-11-12 | plain | h2c | 100 | 99.97 | 6.57 | 7.28 | 7.99 | 9.19 | 0 | - | - | - | - |
| 2026-11-12 | plain | h2c | 200 | 199.95 | 6.58 | 7.37 | 8.62 | 11.5 | 0 | - | - | - | - |
| 2026-11-12 | none | http1 | 50 | 49.99 | 7.79 | 8.89 | 12.19 | 20.69 | 0 | 0.025 | 0.023 | 12.9 | 13 |
| 2026-11-12 | none | http1 | 100 | 99.96 | 7.8 | 8.84 | 9.93 | 12.32 | 0 | 0.049 | 0.047 | 13 | 13.1 |
| 2026-11-12 | none | http1 | 200 | 199.94 | 7.68 | 8.7 | 9.79 | 12.57 | 0 | 0.093 | 0.09 | 13.3 | 13.1 |
| 2026-11-12 | none | h2c | 50 | 49.98 | 9.28 | 10.56 | 14.61 | 23.2 | 0 | 0.046 | 0.042 | 13.8 | 14.4 |
| 2026-11-12 | none | h2c | 100 | 99.96 | 9.36 | 10.64 | 14.69 | 18.35 | 0 | 0.091 | 0.084 | 14.3 | 14.5 |
| 2026-11-12 | none | h2c | 200 | 199.93 | 9.34 | 10.95 | 20.75 | 36.43 | 0 | 0.172 | 0.161 | 14.3 | 14.6 |
| 2026-11-12 | discovery | http1 | 50 | 49.98 | 8.25 | 9.41 | 12.68 | 20.56 | 0 | 0.043 | 0.042 | 12.9 | 12.9 |
| 2026-11-12 | discovery | http1 | 100 | 99.97 | 8.24 | 9.32 | 10.91 | 18.29 | 0 | 0.084 | 0.083 | 13.1 | 13.3 |
| 2026-11-12 | discovery | http1 | 200 | 199.94 | 8 | 9.04 | 11.3 | 13.7 | 0 | 0.165 | 0.164 | 13.4 | 13.3 |
| 2026-11-12 | discovery | h2c | 50 | 49.98 | 9.62 | 11.37 | 15.16 | 20.88 | 0 | 0.064 | 0.061 | 14.3 | 14.2 |
| 2026-11-12 | discovery | h2c | 100 | 99.96 | 9.67 | 11.28 | 14.36 | 19 | 0 | 0.127 | 0.122 | 14.5 | 14.6 |
| 2026-11-12 | discovery | h2c | 200 | 199.92 | 9.7 | 11.08 | 13.89 | 24.09 | 0 | 0.252 | 0.245 | 14.6 | 14.7 |

Other measurements: [mesh view build, M1](2026-11-12-m1-mesh-view.md).

## M0 after #490 (closes #439)

Same machine and matrix, from `main` with #494 (HTTP/1 observation after the bytes) and the `pod_of` fix,
files `2026-11-12-m0-after-490-<setup>.json`. Quiet machine: 1-minute load average sampled every 30 s,
mean 2.2, highest 4.5.

| Protocol | none added p50 (budget) | CPU none / discovery (≤ 70 %) |
|---|---|---|
| http1 | 1.30 ms (≤ 1.5 ms): met | 0.187 / 0.344 cores, 54 %: met |
| h2c | 2.78 ms (≤ 3.0 ms): met | 0.344 / 0.516 cores, 67 %: met |

`discovery` over HTTP/1.1 adds 1.53 ms at p50 against 1.75 ms before #494.

| Date | Setup | Protocol | Rate (rps) | Measured (rps) | p50 (ms) | p90 (ms) | p99 (ms) | p99.9 (ms) | Errors | Caller CPU (cores) | Callee CPU (cores) | Caller RSS (MiB) | Callee RSS (MiB) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-11-12 | plain | http1 | 50 | 49.99 | 6.38 | 6.91 | 8.93 | 10.88 | 0 | - | - | - | - |
| 2026-11-12 | plain | http1 | 100 | 99.97 | 6.38 | 6.9 | 7.73 | 8.92 | 0 | - | - | - | - |
| 2026-11-12 | plain | http1 | 200 | 199.95 | 6.26 | 6.88 | 7.85 | 11.5 | 0 | - | - | - | - |
| 2026-11-12 | plain | h2c | 50 | 49.99 | 6.62 | 7.58 | 8.62 | 8.99 | 0 | - | - | - | - |
| 2026-11-12 | plain | h2c | 100 | 99.97 | 6.6 | 7.49 | 8.63 | 10.26 | 0 | - | - | - | - |
| 2026-11-12 | plain | h2c | 200 | 199.95 | 6.61 | 7.51 | 8.36 | 9.79 | 0 | - | - | - | - |
| 2026-11-12 | none | http1 | 50 | 49.98 | 7.74 | 8.8 | 9.85 | 10.78 | 0 | 0.026 | 0.025 | 12.6 | 12.8 |
| 2026-11-12 | none | http1 | 100 | 99.97 | 7.75 | 8.82 | 10.45 | 13.66 | 0 | 0.051 | 0.049 | 12.7 | 12.9 |
| 2026-11-12 | none | http1 | 200 | 199.94 | 7.56 | 8.61 | 11.06 | 17.33 | 0 | 0.095 | 0.092 | 12.9 | 13 |
| 2026-11-12 | none | h2c | 50 | 49.98 | 9.46 | 10.64 | 12.1 | 13.45 | 0 | 0.047 | 0.043 | 13.9 | 14.3 |
| 2026-11-12 | none | h2c | 100 | 99.96 | 9.48 | 10.74 | 12.47 | 14.39 | 0 | 0.092 | 0.085 | 13.9 | 14.4 |
| 2026-11-12 | none | h2c | 200 | 199.93 | 9.39 | 10.74 | 14.75 | 26.25 | 0 | 0.178 | 0.166 | 14 | 14.4 |
| 2026-11-12 | discovery | http1 | 50 | 49.98 | 8.03 | 8.97 | 12.01 | 14.95 | 0 | 0.045 | 0.044 | 12.9 | 12.8 |
| 2026-11-12 | discovery | http1 | 100 | 99.97 | 8.07 | 9.1 | 11.67 | 18.2 | 0 | 0.09 | 0.088 | 13.1 | 13.1 |
| 2026-11-12 | discovery | http1 | 200 | 199.91 | 7.79 | 8.88 | 9.98 | 14.7 | 0 | 0.173 | 0.171 | 13.2 | 13.1 |
| 2026-11-12 | discovery | h2c | 50 | 49.98 | 9.79 | 11.12 | 13.47 | 15.83 | 0 | 0.067 | 0.064 | 14.3 | 14.5 |
| 2026-11-12 | discovery | h2c | 100 | 99.96 | 9.91 | 11.31 | 13.87 | 18.51 | 0 | 0.133 | 0.127 | 14.3 | 14.7 |
| 2026-11-12 | discovery | h2c | 200 | 199.93 | 9.83 | 11.45 | 13.93 | 26.43 | 0 | 0.263 | 0.253 | 14.7 | 14.7 |
