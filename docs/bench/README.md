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

## Mesh setups (#444): how to run

Design §14 (M1) and [SPEC §12.19](../SPEC.md#1219-benchmarks-436), *Mesh setups*, BEN-11 to BEN-16.
Run by hand, one run at a time, on a quiet machine; the rate caps and gentle defaults are the same.

```sh
SETUPS="mesh-lb mesh-churn" scripts/bench.sh                 # both mesh setups, 3 backend pods
SETUPS=mesh-lb PROPAGATION_SAMPLES=0 scripts/bench.sh        # skip the propagation measure
SETUPS="mesh-lb" RATES="200" PROTOCOLS="http1" scripts/bench.sh   # one rate, to look at a change
scripts/bench.sh --dry-run                                   # the plan, nothing started
```

- `mesh-lb`: the backend (`fortio-server`) has 3 pods; the client sidecar's routing to each pod is
  asserted from its metrics before and after the runs (`mimic_mesh_endpoint_requests_total`, and zero
  `mimic_mesh_fallbacks_total`). Then: propagation of an EndpointSlice change to the client's
  `mesh view applied` (N = `PROPAGATION_SAMPLES`, default 20, budget p99 ≤ 2 s), and the client sidecar's
  RSS with a synthetic view of 100 Services × 10 endpoints, against its baseline in the same run
  (budget baseline + 16 MiB). The synthetic objects are deleted at the end.
- `mesh-churn`: the same 3-pod backend, with `kubectl rollout restart` of the backend just before each
  fortio run, so that the rollout runs through the load. Errors and latency are the client's; fallbacks
  and reconnects are recorded, not asserted.
- Both sidecars run in mode `discovery`, as the B0 setup. The M1 latency budget (mesh-routed call adds
  ≤ B0 + 0.5 ms p50, ≤ B0 + 2 ms p99) compares the `mesh-lb` rows with the `discovery` rows of the same
  protocol and rate; the budgets on propagation and RSS are read from the `within_budget` fields of
  `<date>-mesh-lb.json`. The script records them and does not fail on them.
- The JSON files: `mesh` (see BEN-16) and one `rollout_restart` flag per run. Output files of today's
  date must not exist yet (BEN-02).

## Mesh refuse (#445, RTY-23): how to run

Setup `mesh-refuse` measures the retries of M2 (SPEC §7.9 "Retries") against a backend with one pod that
refuses connections. The backend Service has 3 pods: 2 injected (`fortio-server`, discovery mode) and 1
`fortio-dead`, Ready, not injected (`mimic.platform.io/inject: disabled`), listening on no port, with no
probe, sleeping in the pinned Python image of the e2e stages. The setup runs last, after `mesh-churn`.

```sh
SETUPS=mesh-refuse scripts/bench.sh                          # the refusing setup alone
SETUPS="mesh-lb mesh-refuse" scripts/bench.sh                # with the M1 setup, same session (the 0.2 ms check)
scripts/bench.sh --dry-run                                   # the plan, nothing started
```

- Routing, before and after the runs (BEN-12): each of the 2 injected pods has endpoint requests, no
  fallback to the ClusterIP (strict), and the client's `mimic_mesh_attempts_total{result="connect_failed"}`
  is above 0. A refused try is not written to a pod, so the refusing pod is shown by its connect failures,
  not by an endpoint request.
- The JSON: `routing.before` and `routing.after` carry `retries` (`mimic_mesh_retries_total`) and
  `connect_failed_attempts`; the `refusing` block carries `injected_pods` 2, `not_injected_pods` 1,
  `errors_total` (responses that are not 200, over all runs), `retries_total` and
  `connect_failed_attempts` of the client's sidecar. The counters are cumulative since the client's sidecar
  started (the restart of the setup), so `after` includes the `before` phase.
- The file is `<date>-mesh-refuse.json`; its table rows go to `README.md` as for the other setups. The
  committed M1 files are dated 2026-11-14: run on a later day, or set `BENCH_OUT_DIR` to another directory
  (BEN-02 refuses an existing file).

RTY-23 compares the runs with the committed M1 run of the same protocol and rate (table *M1: mesh setups*):

- With no failure, `mesh-lb` is within 0.2 ms at p50 of that M1 row (run `SETUPS="mesh-lb mesh-refuse"` in
  one session).
- `mesh-refuse`: the error rate is 0 % (`errors` 0 in every run), and its p99 is at most the M1 `mesh-lb`
  p99 + 5 ms. For http1 50 rps the M1 section takes the three repeats (9.95, 11.26, 10.39 ms) and not the
  first run (19.35 ms, called noise there); the script does not decide, the reader reads the JSON.

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

## M1: mesh setups (#444)

`SETUPS="mesh-lb mesh-churn" scripts/bench.sh` on main after #516 (b9def19), same machine and
conditions as above. `mesh-lb`: discovery mode, the backend has 3 pods; the routing was asserted from the
client sidecar's metrics before the runs (59, 66, 27 requests per pod) and after them (7169, 7167, 7008),
0 fallbacks. `mesh-churn`: the same with a `kubectl rollout restart` of the backend before each run
(errors are the M2 baseline). The JSON files are `2026-11-14-mesh-lb.json` and `2026-11-14-mesh-churn.json`.

| Date | Setup | Protocol | Rate (rps) | Measured (rps) | p50 (ms) | p90 (ms) | p99 (ms) | p99.9 (ms) | Errors | Caller CPU (cores) | Callee CPU (cores) | Caller RSS (MiB) | Callee RSS (MiB) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-11-14 | mesh-lb | http1 | 50 | 49.98 | 8.3 | 9.36 | 19.35 | 36.47 | 0 | 0.048 | 0.018 | 14.9 | 13.8 |
| 2026-11-14 | mesh-lb | http1 | 100 | 99.97 | 8.32 | 9.06 | 10.88 | 15.83 | 0 | 0.098 | 0.033 | 14.9 | 13.8 |
| 2026-11-14 | mesh-lb | http1 | 200 | 199.93 | 8.19 | 8.94 | 10.63 | 15.25 | 0 | 0.195 | 0.067 | 14.9 | 13.8 |
| 2026-11-14 | mesh-lb | h2c | 50 | 49.98 | 9.63 | 10.78 | 13.47 | 15.26 | 0 | 0.062 | 0.023 | 15.7 | 14.2 |
| 2026-11-14 | mesh-lb | h2c | 100 | 99.96 | 9.65 | 10.8 | 13.15 | 22.25 | 0 | 0.122 | 0.045 | 15.7 | 14.2 |
| 2026-11-14 | mesh-lb | h2c | 200 | 199.92 | 9.69 | 10.87 | 13.61 | 23 | 0 | 0.245 | 0.092 | 16.1 | 14.2 |
| 2026-11-14 | mesh-churn | http1 | 50 | 49.98 | 8.43 | 9.83 | 15.15 | 18.18 | 0 | 0.053 | - | 15.3 | - |
| 2026-11-14 | mesh-churn | http1 | 100 | 99.97 | 8.47 | 9.77 | 12.93 | 27.5 | 5 | 0.104 | - | 15.8 | - |
| 2026-11-14 | mesh-churn | http1 | 200 | 199.94 | 8.34 | 9.64 | 13.63 | 22.5 | 5 | 0.2 | - | 16 | - |
| 2026-11-14 | mesh-churn | h2c | 50 | 49.98 | 9.92 | 11.79 | 16.9 | 21.6 | 3 | 0.066 | - | 16.2 | - |
| 2026-11-14 | mesh-churn | h2c | 100 | 99.96 | 9.95 | 11.81 | 17.5 | 23.22 | 2 | 0.13 | - | 16.3 | - |
| 2026-11-14 | mesh-churn | h2c | 200 | 199.93 | 9.85 | 11.73 | 16.8 | 36.25 | 12 | 0.255 | - | 16.5 | - |
| 2026-11-14 | mesh-lb (repeat 1) | http1 | 50 | 49.98 | 8.31 | 8.99 | 9.95 | 10.41 | 0 | 0.05 | 0.019 | 15.1 | 13.5 |
| 2026-11-14 | mesh-lb (repeat 2) | http1 | 50 | 49.98 | 8.32 | 9 | 11.26 | 13.12 | 0 | 0.05 | 0.016 | 15 | 13.7 |
| 2026-11-14 | mesh-lb (repeat 3) | http1 | 50 | 49.98 | 8.34 | 9.05 | 10.39 | 15.67 | 0 | 0.051 | 0.019 | 14.7 | 13.4 |

The first `mesh-lb` http1 50 rps run had a p99 of 19.35 ms (p99.9 36.47) — one 30 s run, so the p99 is
about 15 requests. That point was run three more times alone (results kept out of the JSON): p99 9.95,
11.26 and 10.39 ms, so the first value was noise.

Against the pre-mesh `discovery` rows of 2026-11-12 (the three `discovery` runs of the sections "B0 after #480", "M0: mode none" and "M0 after #490", all on main after #480 and before the mesh; the first run of "Results" predates TCP_NODELAY (#481) and is left out; the ranges below span those runs), design §14 M1. The committed first `mesh-lb` http1 50 rps p99 (19.35 ms) alone is above the budget; the table uses its three repeats.

| Budget | Measured | |
|---|---|---|
| Mesh-routed p50 ≤ B0 + 0.5 ms | http1 8.19–8.32 ms vs 7.79–8.27 (per rate +0.13 to +0.25 against the mean of the three runs, at most +0.40 against the fastest); h2c 9.63–9.69 vs 9.62–9.91 (≈ 0) | met |
| Mesh-routed p99 ≤ B0 + 2 ms | http1 9.95–11.26 (50 rps, repeats), 10.88, 10.63 vs 9.98–13.73 (at most +0.65 against the fastest run at the same rate); h2c 13.15–13.61 vs 13.47–15.16 | met |
| EndpointSlice change → sidecar applied ≤ 2 s p99 | 20 samples (3 → 4 → 3 endpoints): 448–791 ms, p50 ≈ 0.5 s | met |
| Sidecar RSS ≤ B0 + 16 MiB with a 100-Service / 1000-endpoint view | 16.2 → 16.6 MiB (+0.4 MiB), the view asserted loaded | met |
| P2C pick ≤ 2 µs (criterion, `cargo bench -p mimic-core --bench mesh_lb`) | 21–41 ns (3 and 100 endpoints, steady and slow start) | met |

`mesh-churn` errors per 30 s run during a rolling restart: http1 0, 5, 5; h2c 3, 2, 12 (out of 1 500 to
6 000 requests). Controller RSS with 1000 Services / 10 000 endpoints and the 200-sidecar rollout are
measured under #441.

## Controller (M1, #441)

`scripts/bench-controller.sh` measures the controller of the mesh (design §14, M1): the RSS of the
controller replicas with 1000 Services and 10 000 endpoints (synthetic objects, the view cache included;
budget 256 MiB), and a rollout of a 50-pod deployment in a namespace of 200 sidecars (views per second per
sidecar, reported for the sidecars present during the whole rollout, `kept`, for the ones started during it,
`new`, and over both; the budget of 1 applies to `kept`, since a new pod's first pulls over a few seconds of
life are not a sustained rate; controller CPU summed over replicas, budget 0.5 core). Results go to
`docs/bench/<date>-controller.json`, with the machine description and the budgets.

```bash
scripts/bench-controller.sh --dry-run   # the plan, nothing started
scripts/bench-controller.sh             # both scenarios on cluster "mimic-ctl-bench", deleted on exit (about 13 minutes on this machine)
SCENARIOS=scale scripts/bench-controller.sh   # the RSS scenario only
```

- The kind node runs with kubelet `maxPods` 300 (kind's default is 110), set by the script's kind config
  only. An existing cluster of that name must allow 300 pods, or the run stops (exit 1).
- The sidecars request 5m CPU and 16Mi memory in this run only (the scheduler reserves them for 200 sidecars
  on one node); the workloads are fortio servers with 1m CPU and 16Mi requests.
- Exit codes as `scripts/bench.sh`: 0 done and every budget met; 1 a step failed or a budget is not met (the
  JSON is written in both cases, and the `FAIL budget` line names it); 2 usage or configuration; 3 another run
  holds a lock (`mimic-e2e`, `mimic-bench` or `mimic-controller-bench`).
- `KEEP_CLUSTER=0` (the default) deletes the cluster on exit. With `KEEP_CLUSTER=1` the namespaces and the
  release are deleted and the cluster stays. The result file is never overwritten.
- Run it on a quiet machine, not in parallel with `scripts/bench.sh` or `scripts/e2e.sh`.
- The 200-sidecar rollout drives this machine's load average to 30–50 for a few minutes; run it rarely.

### Results 2026-11-14 (design §14 M1)

`scripts/bench-controller.sh` on `mesh/441-measure` (main after #517 plus the view pacing, MSH-88); JSON
`2026-11-14-controller.json`. Two controller replicas.

| Budget | Measured | |
|---|---|---|
| Controller RSS ≤ 256 MiB with 1000 Services / 10 000 endpoints, view cache included | max 26.5 MiB per replica (view cache 0.5 MiB); 10 clients each holding a view of 1000+ endpoints | met |
| Rollout of a 50-pod deployment among 200 sidecars: ≤ 1 view/s per sidecar | sidecars present for the whole rollout (150): max 0.818, p99 0.812 views/s; new pods (50): max 0.977 (81 s rollout) | met |
| Controller ≤ 0.5 core average during the rollout | 0.173 cores, both replicas summed | met |

A first run without the pacing (same day, result not kept in the JSON) measured views at max 1.54/s and
mean 1.22/s per sidecar, CPU 0.19 cores and RSS 26.3 MiB: the debounce (100 ms doubling to 1 s) let rollout
bursts through faster than 1/s. MSH-88 spaces each pod's views at least 1 s apart.

## M2: retries (#445)

Same machine and conditions. M2 code: `mesh/445-bench-docs` (main after #523, retries on). M1 code: main
after #519 (e7e3168), before any M2 change.

### No failure: M1 and M2 back to back (RTY-23, first half)

A first M2 session (`2026-11-14-m2-mesh-lb.json`) measured p50 0.55–0.85 ms above the committed M1 run of the
morning. That comparison spans hours of machine drift, so `mesh-lb` was then run four times in a row,
alternating M1 and M2 code (`2026-11-14-ab-{m1a,m2a,m1b,m2b}-mesh-lb.json`). p50 in ms, mean of the two runs of
each:

| Protocol | Rate | M1 | M2 | Δ |
|---|---|---|---|---|
| http1 | 50 | 8.59 | 8.70 | +0.11 |
| http1 | 100 | 8.61 | 8.72 | +0.12 |
| http1 | 200 | 8.48 | 8.59 | +0.11 |
| h2c | 50 | 10.19 | 10.32 | +0.13 |
| h2c | 100 | 10.32 | 10.40 | +0.08 |
| h2c | 200 | 10.33 | 10.35 | +0.03 |

Every rate is within the 0.2 ms budget against M1 measured in the same session (met). M1's own p50 moved by
0.3–0.6 ms between the morning and this session, which explains the first comparison. Routing was asserted in
every run (3 pods served, 0 fallbacks, 0 retries).

### One of three pods refusing (`mesh-refuse`, RTY-23, second half)

2 injected backend pods and one Ready pod that refuses connections; same session as the first M2 `mesh-lb` run.
Routing after the runs: 10585 and 10759 requests on the 2 live pods, 85 connect-failed retries, 0 fallbacks.
The rows are that first M2 session: `mesh-lb`, then `mesh-refuse`.

| Date | Setup | Protocol | Rate (rps) | Measured (rps) | p50 (ms) | p90 (ms) | p99 (ms) | p99.9 (ms) | Errors | Caller CPU (cores) | Callee CPU (cores) | Caller RSS (MiB) | Callee RSS (MiB) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-11-14 | mesh-lb | http1 | 50 | 49.98 | 8.95 | 10.35 | 13.7 | 20.23 | 0 | 0.056 | 0.019 | 15.2 | 14.1 |
| 2026-11-14 | mesh-lb | http1 | 100 | 99.96 | 8.92 | 10.25 | 13.83 | 24.44 | 0 | 0.109 | 0.036 | 15.2 | 14.1 |
| 2026-11-14 | mesh-lb | http1 | 200 | 199.93 | 8.74 | 10.09 | 13.26 | 22.5 | 0 | 0.213 | 0.07 | 15.3 | 14.1 |
| 2026-11-14 | mesh-lb | h2c | 50 | 49.94 | 10.35 | 11.93 | 19.35 | 29.17 | 0 | 0.069 | 0.025 | 15.9 | 14.4 |
| 2026-11-14 | mesh-lb | h2c | 100 | 99.96 | 10.32 | 11.97 | 15.15 | 18 | 0 | 0.134 | 0.048 | 15.9 | 14.5 |
| 2026-11-14 | mesh-lb | h2c | 200 | 199.92 | 10.54 | 12.66 | 17.19 | 29.44 | 0 | 0.274 | 0.1 | 15.9 | 14.5 |
| 2026-11-14 | mesh-refuse | http1 | 50 | 49.98 | 8.61 | 9.82 | 12.9 | 15.89 | 0 | 0.053 | 0.024 | 15.1 | 14.1 |
| 2026-11-14 | mesh-refuse | http1 | 100 | 99.96 | 8.67 | 9.9 | 12.87 | 17 | 0 | 0.105 | 0.051 | 15.2 | 14.1 |
| 2026-11-14 | mesh-refuse | http1 | 200 | 199.93 | 8.56 | 9.79 | 12.09 | 23.89 | 0 | 0.209 | 0.099 | 15.3 | 14.1 |
| 2026-11-14 | mesh-refuse | h2c | 50 | 49.98 | 10.2 | 11.57 | 15.56 | 30.82 | 0 | 0.067 | 0.032 | 15.4 | 14.8 |
| 2026-11-14 | mesh-refuse | h2c | 100 | 99.96 | 10.04 | 11.51 | 13.81 | 25 | 0 | 0.132 | 0.063 | 15.4 | 14.8 |
| 2026-11-14 | mesh-refuse | h2c | 200 | 199.92 | 10.02 | 11.52 | 15.13 | 23.33 | 0 | 0.262 | 0.125 | 15.7 | 14.8 |

| Budget (design §14 M2) | Measured | |
|---|---|---|
| No failure: p50 within 0.2 ms of M1 | +0.03 to +0.13 ms, back to back | met |
| One of three pods refusing: client error rate 0 % | 0 errors in every run | met |
| One of three pods refusing: p99 ≤ M1 `mesh-lb` p99 + 5 ms | http1 12.09–12.90 vs M1 10.63–11.26 + 5; h2c 13.81–15.56 vs 13.15–13.61 + 5 | met |

## M3: request mirror (#450)

### `mesh-mirror` (RTE-94)

Backend of 3 pods, mode discovery, 200 rps, 8 connections, 30 s; JSON `2026-11-15-mesh-mirror.json`. Each rate and
protocol runs three times: with no HTTPRoute (no route), with the HTTPRoute and no mirror filter (route, no mirror:
the baseline of RTE-94), then with the filter mirroring every request to a second fortio Service (response delay
5 ms, as the primary's). The first bench of this setup compared the mirror with no route at all and counted 8 more
copies than fortio's requests: fortio's warm-up requests (one per connection) are not in its count but are routed and
mirrored; the copies are now compared with the sidecar's own count of requests routed (`copies_match`).

| Date | Setup | Protocol | Rate (rps) | Measured (rps) | p50 (ms) | p90 (ms) | p99 (ms) | p99.9 (ms) | Errors | Caller CPU (cores) | Callee CPU (cores) | Caller RSS (MiB) | Callee RSS (MiB) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-11-15 | mesh-mirror (no route) | http1 | 200 | 199.94 | 8.53 | 9.61 | 10.95 | 14.5 | 0 | 0.211 | 0.069 | 15.9 | 14.1 |
| 2026-11-15 | mesh-mirror (route, no mirror) | http1 | 200 | 199.93 | 8.57 | 9.65 | 10.95 | 14.98 | 0 | 0.216 | 0.071 | 16.3 | 14.1 |
| 2026-11-15 | mesh-mirror | http1 | 200 | 199.92 | 9.53 | 10.84 | 13.58 | 21.13 | 0 | 0.283 | 0.073 | 16.7 | 14.3 |
| 2026-11-15 | mesh-mirror (no route) | h2c | 200 | 199.91 | 10.11 | 11.39 | 13.54 | 16 | 0 | 0.271 | 0.097 | 17.2 | 14.7 |
| 2026-11-15 | mesh-mirror (route, no mirror) | h2c | 200 | 199.92 | 10.15 | 11.55 | 13.96 | 19.82 | 0 | 0.275 | 0.098 | 17.2 | 14.8 |
| 2026-11-15 | mesh-mirror | h2c | 200 | 199.91 | 11.51 | 13.89 | 27.05 | 57.14 | 0 | 0.339 | 0.102 | 17.3 | 14.9 |

| p50 (ms) | no route | route, no mirror | route with mirror | routing | mirror | copies = primary requests |
|---|---|---|---|---|---|---|
| http1 | 8.53 | 8.57 | 9.53 | +0.04 | +0.96 | 6008 = 6008 |
| h2c | 10.11 | 10.15 | 11.51 | +0.04 | +1.37 | 6008 = 6008 |

Routing alone costs 0.04 ms (budget 0.2 met). The mirror costs 0.96 ms (http1) and 1.37 ms (h2c): the budget
(+0.2 ms) is **not met**. The cost is not in the primary's path: in process (4 workers, a 5 ms primary) the
decision, the head clone and the spawn cost about 0.03 ms, and a mirror that answers 20 ms later than the primary
costs the primary nothing (-0.04 to +0.1 ms, the noise of the probe). What costs is a copy whose response arrives
at the same instant as the primary's (same delay): the work of the copy (reading its answer, the pool, the
counters) queues with the primary's response on the sidecar's workers (+0.3 ms in process, +1 ms in the cluster).
With a mirror Service answering after 25 ms (`MIRROR_DELAY_MS=25`, same session) the p50 delta is +0.23 (http1)
and +0.24 ms (h2c) over the route without mirror; the sidecar uses 0.07 core more at 200 rps.

#### After the decision of #450: a copy starts at the primary's response head

Same setup and session conditions (load below 3, `MIRROR_DELAY_MS` 5, the symmetric worst case), the copy starting
once the primary's response head arrived; JSON `2026-11-15-mesh-mirror.json` (this run replaces the first one,
whose table is above).

| Date | Setup | Protocol | Rate (rps) | Measured (rps) | p50 (ms) | p90 (ms) | p99 (ms) | p99.9 (ms) | Errors | Caller CPU (cores) | Callee CPU (cores) | Caller RSS (MiB) | Callee RSS (MiB) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-11-15 | mesh-mirror (no route) | http1 | 200 | 199.92 | 8.42 | 9.28 | 10.43 | 16.45 | 0 | 0.201 | 0.067 | 15.9 | 14.3 |
| 2026-11-15 | mesh-mirror (route, no mirror) | http1 | 200 | 199.93 | 8.55 | 9.61 | 10.8 | 13.33 | 0 | 0.215 | 0.07 | 16.1 | 14.7 |
| 2026-11-15 | mesh-mirror | http1 | 200 | 199.94 | 8.81 | 9.9 | 10.99 | 15.71 | 0 | 0.292 | 0.07 | 16.6 | 14.7 |
| 2026-11-15 | mesh-mirror (no route) | h2c | 200 | 199.91 | 10.02 | 11.17 | 13.98 | 22.5 | 0 | 0.27 | 0.096 | 17.3 | 15.3 |
| 2026-11-15 | mesh-mirror (route, no mirror) | h2c | 200 | 199.92 | 9.98 | 11.27 | 13.48 | 17.33 | 0 | 0.268 | 0.095 | 17.4 | 15.3 |
| 2026-11-15 | mesh-mirror | h2c | 200 | 199.92 | 10.57 | 12.12 | 15.8 | 31.43 | 0 | 0.367 | 0.099 | 17.6 | 15.3 |

| p50 (ms) | no route | route, no mirror | route with mirror | routing | mirror | copies = primary requests |
|---|---|---|---|---|---|---|
| http1 | 8.42 | 8.55 | 8.81 | +0.13 | +0.26 | 6008 = 6008 |
| h2c | 10.02 | 9.98 | 10.57 | -0.04 | +0.59 | 6008 = 6008 |

The mirror's cost over the route without mirror falls from +0.96 / +1.37 ms to +0.26 / +0.59 ms (http1 / h2c), still
above the 0.2 ms budget of RTE-94 (http1 by 0.06 ms, h2c by 0.39 ms). The copy's response no longer lands with the
primary's, what remains is the copy's own work on the same workers right after the head (the sidecar uses 0.07 to
0.09 core more) and the run-to-run noise of the probe (routing alone moves between -0.04 and +0.13 ms).

## M4: outlier detection and circuit breaker (#451, CBR-34)

Backend of 3 pods, mode discovery, 200 rps, 30 s; JSON `2026-11-15-mesh-lb.json`, `2026-11-15-mesh-eject.json`
(8 connections, http1 and h2c), `2026-11-15-mesh-breaker.json` (32 connections, http1 and h2c, a later run of the same session). Load average below 4 at
the start. The first run of this section (8 connections for both setups) is replaced: its `mesh-eject` budget
(5 requests per ejection) could not hold with concurrent load, and its `mesh-breaker` never exceeded `maxRequests`
(4 breaker-open lines). CBR-34 now bounds `mesh-eject` by `consecutiveFailures + connections - 1` per ejection and
runs `mesh-breaker` with 32 connections and at least 10 % breaker-open answers. fortio runs `mesh-breaker` with
`-allow-initial-errors`, as its warm-up requests (one per connection) are already refused.

| Date | Setup | Protocol | Rate (rps) | Measured (rps) | p50 (ms) | p90 (ms) | p99 (ms) | p99.9 (ms) | Errors | Caller CPU (cores) | Callee CPU (cores) | Caller RSS (MiB) | Callee RSS (MiB) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-11-15 | mesh-lb | http1 | 200 | 199.93 | 8.53 | 9.57 | 10.95 | 15.83 | 0 | 0.214 | 0.076 | 15.9 | 14.3 |
| 2026-11-15 | mesh-lb | h2c | 200 | 199.92 | 10.03 | 11.36 | 13.98 | 18 | 0 | 0.272 | 0.098 | 17 | 14.8 |
| 2026-11-15 | mesh-eject | http1 | 200 | 199.93 | 8.46 | 9.47 | 11.62 | 14.8 | 0 | 0.21 | 0.088 | 16 | 14.1 |
| 2026-11-15 | mesh-eject | h2c | 200 | 199.92 | 9.78 | 10.93 | 13.29 | 16.46 | 0 | 0.262 | 0.123 | 17 | 14.7 |
| 2026-11-15 | mesh-breaker | http1 | 200 | 199.64 | 5.71 | 56.01 | 59.61 | 59.97 | 4488 | 0.17 | 0.021 | 18.5 | 14.2 |
| 2026-11-15 | mesh-breaker | h2c | 200 | 199.58 | 3.59 | 56.15 | 59.84 | 66.13 | 4488 | 0.137 | 0.027 | 18.9 | 14.4 |

| Check | Result | Budget | Verdict |
|---|---|---|---|
| `mesh-eject`: error rate | 0 % (0 of 12000) | 0 % | met |
| `mesh-eject`: requests to the resetting pod | 19 for 3 ejections (6.33 each) | at most 5 + 8 - 1 = 12 each | met |
| `mesh-breaker`: breaker-open answers | 9024 of 11968 requests (75.4 %) | at least 10 % | met |
| `mesh-breaker`: p99 `duration_ms` of the breaker-open lines | 0 ms (8935 at 0 ms, 70 at 1, 14 at 2, 5 at 3; largest 3) | at most 1 ms | met |
| `mesh-lb` p50 against M3 | see below | within 0.2 ms | met against the same-day M3 runs |

`mesh-breaker` errors are the 503 answers of the breaker (per protocol 1496 answers 200 and 4488 answers 503; the access
lines count 4512, the difference being the warm-up requests, which fortio does not count). Its p50 is that of the
accepted requests and the refused ones together. CBR-34 first bounded the largest `duration_ms` of a breaker-open line
by 1 ms; the first run had one line of 4 ms, this one 19 of 9024 lines (0.21 %) above 1 ms, the largest 3 ms. A single
line can wait on a worker that 32 connections keep busy, and `duration_ms` is whole milliseconds, so CBR-34 now bounds
the p99 and records the counts per millisecond and the largest (decision on #451).

`mesh-lb` p50 (no failures), 200 rps: 8.53 ms (http1) and 10.03 ms (h2c). The M3 runs of the same day, which route
the same way with no mirror: 8.42 to 8.55 ms and 10.02 to 10.15 ms (the "no route" and "route, no mirror" rows
above), so +0.0 to +0.11 ms and -0.12 to +0.01 ms: within 0.2 ms. Against the runs of the day before the spread of
the machine is larger than the budget (M1 `mesh-lb` 8.19 / 9.69 ms: +0.34 / +0.34 ms; M2 8.74 / 10.54 ms: -0.21 /
-0.51 ms), so the comparison is meaningful only within a session.

## M5: admission contract (#453, ADM-32)

Setup `mesh-admission` measures the admission contract of the callee sidecars (SPEC §4.1 "Admission contract")
against design §14. It has the topology of `mesh-breaker` (backend of 3 injected pods, mode discovery, `delay=50ms`
asked in the URL, 32 connections) without its `MimicConfig`, and switches one `MimicConfig fortio-admission` on the
backend Deployment. It runs last. It is not in the default matrix.

```sh
SETUPS=mesh-admission RATES="50 100 200" scripts/bench.sh   # 13 runs per protocol
SETUPS=mesh-admission scripts/bench.sh --dry-run            # the plan, nothing started
```

- For each protocol and rate, four runs in ABBA order: without any admission contract (`adm-off`), under
  `admission: {concurrency: 64, rps: 1000}` per pod (`adm-under`, which never binds: 200 rps over 3 pods is about
  67 per pod), under it again, then without it again. A drift over the four runs cancels in the pair, whose sides
  are each the mean of their two runs.
- These runs log every request's latency (fortio `-access-log-file /dev/stderr`), and the pairs use the exact
  p50, p90 and p99 of those latencies. Fortio's own percentiles cannot tell 0.5 ms here: its histogram has 10 ms
  buckets around the 50 ms backend (60 to 70 ms, then 70 ms to the largest sample), and it interpolates a
  percentile inside its bucket, so a few requests on either side of 70 ms, or the largest one, move its p99 by
  milliseconds. In the first run of this setup (2026-11-19, `adm-off` then `adm-under`), the p99 differed by +0.1
  to +4.4 ms while the p50 differed by -0.3 to +0.4 ms; at http1 50 rps the p99 delta (+2.76 ms) was that of the
  two largest samples (+2.90 ms) times 0.95.
- Then one run per protocol under `admission: {concurrency: 64, rps: 30}` per pod (`adm-over`), at twice the rps
  of the 3 pods: 180 rps, with 64 connections so that the refusals (up to 150 ms in the queue) do not slow the load,
  fortio's `-allow-initial-errors`, and `-uniform`. Without `-uniform` the 64 connections send together, in bursts
  3 times a second, and each pod's bucket (`rps / 10` = 3 tokens) is full between them: the tokens it cannot hold
  are lost, and the first run (2026-11-19, #553) admitted 22.2 to 23.0 rps per pod instead of 30.
- Each change of the contract waits, up to 60 s, until the config is `Accepted` and every backend sidecar
  pulled a view after it (`mimic_mesh_view_pulls_total{result="ok"}`), then 2 s.
- For each `adm-over` run, the script records per backend pod the deltas of its inbound requests
  (`mimic_http_requests_total{direction="inbound"}`, all and `2xx`) and the `duration_ms` of its sidecar's
  refusal lines, grouped by `reason`, read with `kubectl logs --since-time` at the end of the run (kubelet
  rotates a container log at about 10 MiB).

The verdicts are in `mesh.admission` of `<date>-mesh-admission.json`. A false one adds the setup to the failed
setups (exit 1 once the file is written):

| Check | Budget |
|---|---|
| Per protocol, the median over the rates of the added p99 under the contract (`pair_medians[]`, from `pairs[].delta_p99_ms`), and no error under it | at most +0.5 ms |
| Load sent in an `adm-over` run (`over[].actual_qps`) | at least 95 % of 180 rps |
| Admitted rate per pod (`2xx` delta over fortio's `ActualDuration`) | within 5 % of 30 rps per pod |
| p99 of the `duration_ms` of the `overloaded` lines | at most 1 ms |
| Longest `duration_ms` of a `queue-timeout` line (counts per millisecond in `queue_timeout_ms_counts`) | at most 160 ms |

A p99 delta of 0.5 ms on a 50 ms backend is close to the noise of the machine: compare only the runs of one session.
The callers' retries and fortio's warm-up add offers, and the tokens still cap the admitted rate (the bias is at most
the burst, 3, per pod and run). The results of the first run go here with the report on #453.
