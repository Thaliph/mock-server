# Operations

Names assume the Helm release is called `mimic`; replace `mimic` with your release name otherwise.

Audience: ops. Covers F0 to F5. Reference: spec §7.3, §9, §10, §11.

## Health probes

| Component | Port | Paths |
|---|---|---|
| Controller | 8081 | `/healthz` (process alive), `/readyz` (ready), `/metrics` |
| Sidecar | 15100 | same, plus `/discovery` (loopback only) |

The controller is **ready** only when Valkey answers `PING`, the webhook certificate is loaded, the
`MimicWorkload` cache is synced and the TLS seed is loaded. It becomes unready without exiting when
Valkey is unreachable, and ready again when it returns. The sidecar is ready once its listeners are
bound (app containers start after that); its readiness does not depend on the controller.

## Metrics

OpenMetrics text on `/metrics` of each admin port. Enable the `ServiceMonitor` with
`metrics.serviceMonitor.enabled=true` for the controller; sidecars expose `/metrics` on port 15100; the chart
does not scrape them, so use a `PodMonitor` of your own. Authority labels are capped per direction and metric
(`MIMIC_METRICS_MAX_AUTHORITIES`, overflow is `(other)`); no label carries a namespace, deployment or
pod on the controller.

### Both components

| Metric | Type | Labels |
|---|---|---|
| `mimic_build_info` | gauge (1) | `component`, `version` |
| `mimic_log_lines_dropped_total` | counter | none |

### Sidecar

| Metric | Type | Labels |
|---|---|---|
| `mimic_http_requests_total` | counter | `direction`, `protocol`, `authority`, `status_class` |
| `mimic_http_request_duration_seconds` | histogram (time to response head) | `direction`, `authority`, `status_class` |
| `mimic_proxy_connections_total` | counter | `direction`, `protocol`, `authority` |
| `mimic_proxy_bytes_total` | counter | `direction`, `protocol`, `flow` |
| `mimic_tcp_connections_total` | counter | `direction`, `result` (`ok`, `connect_error`, `loop`, `no_original_dst`) |
| `mimic_accept_errors_total` | counter | `direction` |
| `mimic_tls_connections_total` | counter | `interception` |
| `mimic_tls_passthrough_destinations` | gauge | none |
| `mimic_tls_intermediate_expiry_timestamp_seconds` | gauge | none |
| `mimic_tls_intermediate_requests_total` | counter | `result` (`ok`, `error`, `mismatch`) |
| `mimic_discovery_exchanges_total` | counter | `direction`, `result` (`recorded`, `not_found`, `invalid`, `endpoint_limit`, `dropped`, `panic`) |
| `mimic_discovery_bodies_total` | counter | `direction`, `part`, `result` (`parsed`, `empty`, `opaque`, `too_large`, `encoding`, `invalid`, `budget`) |
| `mimic_discovery_samples_total` | counter | `direction`, `result` (`kept`, `budget`; `replaced`, `rejected` reserved) |
| `mimic_discovery_redactions_total` | counter | `location` (`path`, `query`, `header`, `body`), `rule` |
| `mimic_discovery_endpoints` | gauge | `direction` |
| `mimic_discovery_sample_bytes` | gauge | none |
| `mimic_discovery_pushes_total` | counter | `result` (`ok`, `duplicate`, `too_large`, `rejected`, `error`) |
| `mimic_discovery_last_push_timestamp_seconds` | gauge | none (0 before the first acknowledged push) |

### Controller

| Metric | Type | Labels |
|---|---|---|
| `mimic_sidecar_api_requests_total` | counter | `endpoint`, `result` |
| `mimic_tls_seed_loaded` | gauge | none (1 once loaded) |
| `mimic_store_writes_total` | counter | `result` (`ok`, `conflict`, `error`, `refused`) |
| `mimic_store_write_duration_seconds` | histogram | none |
| `mimic_store_report_bytes` | histogram (4Ki to 64Mi) | none |
| `mimic_store_pushes_total` | counter | `result` (`applied`, `duplicate`) |
| `mimic_store_refused_total` | counter | `kind` (`endpoint`, `sample`), `reason` (`endpoint_limit`, `budget`) |
| `mimic_store_evicted_total` | counter | `kind` |
| `mimic_store_expired_total` | counter | `kind` |
| `mimic_status_updates_total` | counter | `result` (`ok`, `error`) (F5 status) |
| `mimic_ui_requests_total` | counter | `route` (route pattern or `other`), `code` (read API and UI listener) |
| `mimic_ui_render_duration_seconds` | histogram | `route` (parse and render of a stored report for `endpoints`, `openapi` and a document) |
| `mimic_ui_skipped_total` | counter | `reason` (`meta-unreadable`: a list entry left out) |

Webhook metrics (controller):

| Metric | Type | Labels |
|---|---|---|
| `mimic_webhook_admissions_total` | counter | `result` (`injected`, `skipped`, `error`), `reason` (for example `ok`, `not-selected`, `malformed`, `seed-not-loaded`, and the skip reasons of [Concepts](Concepts#injection-which-pods) such as `host-network`) |
| `mimic_webhook_certificate_expiry_timestamp_seconds` | gauge | none (expiry of the webhook serving certificate; the controller renews it 30 days before) |

Starting alerts (tune to taste):

| Alert on | Meaning |
|---|---|
| `mimic_discovery_pushes_total{result="error"}` rising, or `time() - mimic_discovery_last_push_timestamp_seconds` large on a sidecar that has traffic | Reports are not reaching the controller |
| `mimic_store_writes_total{result="error"}` rising | Valkey unavailable or full |
| `mimic_store_refused_total`, `mimic_store_evicted_total` rising | A deployment hit the store limits |
| `mimic_discovery_exchanges_total{result="dropped"}` or `{result="panic"}` | Discovery is losing records |
| `mimic_webhook_certificate_expiry_timestamp_seconds - time()` under a few days | The webhook certificate is not being renewed |
| `mimic_webhook_admissions_total{result="error"}` rising | Admissions failing; the `reason` label says why: `too-large`, `unreadable`, `cache-not-synced`, `patch`, `seed-not-loaded`, `ca-not-loaded`, `root` or `malformed` |
| `mimic_tls_seed_loaded == 0` | Webhook cannot issue injections that need the seed |
| `mimic_log_lines_dropped_total` rising | stdout cannot keep up; log lines are being dropped |
| `mimic_tls_intermediate_expiry_timestamp_seconds - time()` small, non-zero | Intermediate not renewed |

### The UI

The UI is on the read API listener (`MIMIC_UI_ADDR`, loopback of each controller pod): reach it with
`kubectl -n mimic-system port-forward deploy/mimic-controller 8080:8080` and open
`http://localhost:8080/ui/`. Same access rule as the read API: whoever may use `pods/portforward` in
the release namespace sees every namespace's discovered data. A render (the `endpoints` and
`openapi` routes) parses the stored report on a blocking thread, at most 2 at once per replica; the
others wait, and the list, workload and report routes are not held up by them. No log line is written per request.
`mimic_ui_skipped_total{reason="meta-unreadable"}` counts list entries left out because their
`:meta` does not parse (also logged). Rising `mimic_ui_requests_total{code="500"}` or `{code="503"}`
means a stored value that does not parse or Valkey unavailable (see [Troubleshooting](Troubleshooting)).

## Logs

JSON, one object per line on stdout, fields flattened at the top level: `timestamp`, `level`, `target`,
`message`, plus event fields. Level from `MIMIC_LOG` (`controller.logLevel`,
`inject.sidecarLogLevel`). Lines go through a bounded queue (8192) and are dropped, not blocked,
when stdout cannot keep up.

| `target` | Lines |
|---|---|
| `access` | One per HTTP exchange (`message` `http exchange`) and per opaque/TLS connection (`connection`): `direction`, `protocol`, `dst`, `authority`, `method`, `path` (no query), `status`, `result`, `duration_ms`, `head_ms`, `bytes_in`, `bytes_out`, `tls`, `interception` |
| `discovery` | `endpoint discovered`, `template absorbed`, push events |
| `sidecar_api` | One per request: `endpoint`, `namespace`, `pod`, `deployment`, `result` (`ok`, `duplicate`, `unauthenticated`, `forbidden`, `invalid`, `too_large`, `rate_limited`, `push_limit`, `deployment_limit`, `unavailable`) |
| `tls` | `tls passthrough` |

Header values, cookies, query strings and bodies are never logged. Try:

```bash
kubectl -n mimic-system logs deploy/mimic-controller | jq -c 'select(.level != "DEBUG")'
kubectl -n demo logs <pod> -c mimic-sidecar | jq -c 'select(.target=="access")'
```

Key messages:

| Level | Message | Source | Meaning / action |
|---|---|---|---|
| ERROR | `controller stopped` / `sidecar stopped` | both | The process exited on error. |
| ERROR | `stored report unreadable` / `stored meta unreadable` / `stored pods unreadable` | controller | Corrupt Valkey value; see [Troubleshooting](Troubleshooting). |
| ERROR | `store writer failed` | controller | A writer panicked; pushes of its batch got `503`. Report it. |
| ERROR | `tls ca mismatch` | sidecar | No interception until the pod is recreated. |
| ERROR | `trust volume not published` | sidecar | No interception. |
| ERROR | `recorder panic` | sidecar | One exchange skipped; counted `panic`. |
| ERROR | `redirection failed` | `mimic-init` | The pod's traffic is not captured. |
| WARN | `valkey unreachable` / INFO `valkey reachable` | controller | Readiness flips. |
| WARN | `valkey command failed`, `valkey connection failed` | controller | Once per failure streak. |
| WARN | `store write kept conflicting` | controller | Five optimistic attempts lost; pushes get `503` and are retried. |
| WARN | `store limit reached`, `store evicted` | controller | Once per deployment and reason per process. |
| WARN | `status not updated` | controller | Valkey unavailable; status left as is. |
| WARN | `report push failed` | sidecar | Once per failure streak; INFO `report push recovered` follows. |
| WARN | `report rejected` (`status`) | sidecar | The controller refused the report (400, 405, 507); it is dropped. |
| WARN | `report not shipped` | sidecar | Shutdown ended before the last report was acknowledged; it is lost. |
| WARN | `endpoint limit reached`, `sample budget full` | sidecar | Discovery is now partial. |
| WARN | `grace period over` (sidecar, with `connections`), `grace period over, closing remaining connections` and `grace period over, closing remaining webhook connections` (controller and runtime) | sidecar, controller | Connections were still open at the end of `MIMIC_SHUTDOWN_GRACE`. |
| WARN | `tls passthrough`, `tls roots skipped`, `tls exclude truncated` | see [TLS interception](TLS-interception) | |
| INFO | `report shipping disabled` | sidecar | No `MIMIC_CONTROLLER_URL`: nothing is shipped. |

## Sizing

| What | Default | Guidance |
|---|---|---|
| Valkey memory limit | `1Gi` (`maxmemory` 768Mi) | Watch `mimic_store_report_bytes` and Valkey `used_memory`. A deployment's report is bounded by the store limits (512 endpoints per direction, 4Mi of samples); up to 50 deployments per namespace. Raise `valkey.resources.limits.memory` for many deployments. |
| Valkey PVC | `4Gi` | At least twice `maxmemory`, see [AOF rewrites](#aof-rewrites-and-sizing). |
| Controller memory limit | `768Mi` | The pushes the 32Mi push body budget lets through at once, each held until its write ends, plus allocator headroom. |
| Sidecar memory limit | `128Mi` | Samples up to 16Mi, capture up to 8Mi, plus proxy state. Raise `inject.sidecarResources.limits.memory` for many endpoints or connections. |
| Connections | 16384 per direction per sidecar | Further ones wait in the accept backlog; never refused. |
| Sidecar API | 10 requests/s per source, burst 20; push body max 32Mi | Over the rate: `429` with `Retry-After: 1`. |

Pushes go every 30 s per pod, so controller load grows with pods, not traffic.

## Valkey

- **Persistence.** AOF on a PVC (`valkey.persistence.enabled`). With `false` the store is lost when the
  pod restarts; sidecars rebuild it from new traffic. The AOF is not encrypted by mimic: it holds
  redacted reports only; use an encrypted StorageClass.
- **`noeviction`.** Valkey runs with `maxmemory` at 75 % of its limit and `maxmemory-policy noeviction`.
  When full it refuses writes: pushes get `503` and sidecars keep merging locally within their bounds,
  rather than evicting part of a deployment. Free space by letting data expire, resetting keys, or
  raising the limit. Check with:

  ```bash
  kubectl -n mimic-system exec mimic-valkey-0 -- valkey-cli CONFIG GET maxmemory
  kubectl -n mimic-system exec mimic-valkey-0 -- valkey-cli INFO memory
  ```

  (`REDISCLI_AUTH` is set in the container, so `valkey-cli` authenticates.)

- **Reset.** The store only holds discovery data; it is safe to delete. All mimic keys start with
  `mimic:v1:`. One deployment:

  ```bash
  kubectl -n mimic-system exec mimic-valkey-0 -- sh -c \
    'valkey-cli --scan --pattern "mimic:v1:{demo/users}:*" | xargs -r valkey-cli DEL'
  ```

  (The image is Alpine, so `sh` and `xargs` exist.) Also remove the deployment from the indexes:

  ```bash
  kubectl -n mimic-system exec mimic-valkey-0 -- valkey-cli ZREM mimic:v1:workloads demo/users
  kubectl -n mimic-system exec mimic-valkey-0 -- valkey-cli ZREM mimic:v1:ns:demo users
  ```

  A push that arrives during the delete re-creates part of the data; this is harmless. Everything:
  the same scan with pattern `mimic:v1:*`. Sidecars rebuild from new traffic; counters restart from
  zero. Standalone Valkey 9.0 or later is required; cluster mode is not supported.

### AOF rewrites and sizing

Valkey rewrites the AOF in a forked child while the old file is still in use, so a rewrite briefly
needs both on disk, and copy-on-write pages in memory for what changes meanwhile. The rules:

- **Volume:** `valkey.persistence.size` at least twice `maxmemory`, that is **1.5 times
  `valkey.resources.limits.memory`** (`maxmemory` is 75 % of the limit). The defaults (`1Gi`
  limit, `768Mi` `maxmemory`, `4Gi` volume) leave a margin of 2.5Gi; a limit above about 2.67Gi needs a
  larger volume, and the volume is immutable after install
  ([Install](Install#known-issue-upgrading-a-chart-installed-from-an-earlier-main-build-293)).
  A full volume stops the rewrite and then the appends, and pushes get `503`.
- **Memory:** the 25 % of the limit above `maxmemory` (`256Mi` by default) is the headroom of the
  child's copy-on-write pages and buffers. Pushes arrive every 30 s per pod, so few pages change
  during a rewrite, but with a store near `maxmemory` watch `used_memory` and the container's
  working set; if a rewrite is OOM-killed, raise the memory limit (and the volume with it).
## Retention

Data of a deployment expires `MIMIC_STORE_TTL` (30 days) after its last applied push; endpoints and
samples not seen for that long (counted in whole days) are dropped at the next push
(`mimic_store_expired_total`). Pod entries expire after `min(24 h, TTL)`. Keys outlive a deleted
Deployment until the TTL, and a Deployment re-created with the same name continues them. The read API
cannot delete anything. The chart does not expose `MIMIC_STORE_TTL`.

## Scaling the controller

The controller is stateless; run two or more replicas (`controller.replicas`, default 2). Replicas
converge on one webhook certificate and one TLS seed, any replica can receive any push (applied once,
by sequence number), and each computes the status (no leader; a pod count can differ by one at a window
edge). `PodDisruptionBudget` `maxUnavailable: 1` and preferred anti-affinity spread them. Do not scale
to 0 or drain both at once: the webhook fails closed and pod creation in the selected scope fails.

## Shutdown

On `SIGTERM` every mimic process stops accepting, drains and exits 0 within `MIMIC_SHUTDOWN_GRACE`
(default `10s`). The sidecar receives `SIGTERM` **after the app containers stopped**; it then posts its
last report. Idle HTTP/1 connections close with a FIN, HTTP/2 connections get `GOAWAY`, tunnels run
until the grace ends, then remaining connections are closed with an access line `result: shutdown`.

**Your apps must exit on `SIGTERM`.** The sidecar can only drain once the app containers are gone; an
app that ignores `SIGTERM` is killed at `terminationGracePeriodSeconds`, and the sidecar may not get
time to ship its last report (WARN `report not shipped`). Keep
`terminationGracePeriodSeconds` larger than the app's own stop time plus `MIMIC_SHUTDOWN_GRACE`. A pod
that dies without `SIGTERM` loses its unshipped report: at most one push interval.

## Upgrades and rollouts

See [Install](Install#upgrade). A rolling update of an app pushes into the same key; the data of old
pods stays merged.
