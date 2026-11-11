# Load test

Covers F11. Reference: spec §7.8, §10.2, §11.1, §12.12 (LDT-01 to LDT-20). A `MimicLoadTest` replays the
inbound endpoints that mimic discovered on one Deployment against that Deployment's pods, with no scenario to
write. It runs **only while the Deployment is in [mock mode](Mock-mode)**, so the load does not reach the
real destinations the application calls.

## Turn it on

Off by default, because it adds Job and EndpointSlice RBAC, the `MimicLoadTest` CRD and two
ValidatingAdmissionPolicies. It needs the mimic image pinned by digest, which a released chart has:

```bash
helm upgrade mimic oci://ghcr.io/thaliph/charts/mimic --version 0.2.0-alpha.1 -n mimic-system \
  --reuse-values --set loadTest.enabled=true
```

Pods injected before the switch do not honour the load marker: `rollout restart` the Deployments you want to
test. `loadTest.networkPolicy` (default `true`) gives each generator a NetworkPolicy; it needs a CNI that
enforces NetworkPolicy.

## Run one

The Deployment needs discovered inbound traffic, a `MimicWorkload`, mock mode, and at least one ready pod
behind a Service (targets come from EndpointSlices):

```bash
kubectl -n shop patch mimicworkload payment --type merge -p '{"spec":{"mode":"mock"}}'
```

```yaml
apiVersion: mimic.platform.io/v1alpha1
kind: MimicLoadTest
metadata: {name: payment-1, namespace: shop}
spec:
  deployment: payment      # required
  duration: 5m             # required, 10s to MIMIC_LOAD_MAX_DURATION (30m)
  concurrency: 10          # requests in flight at most, default 10, up to 256
  rate: 200                # optional requests/s, up to 2000; absent: closed model
  rampUp: 30s              # default 0s
  timeout: 10s             # per request
  endpoints:
    include: ["POST /payments"]            # absent: every usable endpoint
    exclude: ["DELETE /payments/{payments_id}"]
  credentials:             # optional
    secretName: payment-load
    fields:
      - {field: header/authorization, key: token}
  stop: false              # set to true to stop the run
```

Every field but `stop` is immutable. `kubectl get mimicloadtest -n shop` prints `Deployment`, `Phase`,
`Reason`, `Requests`, `RPS`, `P99 µs` and `Age`. With a `rate` the open model applies: when every concurrency
slot is busy, a send is counted as `missed`, not queued.

## What is sent

The controller builds a plan from the stored inbound report: each endpoint is drawn by weight (its recorded
exchanges), then one of its recorded samples. Fields that discovery marked volatile (timestamps, UUIDs, ids)
get fresh values that keep their shape. Values that were redacted are not replayed: an endpoint that needs one
is skipped unless `credentials.fields` supplies it from your Secret, which the author of the object must be
allowed to `get`. Skipped endpoints are counted by reason in `status.skipped` (`grpc`, `upgrade`, `excluded`,
`no_sample`, `credentials`, `opaque_body`, `limit`, `plan_size`). HTTP/1.1 cleartext only; gRPC is skipped.

## Safety: mock only

- The generator connects to **pod IPs** from EndpointSlices, never to a Service address or a name.
- Before each connection it asks the sidecar (`GET :15100/load`) for the run, the pod and `mode: mock`.
- Each load request carries a signed ticket. A sidecar that is not in mock, or not of that deployment, answers
  `599` with `x-mimic-load: refused` and the application gets nothing. Load requests are not recorded in
  discovery.
- The first refusal or unverified answer ends the run (`Aborted`, `ModeLeft` or `Unverified`), and so does the
  deployment leaving mock. The controller then deletes the Job.

## Results

A run goes `Pending` (`WaitingForMock` while the mode converges) to `Running` to `Succeeded`, or `Failed` or
`Aborted` with a `reason`. `status.results` holds, in total and per endpoint, the requests, missed sends,
statuses by class, errors by kind, requests per second and p50, p90, p99 and max latency in microseconds. They
are also in Valkey (the last 20 runs per deployment, shown in the UI), in the generator's `/metrics`, and in
the [Mock mode and load tests dashboard](Monitoring#dashboards). Latencies are bucket bounds, at most 4.4 %
high, and measured by the client, so they include the network and the generator.

## Limits

| Limit | Default | Setting |
|---|---|---|
| Active runs, cluster / namespace / deployment | 4 / 2 / 1 | `MIMIC_LOAD_MAX_RUNS`, `MIMIC_LOAD_MAX_NAMESPACE_RUNS` |
| Concurrency per generator pod | 128 | `MIMIC_LOAD_POD_CONCURRENCY` |
| Endpoints in a plan | 64 | `MIMIC_LOAD_MAX_ENDPOINTS` |
| Wait in `Pending` | 5m | `MIMIC_LOAD_START_TIMEOUT` |

A run over a limit fails with `Busy`, `NamespaceBusy`, `ClusterBusy`, `StoreFull` or `Invalid`.

Accepted limits (spec §11.1): requests in flight when the deployment leaves mock can still reach real
destinations (at most the concurrency); the application's real datastores and `spec.mock.passthrough`
destinations are loaded too; load and credentials cross the network in cleartext; a secret that redaction
misses is replayed, so add `spec.discovery.redact`; the namespace is the trust domain of a run. See
[Security](Security#accepted-limits).
