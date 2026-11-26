# Monitoring

mimic exposes Prometheus
metrics on its admin port, and the chart can scrape them. A **bundled Prometheus and Grafana** are an option for a
demo or a dev cluster. A production install brings its own monitoring and uses the ServiceMonitor and the
PodMonitors instead.

## Scrape objects of a production monitoring stack

| Value | Default | What it renders |
|---|---|---|
| `metrics.serviceMonitor.enabled` | `false` | A ServiceMonitor `<release>-controller` for the controller's admin port (8081). Needs the `monitoring.coreos.com` CRDs. |
| `metrics.podMonitor.enabled` | `false` | PodMonitors `<release>-sidecar` (the injected pods, port `mimic-admin` 15100) and `<release>-load` (the load generators). |
| `valkey.metrics.enabled` | `false` | The Redis exporter next to Valkey (port 9121, a read-only ACL user). Enabling it restarts Valkey once. |

All three are off by default, so the default render is unchanged.

The PodMonitors select pods in **every namespace** by default (`metrics.podMonitor.namespaceSelector: {any: true}`),
since a pod can be injected through a `MimicWorkload` alone. The Prometheus that reads them needs `list` and `watch`
on `pods` in each namespace with injected pods (a ClusterRole, or a Role per namespace). Without that RBAC the
namespace has no target and Prometheus logs forbidden errors. To narrow the scrape, set the selector with
`matchNames` (or `matchLabels`) to the namespaces that run injected pods.

## The bundled Prometheus and Grafana

For a demo or a dev cluster only. Both are off by default and rendered by the chart's own templates: there is no
Helm dependency, and nothing is vendored into the chart.

```bash
helm upgrade --install mimic deploy/helm/mimic -n mimic-system \
  --set monitoring.prometheus.enabled=true \
  --set monitoring.grafana.enabled=true \
  --set valkey.metrics.enabled=true
```

- **Prometheus** (`<release>-prometheus`): one replica, 24 h of data in an emptyDir of 1 Gi, with no
  persistent volume. Its scrape jobs are the same targets as the ServiceMonitor and the PodMonitors: the
  controller, the injected sidecars, the load generators and, with `valkey.metrics.enabled`, the Valkey
  exporter. It reads pods only (`list` and `watch`), in the whole cluster or in `monitoring.prometheus.namespaces`.
  It has no admin, lifecycle or remote-write endpoint.
- **Grafana** (`<release>-grafana`): anonymous access is off, analytics, update checks and plugins are off, and
  nothing calls the internet. The datasource `mimic-prometheus` is provisioned. Dashboards are read from the
  ConfigMaps of the release namespace that carry the label `grafana_dashboard: "1"`. The dashboards themselves
  come with #380 to #384.
- **Admin password**: the Secret `<release>-grafana-admin` holds it (`admin-password`, 32 characters, kept across
  upgrades). Read it with:

  ```bash
  kubectl -n mimic-system get secret mimic-grafana-admin -o jsonpath='{.data.admin-password}' | base64 -d; echo
  ```

- **Services**: both are `ClusterIP`, and no ingress is rendered. Reach Grafana with a port-forward:

  ```bash
  kubectl -n mimic-system port-forward svc/mimic-grafana 3000:3000
  ```

  Then open `http://localhost:3000`, log in as `admin` with the password above.

## Mesh metrics

The sidecar exposes these only with `MIMIC_MESH=on` ([Mesh](Mesh)); the controller's own are in the second
table. `service` is `<namespace>/<name>`, `backend` is `<namespace>/<name>:<port>`. No label carries a path, a
header value or another caller-controlled string; at most `MIMIC_METRICS_MAX_ENDPOINTS` pods have a label of
their own (the rest are `other`), and the series of a pod or Service port that left the view are removed 5
minutes later.

| Metric (sidecar) | Type | Labels |
|---|---|---|
| `mimic_mesh_requests_total` | counter, one per mesh-routed request | `service`, `route` (the matched route `<namespace>/<name>`, `none` without a match; at most 256 distinct values, the next ones are `other`, and the series of a route that left the view go 5 minutes later), `backend`, `status_class` (`1xx` to `5xx`, `none`), `reason` (`none`, the `x-mimic-error` of an answer of the sidecar, among them `timeout` (a route timeout expired before the response head, [Mesh](Mesh#timeouts)), `no-route` (no rule of the port's routes matched), `no-backend` (every weight of the rule is 0) and `invalid-backend` (the drawn backend is not valid), `breaker-open` (the circuit breaker answered, [Mesh](Mesh#circuit-breaker)), `fault-injected` (an injected abort, [Mesh](Mesh#fault-injection), with the `status_class` of the configured status), or that of a trusted refusal passed to the app) |
| `mimic_mesh_request_duration_seconds` | histogram, until the response head, spanning every try | `service`, `backend`, `status_class` |
| `mimic_mesh_endpoint_requests_total` | counter, each try written to a pod | `backend`, `endpoint` (`ip:port` or `other`), `status_class` |
| `mimic_mesh_endpoint_in_flight` | gauge, per pod | `backend`, `endpoint` |
| `mimic_mesh_reconnects_total` | counter, requests sent again on a new connection because nothing of them was written | `backend` |
| `mimic_mesh_fallbacks_total` | counter, requests and L4 connections sent to the original destination (the ClusterIP) | `backend` |
| `mimic_mesh_attempts_total` | counter, every try to a pod or to the original destination (a reconnect is not a try) | `backend`, `result` (`ok`, `connect_failed`, `refused_stream`, `reset`, `overloaded`, `queue_timeout`, `timeout`: a route timeout expired before the try's response head) |
| `mimic_mesh_retries_total` | counter, tries after the first | `backend`, `reason` (`connect_failed`, `refused_stream`, `reset`, `app_refused`, `overloaded`, `queue_timeout`) |
| `mimic_mesh_retry_budget_exhausted_total` | counter, retries refused because the retry budget of the Service was used up (see [Mesh](Mesh#retries)) | `backend` |
| `mimic_mesh_retries_skipped_total` | counter, failures of a retried kind that were not retried | `backend`, `reason` (`exhausted`: no try left; `method`: a `reset` of a method that is not idempotent; `buffer`: the body was not held) |
| `mimic_mesh_retry_buffer_bytes` | gauge, request body bytes held for replay, at most `MIMIC_MESH_RETRY_BUFFER_TOTAL` | none |
| `mimic_mesh_mirrored_total` | counter, one per drawn mirror of a routed request ([Mesh](Mesh#mirroring)); the series appears at the first copy | `backend` (the mirror Service port, bounded by the view), `result` (`sent`: a response head came back, any status; `error`: none did, or the deadline passed; `skipped_body`: the body did not fit the replay buffer or did not end in time; `skipped_limit`: no free slot, 256 per mirror Service port and 1024 per sidecar) |
| `mimic_mesh_ejections_total` | counter, one per ejection of a pod by the caller's sidecar ([Mesh](Mesh#outlier-detection)); the series appears at the first ejection | `backend`, `reason` (`consecutive_failures`, `consecutive_5xx`) |
| `mimic_mesh_ejected_endpoints` | gauge, pods of the Service port ejected now, updated every second and at each ejection; 0 for every Service port of the view | `backend` |
| `mimic_mesh_faults_total` | counter, one per fault injected by the caller's sidecar ([Mesh](Mesh#fault-injection)), a delay cut by a route timeout included; the series appears at the first injection | `service` (the Service whose policy injected it: the drawn backend's, bounded by the view; its series go 5 minutes after it left the view), `kind` (`abort`, `delay`) |
| `mimic_mesh_stripped_headers_total` | counter, `x-mimic-*` fields removed from app responses by the inbound side | none |
| `mimic_mesh_inbound_refusals_total` | counter, inbound connections answered `503` `connect-failed` because the app refused the connection (cleartext HTTP/1 and `h2c`) | none |
| `mimic_mesh_unframed_closes_total` | counter, inbound connections closed because the app sent a response the sidecar cannot frame while the fields are removed | none |
| `mimic_mesh_unscoped_connections_total` | counter, outbound connections whose destination is not a Service port of the view | none |
| `mimic_mesh_view_pulls_total` | counter | `result` (`ok`, `not_modified`, `older`, `invalid`, `error`) |
| `mimic_mesh_view_age_seconds`, `mimic_mesh_view_services`, `mimic_mesh_view_endpoints` | gauges | none |

| Metric (controller) | Labels |
|---|---|
| `mimic_mesh_objects` | `kind` (`service`, `endpointslice`, `mimicconfig`, `referencegrant`, `deployment`, `httproute`) |
| `mimic_mesh_gateway_api_present` (gauge, 1 while the `HTTPRoute` CRD is served, else 0) | none |
| `mimic_mesh_routes` (gauge, HTTPRoutes with a Service parent at the last publication) | `state` (`programmed`, `unsupported`) |
| `mimic_mesh_route_status_total` (counter, `status.parents[]` patches of HTTPRoutes) | `result` (`ok`, `conflict`, `error`) |
| `mimic_mesh_view_builds_total` | `result` |
| `mimic_mesh_view_build_duration_seconds`, `mimic_mesh_watches`, `mimic_mesh_view_truncated_total`, `mimic_mesh_view_cache_bytes` | none |

Mirrors: `rate(mimic_mesh_mirrored_total{result!="sent"}[5m])` by `result` shows copies that were lost; `skipped_body`
and `skipped_limit` rising are the bounds of [Mesh](Mesh#mirroring) at work, `error` is the mirror Service failing.

Outlier detection and the breaker: `mimic_mesh_ejected_endpoints` above 0 for a Service port is a pod the caller's
sidecar avoids; `rate(mimic_mesh_ejections_total[10m])` by `reason` shows how often (an ejection time doubles on each
repeat, so a flapping pod shows as a slow rate). `rate(mimic_mesh_requests_total{reason="breaker-open"}[5m])` is the
requests the breaker refused ([Troubleshooting](Troubleshooting#mesh)).

Faults: `sum by (service, kind) (rate(mimic_mesh_faults_total[1m]))` is what the faults of [Mesh](Mesh#fault-injection)
inject, per Service; divided by `sum by (service) (rate(mimic_mesh_requests_total[1m]))` it gives the share, close to
the `percent` of the fault when it matches every request (each sidecar draws on its own). A series that rises outside
a planned test is a `MimicConfig` with `faults` left behind. Injected aborts are also in
`mimic_mesh_requests_total{reason="fault-injected"}`.

A retry rate to watch: `rate(mimic_mesh_retries_total[5m])` by `reason`; `mimic_mesh_retries_skipped_total`
with `reason="exhausted"` rising means requests ran out of tries; `mimic_mesh_retry_budget_exhausted_total`
rising means the budget refused retries (a Service failing for most of its requests).

## Admission metrics

The sidecar of a pod with an [admission contract](Mesh#admission-contract) exposes these (M5, #454). With
`MIMIC_MESH=off` none exists, and before the first contract applied none exists either. No label but `result` and
`kind`, both fixed lists.

| Metric (sidecar) | Type | Labels |
|---|---|---|
| `mimic_admission_requests_total` | counter, "Requests offered to the admission contract, by result". Each offer counts once, when it ends, in one of `admitted` (at once or from the queue), `overloaded`, `queue-timeout` and `cancelled` (its client left while it was queued); `queued` counts the offers that had to wait, which also end in one of the other four. The five series exist from the first contract applied, at 0, and stay after it is removed | `result` (`admitted`, `queued`, `overloaded`, `queue-timeout`, `cancelled`) |
| `mimic_admission_in_flight_seconds_total` | counter, "Integral of the admitted requests in flight over time, in seconds": its `rate()` is the mean in flight. Exists from the first contract applied | none |
| `mimic_admission_in_flight` | gauge, "Admitted requests in flight"; only while a contract is in force | none |
| `mimic_admission_queue_depth` | gauge, "Requests waiting in the admission queue"; only while a contract is in force | none |
| `mimic_admission_limit` | gauge, "Admission contract in force, by kind"; only while a contract is in force | `kind` (`concurrency`, `rps`) |
| `mimic_admission_utilization` | gauge, "Share of the admission contract used, smoothed over 15 s, refused demand included"; not capped at 1; only while a contract is in force | none |

**Demand** is `admitted` + `overloaded` + `queue-timeout`: a request whose client left while it was queued is not
demand. The refusal values of `result` are the `x-mimic-error` values of the refusals and the `reason` of their
access lines, the same strings. On the caller's side the retries of these refusals keep their M2 spelling in
`mimic_mesh_attempts_total{result}` and `mimic_mesh_retries_total{reason}`: `overloaded` and `queue_timeout`
(`_` for `-`).

**What the utilisation means.** Every 100 ms the sidecar takes two samples from the admission counts: the mean
requests in flight and the demand rate (offers that ended admitted, `overloaded` or `queue-timeout`, per second).
Each moves an average with a 15 s time constant (an exponentially weighted moving average that follows the real
time between two ticks), and

```text
mimic_admission_utilization = max(avg(in flight) / concurrency, avg(demand) / rps)
```

with the contract in force. So:

- `0.7` is 70 % of the contract in use, on its tighter limit.
- **Above 1 is refused demand**: more requests were offered than `rps` admits. It is not capped: at twice the
  contract's `rps` it tends to 2. The in-flight term never exceeds 1, since in flight never exceeds `concurrency`.
- It follows a change with the 15 s constant: a load that stops leaves about 72 % (e^(−1/3)) of the value after
  5 s.
- **Absent without a contract**, not 0: the four gauges exist only while a contract is in force, so a pod without
  one has no utilisation series (an HPA reading it sees no metric rather than 0). The averages start at 0 when a
  contract starts, are kept when its values change (and divided by the new values), and are dropped when it is
  removed. The two counters stay.

Useful queries: `max by (pod) (mimic_admission_utilization)` per pod;
`sum by (result) (rate(mimic_admission_requests_total{result=~"overloaded|queue-timeout"}[5m]))` the refusals;
`rate(mimic_admission_in_flight_seconds_total[5m])` the mean in flight of a pod, against
`mimic_admission_limit{kind="concurrency"}`.

The sidecar also writes one INFO `admission contract changed` each time the contract in force changes (set, given
new values, or removed, also when it comes from the persisted view at start), with `concurrency` and `rps` of the
new contract (absent when it was removed), `previous_concurrency` and `previous_rps` (absent when there was none)
and the view's `etag`. A view that keeps the same contract writes no line, and no line is written per refusal.

## Gateway metrics

The controller exposes these with `gateway.enabled` ([Gateway](Gateway)). No label holds a Gateway name, a
namespace, a hostname or another `spec` value.

| Metric (controller) | Type | Labels |
|---|---|---|
| `mimic_gateway_reconciles_total` | counter, reconciles of the Gateways of a mimic class | `result`: `ok` (the objects and the status are correct, after writes or with none), `invalid` (the Gateway is not accepted, so it has no object), `conflict` (an object of the generated name is not managed), `error` (a request failed) |
| `mimic_gateway_gateways` | gauge, the Gateways of a mimic class in the listed namespaces at the last pass | `state` (`programmed`, `pending`, `not_accepted`) |

The controller probes the `gateways` CRD as it probes the `HTTPRoute` CRD. While the CRD is absent,
`mimic_mesh_gateway_api_present` is 0 and the controller applies, writes and deletes nothing for the gateways.

Useful queries: `sum by (state) (mimic_gateway_gateways)` the Gateways by state, and
`sum by (result) (rate(mimic_gateway_reconciles_total{result=~"conflict|error"}[5m]))` the reconciles that did
not finish. A `pending` count that stays above 0 shows a Gateway that does not become programmed
([Troubleshooting](Troubleshooting#gateway)).

The controller also writes one INFO `gateway programmed` (`gateway`, `generation`) when the `Programmed` condition of
a Gateway changes to `True`.

Each gateway pod serves `/metrics` on its admin port 15100, with `component="gateway"` in `mimic_build_info`.

## Dashboards

Five dashboards ship with the chart in `deploy/helm/mimic/dashboards/`: `mimic-controller`, `mimic-sidecar`
(traffic), `mimic-discovery`, `mimic-mock-load` (mock mode and load tests) and `mimic-valkey`. With
`monitoring.grafana.enabled`, or `monitoring.dashboards.enabled` for your own Grafana, the chart renders one
ConfigMap per file, labelled `grafana_dashboard: "1"`; `monitoring.dashboards.annotations` is added to each
(for example `grafana_folder: Mimic`). They are tagged `mimic` and read the metrics listed above.

The `mimic-sidecar` dashboard (variables `namespace` and `deployment`) has two rows for the mesh:

| Row | Panel | Query |
|---|---|---|
| Mesh | Calls by Service and status class | `sum by (service, status_class) (rate(mimic_mesh_requests_total[…]))` |
| Mesh | Call latency p50, p99 by Service | `histogram_quantile` over `mimic_mesh_request_duration_seconds_bucket` by `service` |
| Mesh | Answers of the sidecar by reason | `sum by (reason) (rate(mimic_mesh_requests_total{reason!="none"}[…]))` |
| Mesh | Attempts by backend and result | `sum by (backend, result) (rate(mimic_mesh_attempts_total[…]))` |
| Mesh | Retries by reason, budget exhausted | `mimic_mesh_retries_total` by `reason`; `mimic_mesh_retry_budget_exhausted_total` by `backend` |
| Mesh | Ejections, ejected endpoints (most per pod) | `mimic_mesh_ejections_total` by `backend`; `max by (backend) (mimic_mesh_ejected_endpoints)` |
| Admission | Utilisation per pod | `max by (pod) (mimic_admission_utilization)`, thresholds orange at 0.7 and red at 1 (above 1 is refused demand) |
| Admission | Requests by result | `sum by (result) (rate(mimic_admission_requests_total[…]))` |
| Admission | Refusals by reason | the same, `result=~"overloaded\|queue-timeout"` |
| Admission | In flight (busiest pod), limit | `max(mimic_admission_in_flight)`; `max(mimic_admission_limit{kind="concurrency"})` as a dashed red `limit` |
| Admission | Mean in flight per pod | `sum by (pod) (rate(mimic_admission_in_flight_seconds_total[…]))` |
| Admission | Queue depth (busiest pod) | `max(mimic_admission_queue_depth)` |

The `mimic-mock-load` dashboard has, in its *Load (sidecar)* row, the panel "Admission by result"
(`sum by (result) (rate(mimic_admission_requests_total[…]))`): a load test above the contract shows its
refusals there, next to "Load requests received by result" (`mimic_load_inbound_total`, where they are
`refused_admission`). A pod without a contract shows no admission data.

## Network and an external datasource

With `networkPolicy.enabled` (the default), Prometheus admits queries from the Grafana pods only, and Grafana
admits the release namespace only. Prometheus reaches the scrape ports, the controller and the Valkey exporter.
Grafana reaches Prometheus, DNS and the API server (its dashboard sidecar watches the ConfigMaps). A port-forward
is not blocked by a NetworkPolicy, and kind's default CNI does not enforce NetworkPolicy. Set
`networkPolicy.apiServerCIDR` to narrow the API server egress.

To use a Prometheus outside the release, set `monitoring.grafana.datasourceUrl` **and**
`monitoring.grafana.datasourceEgress`. The second is a list of NetworkPolicy egress rules. Without it, Grafana
cannot reach the external datasource:

```yaml
monitoring:
  grafana:
    datasourceUrl: https://prometheus.example.internal:9090
    datasourceEgress:
      - to: [{ipBlock: {cidr: 10.20.0.0/16}}]
        ports: [{port: 9090, protocol: TCP}]
```

## Limits

- The bundled Prometheus reads pod objects, which show literal env values to it. This is accepted for a demo
  (an accepted limit, see [Security](Security#accepted-limits)). It does not scrape the kubelet or cAdvisor, which need `nodes/proxy`: use the `process_*` series.
- The Prometheus data are lost on a pod restart. Grafana's database is in an emptyDir as well.
- Alerting, long-term storage and ingress are out of scope.
- **The admission metrics lag by up to 100 ms and miss the last tick at exit.** They are taken from the admission
  counts every 100 ms, so a scrape can miss the latest 100 ms of requests, and the requests of the last 100 ms
  before the process exits are never exported. The access lines and `mimic_http_requests_total` count every
  refusal at once.
- **Callers drive the utilisation.** `mimic_admission_utilization` counts refused demand and is not capped, by
  design (the autoscaling of #455 scales on it). Any client that can reach a pod IP, meshed or not, and a namespace
  writer that lowers the contract, raise it; an HPA built on it can then be driven up to its `maxReplicas`.
  NetworkPolicy bounds who can reach the pods, `maxReplicas` bounds the cost, and the contract is the namespace's
  own configuration. The app can move only its own pod's signal, by answering slowly or by calling itself.
