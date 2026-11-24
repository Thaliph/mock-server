# Load test

Step 4 of [Get started](Home#get-started). A `MimicLoadTest` replays the
inbound endpoints that mimic discovered on one Deployment against that Deployment's pods, with no scenario to
write. It runs **only while the Deployment is in [mock mode](Mock-mode)**, so the load does not reach the
real destinations the application calls.

## Turn it on

Off by default, because it adds Job and EndpointSlice RBAC, the `MimicLoadTest` CRD and two
ValidatingAdmissionPolicies. It needs the mimic image pinned by digest, which a released chart has. Turn it on
at install time with `--set loadTest.enabled=true` ([Install](Install#enable-load-tests)), or on a running
release:

```bash
helm upgrade mimic oci://ghcr.io/thaliph/charts/mimic --version 0.2.0-alpha.3 -n mimic-system \
  --reuse-values --set loadTest.enabled=true
kubectl wait --for=condition=Established crd/mimicloadtests.mimic.platform.io --timeout=60s
kubectl -n mimic-system rollout status deploy/mimic-controller
```

Pods injected before the switch do not honour the load marker: `rollout restart` the Deployments you want to
test (`kubectl -n demo rollout restart deploy/front`; do it before the first run, so turn the option on before
you deploy your apps). `loadTest.networkPolicy` (default `true`) gives each generator a NetworkPolicy; it needs a
CNI that enforces NetworkPolicy, and is simply inert where there is none. kind's default CNI enforces it, so
it is active on the cluster of [Try it on kind](Install#try-it-on-kind).

## Run one

The examples use the apps of the [Quick start](Quick-start) and [Mock mode](Mock-mode): the Deployment `front` in
the namespace `demo`, with its `MimicWorkload` `front` and the traffic you sent. `front` is the one that is
load-tested, **in mock**: its calls to `users` are answered by its sidecar, so `users` is not part of the test
and is never called. The Deployment needs discovered inbound traffic, a `MimicWorkload`, mock mode, and at
least one ready pod behind a Service (targets come from EndpointSlices).

First switch it to mock and wait until the pod follows ([Mock mode](Mock-mode#turn-it-on-and-off) has the
wait loop; the load test would also wait, in `WaitingForMock`). Skip this if `front` is still in mock from the
previous page:

```bash
kubectl -n demo patch mimicworkload front --type merge -p '{"spec":{"mode":"mock"}}'
```

To show that `users` is not called, count its access lines now and after the run (each request to `users`
writes one line; `--tail=-1` because a selector prints 10 lines by default):

```bash
kubectl -n demo logs -l app=users -c app --tail=-1 | wc -l
```

Then create the run. This one lasts a minute:

```bash
kubectl apply -f - <<'EOF'
apiVersion: mimic.platform.io/v1alpha1
kind: MimicLoadTest
metadata: {name: front-1, namespace: demo}
spec:
  deployment: front
  duration: 1m
  concurrency: 10
  rate: 50
  rampUp: 10s
EOF
kubectl -n demo wait mimicloadtest/front-1 --for=jsonpath='{.status.phase}'=Succeeded --timeout=5m
```

To watch it live instead, `kubectl -n demo get mimicloadtest front-1 -w` (Ctrl-C to stop). When `Phase` is
`Succeeded`, count the access lines of `users` again: the number
is the same, the load test never reached it (it would be the same with `users` scaled to 0). The plan comes
from the inbound endpoints of `front`: `GET` and `PUT /front/users/{users_id}`. All the fields:

```yaml
apiVersion: mimic.platform.io/v1alpha1
kind: MimicLoadTest
metadata: {name: front-1, namespace: demo}
spec:
  deployment: front        # required
  duration: 5m             # required, 10s to MIMIC_LOAD_MAX_DURATION (30m)
  concurrency: 10          # requests in flight at most, default 10, up to 256
  rate: 200                # optional requests/s, up to 2000; absent: closed model
  rampUp: 30s              # default 0s
  timeout: 10s             # per request
  endpoints:
    include: ["GET /front/users/{users_id}"]     # absent: every usable endpoint
    exclude: ["PUT /front/users/{users_id}"]
  credentials:             # optional, see "Testing with credentials"
    secretName: front-load
    fields:
      - {field: header/authorization, key: token}
  stop: false              # set to true to stop the run
```

Every field but `stop` is immutable. `kubectl get mimicloadtest -n demo` prints `Deployment`, `Phase`,
`Reason`, `Requests`, `RPS`, `P99 Micros` and `Age` (kubectl upper-cases the header: `P99 MICROS`; the value is
the p99 latency in microseconds). With a `rate` the open model applies: when every concurrency
slot is busy, a send is counted as `missed`, not queued. The `RPS` of a run is its requests over its whole
duration, ramp-up included, so it stays a little under `rate`.

## What is sent

The controller builds a plan from the stored inbound report: each endpoint is drawn by weight (its recorded
exchanges), then one of its recorded samples. Fields that discovery marked volatile (timestamps, UUIDs, ids)
get fresh values that keep their shape. Values that were redacted are sent as their placeholder
(`Authorization: Bearer mimic-token`), unless `credentials.fields` supplies the real value from your Secret,
which the author of the object must be allowed to `get` (see [Testing with credentials](#testing-with-credentials)).
A placeholder that is not a value cannot be replayed: a sample with one in a path segment, a header name, a query key, a form key or a JSON object key is unusable, and an endpoint with no usable sample is skipped as `credentials`. Skipped endpoints are counted by reason in `status.skipped` (`grpc`, `upgrade`, `excluded`,
`no_sample`, `credentials`, `opaque_body`, `limit`, `plan_size`). HTTP/1.1 cleartext only; gRPC is skipped.

## Testing with credentials

An authenticated endpoint rejects a request that carries a placeholder, unless you choose one of two ways.
Without either, it answers `4xx`.

1. **A Secret with real test values**, mapped with `credentials.fields`. Each entry replaces the whole value of
   one field with the bytes of a Secret key (a trailing newline removed); the other credential fields keep
   their placeholder. The Secret is read only by the generator, never reaches sidecars or Valkey.

   Create the Secret in the namespace of the `MimicLoadTest` (a dummy value here; use a token that your test
   environment accepts):

   ```bash
   kubectl -n demo create secret generic front-load \
     --from-literal=token='Bearer dummy-test-token' --from-literal=password='dummy-test-password'
   ```

   The Quick start `PUT`s that `front` received also carry a `password` in their JSON body, so the Secret has a second key for it. An
   endpoint is never skipped because of a credential value: a field you do not map is sent with its
   placeholder (`mimic-password` here).

   Then a complete run that uses it. Every field but `stop` is immutable, so a change of spec needs a **new
   name** (`front-2`, not `front-1`):

   ```bash
   kubectl apply -f - <<'EOF'
   apiVersion: mimic.platform.io/v1alpha1
   kind: MimicLoadTest
   metadata: {name: front-2, namespace: demo}
   spec:
     deployment: front
     duration: 1m
     concurrency: 10
     rate: 50
     rampUp: 10s
     credentials:
       secretName: front-load        # key `token` holds `Bearer <test token>`
       fields:
         - {field: header/authorization, key: token}
         - {field: body/password, key: password}
   EOF
   kubectl -n demo wait mimicloadtest/front-2 --for=jsonpath='{.status.phase}'=Succeeded --timeout=5m
   ```

   **Checking that the credential was used.** Only a request whose recorded sample carried the field gets the
   Secret value: in the Quick start only the 5 `PUT`s sent an `Authorization` header (and a `password` in
   the body), so the `GET`s are unchanged. The demo apps do not check authentication, so a run with and
   without credentials look the same here. With your own app, compare `statuses` in the results of the two runs
   (for example `4xx` for 401s without credentials, `2xx` with them) to confirm that the credentials work. The token itself is never in `status`, in the read API or in the logs.

   `field` names the value to replace: `header/<lower-case header name>`, `query/<key>` or
   `body<JSON pointer>` (for example `body/card/token`); up to 32 distinct fields, each with the Secret `key`
   that holds its value.

2. **A test environment that accepts mimic's placeholders**, for example a test identity provider or a gateway
   rule that accepts `Authorization: Bearer mimic-token`. The 13 placeholders are `mimic-auth`, `mimic-token`,
   `mimic-jwt`, `mimic-api-key`, `mimic-password`, `mimic-private-key`, `mimic-secret`, `mimic-session`,
   `mimic-cookie`, `mimic-signature`, `mimic-card`, `mimic-iban` and `mimic-ssn`; see
   [Discovery and redaction](Discovery-and-redaction).

Limit: a real value exactly equal to a placeholder, for example a field that really holds
`mimic-token`, is treated as a placeholder. Mock matching skips that field and a load test treats it as a
credential field. Placeholders are public constants, not secrets.

## Safety: mock only

- The generator connects to **pod IPs** from EndpointSlices, never to a Service address or a name.
- Before each connection it asks the sidecar (`GET :15100/load`) for the run, the pod and `mode: mock`.
- Each load request carries a signed ticket. A sidecar that is not in mock, or not of that deployment, answers
  `599` with `x-mimic-load: refused` and the application gets nothing. Load requests are not recorded in
  discovery.
- The first refusal of the load marker or unverified answer ends the run (`Aborted`, `ModeLeft` or `Unverified`),
  and so does the deployment leaving mock. The controller then deletes the Job.
- One kind of answer without `x-mimic-load` does not end the run: an **inbound refusal** of the target's own
  sidecar, which never reached the app ([admission contract](Mesh#admission-contract), or the app refusing the
  connection). It is a `503` with no `x-mimic-load` field, exactly one `x-mimic-error` (`overloaded`,
  `queue-timeout` or `connect-failed`), exactly one `x-mimic-origin: inbound; pod=<the target's pod name>` and
  exactly one `x-mimic-endpoint` equal to the `<ip>:<port>` the generator dialled. It is counted in `mimicErrors`
  ([Results](#results)) and the run goes on. Any other answer without `x-mimic-load: ok` (another pod's name,
  another address, a repeated field, a `500`, `breaker-open`) is still `Unverified`.
- A refusal never verifies a target: it proves that a mimic sidecar answered, not that the pod is in mock. On a
  connection that already had an `x-mimic-load: ok` answer it is counted and the connection is kept. When it is
  the first answer of a new connection, it is counted, the connection is dropped and the next request to that
  target probes `:15100/load` again (PM decision of 2026-11-19 on #454).

## Results

While the object exists, `kubectl -n demo get mimicloadtest front-1 -o json | jq .status.results` prints them.
The run also stays readable after the object is deleted: see
[Load test runs in Reading the API](Reading-the-API#load-test-runs) for the routes
(`/v1/workloads/demo/front/loadtests/<uid>`, where the uid is the object's `metadata.uid`, not its name) and a
command that prints the per-endpoint summary.
The UI lists the runs too: the Load tests tab (`/ui/loadtests`) shows every stored run and opens a run page with
its per-endpoint results (see [Reading the API](Reading-the-API)).

A run goes `Pending` (`WaitingForMock` while the mode converges) to `Running` to `Succeeded`, or `Failed` or
`Aborted` with a `reason`. `status.results` holds, in total and per endpoint, the requests, missed sends,
statuses by class, errors by kind, requests per second and p50, p90, p99 and max latency in microseconds. They
also hold `mimicErrors` when the targets' sidecars refused requests: the inbound refusals by their `x-mimic-error`
value (keys `connect-failed`, `overloaded`, `queue-timeout`; the field is absent when there was none). A refusal
counts in `requests` and `rps`, not in `statuses` nor in the latency, which stay those of the app's answers, so
`requests` is the sum of `statuses`, `errors` and `mimicErrors`:

```bash
kubectl -n demo get mimicloadtest front-1 -o json | jq '.status.results.total | {requests, statuses, mimicErrors}'
```

On the generator's `/metrics`, `mimic_load_requests_total{result}` has the same three reasons as values of
`result`, and a refusal is not observed in `mimic_load_request_duration_seconds`. No log line is written per
refusal. They are also in Valkey (the last 20 runs per deployment, shown in the UI and served by the read API), in the
generator's `/metrics`, and in the [Mock mode and load tests dashboard](Monitoring#dashboards). Latencies are bucket bounds, at most 4.4 %
high, and measured by the client, so they include the network and the generator.

## Finding and setting the contract

An [admission contract](Mesh#admission-contract) (`concurrency` and `rps` per pod) should sit a little below what
one pod serves before its latency breaks. A load test finds that point and then checks the contract: its requests
go to the pod addresses and through the contract like any other request. It needs the mesh on (`MIMIC_MESH=on`)
and the deployment in mock, as every run. Scale the deployment to one replica first, so that the numbers of a run
are those of one pod (`kubectl -n demo scale deploy/front --replicas=1`).

1. **Find where latency breaks, without a contract.** Run the open model (`rate` set) at rising rates, with a
   `concurrency` high enough that few sends are `missed`. A new name for each run, since the spec is immutable:

   ```bash
   for rate in 50 100 200 400; do
     kubectl apply -f - <<EOF
   apiVersion: mimic.platform.io/v1alpha1
   kind: MimicLoadTest
   metadata: {name: front-r$rate, namespace: demo}
   spec: {deployment: front, duration: 2m, concurrency: 64, rate: $rate}
   EOF
     kubectl -n demo wait mimicloadtest/front-r$rate --for=jsonpath='{.status.phase}'=Succeeded --timeout=10m
     kubectl -n demo get mimicloadtest front-r$rate -o json \
       | jq -c --arg rate "$rate" '.status.results.total | {rate: $rate, rps, missed, latencyMicros}'
   done
   ```

   The open model sends on its schedule whatever the answers, so past the pod's capacity the p99 grows fast and
   `missed` rises (every concurrency slot is busy). Note the last rate before that, and its latency: the app then
   has about `rate × p50` requests in flight.

2. **Set a contract below it**, for example 80 % of that rate and of those requests in flight, in the Deployment's
   `MimicConfig`. The oldest config of a Deployment wins as a whole, so add `admission` to its existing config:

   ```yaml
   apiVersion: mimic.platform.io/v1alpha1
   kind: MimicConfig
   metadata: {name: front, namespace: demo}
   spec:
     targetRefs: [{group: apps, kind: Deployment, name: front}]
     admission: {concurrency: 8, rps: 80}
   ```

   Each sidecar writes one INFO `admission contract changed` with the new `concurrency` and `rps` when it applies
   it (`kubectl -n demo logs deploy/front -c mimic-sidecar | grep 'admission contract changed'`). Count the
   long-lived responses of the app (SSE, gRPC streams): each holds a slot for its whole life.

3. **Run again above the contract.** The same open-model run at the rate where latency broke now ends
   `Succeeded` with `mimicErrors` in its results instead of the latency collapse: the requests over the contract
   are refused `overloaded` or `queue-timeout` by the pod's sidecar, and `statuses` and `latencyMicros` are those of
   the admitted requests. The app never has more than `concurrency` requests in flight and an admitted request
   waits at most 150 ms in the queue, so their latency should stay close to that of step 1 below the break. On the pod,
   `mimic_admission_requests_total{result}` shows the same refusals, and `mimic_load_inbound_total` counts them
   `refused_admission` ([Monitoring](Monitoring#admission-metrics)).

4. **Read the utilisation at the planned load.** Run at the load you expect in production and read
   `mimic_admission_utilization` on each pod (the *Admission* row of the `mimic-sidecar` dashboard): `0.7` means
   70 % of the contract in use, smoothed over 15 s. The autoscaling of #455 targets `averageValue: 700m`, which is
   that 70 %. A value above 1 means demand over the contract was refused. Then scale back: the capacity is
   `replicas × contract`.

**Why not the closed model.** In the closed model (no `rate`) a worker sends its next request as soon as it has an
answer, and an `overloaded` refusal comes at once. Over the contract the workers then spin: the pod receives
refusals as fast as its sidecar writes them, within the run's concurrency, and the results fill with
`mimicErrors`. The load stays on the deployment under test and its sidecar, but it says nothing about the
contract. Use the open model, whose sends do not depend on the answers.

## Limits

| Limit | Default | Setting |
|---|---|---|
| Active runs, cluster / namespace / deployment | 4 / 2 / 1 | `MIMIC_LOAD_MAX_RUNS`, `MIMIC_LOAD_MAX_NAMESPACE_RUNS` |
| Concurrency per generator pod | 128 | `MIMIC_LOAD_POD_CONCURRENCY` |
| Endpoints in a plan | 64 | `MIMIC_LOAD_MAX_ENDPOINTS` |
| Wait in `Pending` | 5m | `MIMIC_LOAD_START_TIMEOUT` |

A run over a limit fails with `Busy`, `NamespaceBusy`, `ClusterBusy`, `StoreFull` or `Invalid`.

Accepted limits: requests in flight when the deployment leaves mock can still reach real
destinations (at most the concurrency); the application's real datastores and `spec.mock.passthrough`
destinations are loaded too; load and credentials cross the network in cleartext; a secret that redaction
misses is replayed, so add `spec.discovery.redact`; the namespace is the trust domain of a run. See
[Security](Security#accepted-limits).

Admission refusals (from #454):

- **A load test no longer stops at the first admission refusal.** Until #454 a refusal of the
  [admission contract](Mesh#admission-contract) ended the run `Unverified`; the generator now counts the inbound
  refusals of its targets in `mimicErrors` under their reason.
- **A server that passes the probe can pass refusals off as its own.** The generator trusts an inbound refusal on
  its fields alone, as it trusts `x-mimic-load: ok`: no signature binds either to the sidecar. A server at a target
  address that answers the `/load` probe as the right pod can already return `x-mimic-load: ok`, so a forged
  refusal gains it nothing more: the load it receives stays within the run's rate and concurrency, and the forged
  answers are counted in `mimicErrors`. The app behind a real sidecar cannot forge one: its answers carry
  `x-mimic-load: ok` and are counted by status.
- **A closed-model load test against a refusing pod spins.** An `overloaded` answer comes at once and a closed-model
  worker sends its next request as soon as it has an answer, so a pod over its contract receives refusals as fast
  as its sidecar writes them, within the run's concurrency. There is no pause after a refusal. To check a contract,
  use the open model ([Finding and setting the contract](#finding-and-setting-the-contract)).

## Next

Step 5 of Get started: [Reading the API](Reading-the-API) reads the run that you just made from the read API, and
ends with the clean-up.
