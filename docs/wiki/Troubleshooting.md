# Troubleshooting

Symptom, then cause and fix. Messages are the ones mimic logs; see [Operations](Operations#logs) for how
to read the logs, and [Access logs](Access-logs) for the sidecar's access lines.

## Injection

**A pod has no mimic containers.**
Check the annotation: `kubectl -n demo get pod <pod> -o jsonpath='{.metadata.annotations}'`.
- `mimic.platform.io/skipped: <reason>`: `host-network` (not supported), `unsupported-owner` (not owned
  by a Deployment), `port-conflict` (the app declares 15001, 15006 or 15100), `uid-conflict` (UID 1337
  in use), `name-conflict` (a container or volume name mimic needs is taken).
- No annotation: the pod is out of scope. Label the namespace (`mimic.platform.io/inject=enabled`), or
  use `selected` plus a `MimicWorkload` for the Deployment; the pod label `disabled` opts out.
  Injection only happens at pod creation: `kubectl rollout restart deploy/<name>`.

**ReplicaSet shows `FailedCreate` ... `violates PodSecurity "baseline"` (or `restricted`).**
`mimic-init` needs `NET_ADMIN`/`NET_RAW`. Set `pod-security.kubernetes.io/enforce=privileged` on the
application namespace.

**Pods cannot be created in an injected namespace: `failed calling webhook ...inject.mimic.platform.io`.**
The webhook fails closed and the controller is unreachable or not ready. Check
`kubectl -n mimic-system get pods`, `rollout status deploy/mimic-controller`, then the controller logs.
An admission that needs the TLS seed is answered `allowed: false` while the seed is not loaded. Also
check that a NetworkPolicy of yours does not block the API server from port 8443.

**A `MimicWorkload` or label change had no effect.** It applies to pods created afterwards:
`kubectl rollout restart`. This includes `spec.tls.exclude` and `spec.discovery.redact`.

**App containers do not start, the pod stays in init.** The sidecar is ready only once its listeners
are bound and it published the trust volume. `kubectl logs <pod> -c mimic-init` (ERROR
`redirection failed` means the iptables rules were not installed; the nodes need the nft backend) and
`-c mimic-sidecar`.

## Controller

**Controller not Ready.** Readiness needs Valkey, the webhook certificate, the `MimicWorkload` cache
and the TLS seed. Logs: `valkey unreachable`, `webhook certificate not in place yet`,
`tls seed not in place yet`, `no Kubernetes client yet`. Fix Valkey first:
`kubectl -n mimic-system get pods,pvc`; a Valkey stuck Pending usually has no StorageClass.

**A process exits immediately with code 2.** An invalid or missing `MIMIC_*` value; stderr names it
(`MIMIC_X="value": reason` or `MIMIC_X is required`). For the controller check the Helm value that maps
to it ([Configuration](Configuration)).

**`helm upgrade` fails on the Valkey StatefulSet** (`updates to statefulset spec ... are forbidden`).
The PVC size default changed between `main` builds; see [Install](Install#known-issue-upgrading-a-chart-installed-from-an-earlier-main-build-293).

**GitOps shows a diff on the webhook `caBundle` or Secrets.** The controller owns them; tell your tool
to ignore those fields.

## Mesh

**Log line `gateway api crds missing`, `mimic_mesh_gateway_api_present` 0.** The `HTTPRoute` CRD is not served.
The mesh works without routes and without `ReferenceGrant` consent. Install the standard channel
([Install](Install#gateway-api-optional)); the controller notices within 30 s (`gateway api crds found`). The
line is written once per absence.

**An `HTTPRoute` has `Accepted: False`, reason `UnsupportedValue`.** The message names the first field the
controller does not support (`unsupported: rules[0].filters[0].type: URLRewrite`); the route is in no view. Remove
it or use a supported filter ([Mesh](Mesh#routes)). Check also that the CRDs are the standard channel, v1.2 or later.

**An `HTTPRoute` has `ResolvedRefs: False`, reason `RefNotPermitted`.** A backend in another namespace has no
`ReferenceGrant` there with `from` `gateway.networking.k8s.io` `HTTPRoute` of the route's namespace and `to` the
core `Service`. The message names `<namespace>/<name>`, the same whether it exists. A grant for `MimicConfig`
does not count. `InvalidKind` and `BackendNotFound` mean the backend is not a core Service, or does not exist or
lacks the port. A route with no status entry has no Service parent that exists.

**`httproute status failed` in the controller logs.** Writing the route's status failed (the line names the route
and the error); it is tried again after 30 s and counted in `mimic_mesh_route_status_total{result="error"}`.

**`404` with `x-mimic-error: no-route`.** The Service port has an `HTTPRoute` and no rule of it matches the request.
This is the default, not a failure of the pod: nothing was sent. Check the `matches` of the route against the path
as the app sent it (case-sensitive, no normalisation; `PathPrefix: /cart` does not match `/carts`), the method and
the headers, and add a catch-all rule that names the Service itself to keep the old behaviour for the rest
([Mesh](Mesh#routes)). `mimic_mesh_requests_total{reason="no-route"}` counts them; the access line has no `route`.

**`500` with `x-mimic-error: no-backend`.** The rule that matched has no backend, or every weight is `0`. Give a
backend a weight above 0.

**`500` with `x-mimic-error: invalid-backend`.** The rule drew a backend that is not valid: the Service is not in
the caller's view, lacks the port, or the route has no `ReferenceGrant` for a backend of another namespace. Only
that backend's share of the requests fails. `kubectl describe httproute` shows `ResolvedRefs: False` and why.

**`503` with `x-mimic-error: no-endpoint`.** The sidecar's view has the Service port with no ready
endpoint. Check `kubectl get endpointslices -l kubernetes.io/service-name=<svc>`, the readiness of the
pods and that the target port name matches. The answer is generated by the sidecar, not by the callee,
and is not recorded; `mimic_mesh_requests_total{reason="no-endpoint"}` counts them. The mesh does not
retry through the ClusterIP, because kube-proxy would have no endpoint either. See [Mesh](Mesh).

**Log line `mesh view stale`, `mimic_mesh_view_age_seconds` above 60.** The sidecar has not heard from
the controller for `MIMIC_MESH_STALE_AFTER`. Calls still work, through the ClusterIP and not balanced by
the sidecar (`mimic_mesh_fallbacks_total` rises). Check the controller pods, the sidecar's
`mesh view pull failed` lines (`kubectl logs <pod> -c mimic-sidecar`) and any NetworkPolicy towards the
controller's sidecar API port. The sidecar goes back to pod IPs by itself at the next successful pull.

**`503 watch_limit` in the controller logs.** A pod holds more than 2 view long-polls, or a namespace
more than 25 % of the watch budget (`MIMIC_SIDECAR_API_MAX_WATCHES_PER_POD`,
`MIMIC_SIDECAR_API_WATCH_NAMESPACE_PERCENT`). Sidecars retry after `Retry-After: 1`; pushes and
certificates are not affected.

**A request was tried more than once, or a pod saw the same request twice.** Expected after a connect
failure, a refused stream, a trusted refusal (any method) or a `reset` before any response head (only
`GET`, `HEAD`, `OPTIONS`, `TRACE`, `PUT`, `DELETE`). The access line has `attempts` above 1 and
`mimic_mesh_retries_total{reason}` says why; at most 4 tries per request and per tier. An app whose
idempotent-by-name methods have side effects can see such a request twice. See [Mesh](Mesh#retries).

**A `POST` got a closed connection (or a reset stream) and no retry.** A reset after the request was written is
retried only for the six idempotent methods: `mimic_mesh_retries_skipped_total{reason="method"}`. A body
larger than `MIMIC_MESH_RETRY_BUFFER` (64 KiB) is not retried once written: `reason="buffer"`. A `5xx` answer
of the app is never retried.

**The sidecar logs `retry budget exhausted`, and failures are not retried.** The Service the `backend`
field names failed so often that the retries of this pod used up its budget (by default 100 retries per 10 s
plus 20 % of the requests): the failure is returned as it is, counted in
`mimic_mesh_retry_budget_exhausted_total{backend}`; the line is written at most once per second per backend
(`suppressed` counts the others). Fix the failing pods; to allow more retries raise `retryBudget` in a
`MimicConfig` on the Service ([Mesh](Mesh#retries)). `retries: 0` turns the retries off on purpose.

**`503` with `x-mimic-error: breaker-open`.** The caller's sidecar refused the request at once; nothing was sent to
any pod. Two causes, named by the `cause` field of the WARN line `breaker open` (written at most once per second
per Service, with `suppressed` counting the others):

- `max_requests`: the caller pod already had `maxRequests` requests in flight to that Service
  ([Mesh](Mesh#circuit-breaker)). Pods that are slow, or streams that stay open (SSE, gRPC), hold the slots. Look at
  `in_flight` and `max_requests` in the line; raise `circuitBreaker.maxRequests` in the `MimicConfig` of the Service
  or find why the pods are slow. The limit is per caller pod.
- `all_ejected`: every endpoint of the port is ejected (possible only with `maxEjectionPercent: 100`). See the
  next entry.

`mimic_mesh_requests_total{reason="breaker-open"}` counts them and the access line has `reason: breaker-open`.
The answer is never retried and names no pod.

**Log lines `endpoint ejected` and `endpoint restored`; `mimic_mesh_ejected_endpoints` above 0.** The caller's
sidecar stopped sending requests to a pod after `consecutiveFailures` failures in a row (connect failures, resets
before any response head, refusals of its app) or, with `consecutive5xx`, `5xx` answers: `reason` says which, `endpoint` is the pod `ip:port` and
`ejection_seconds` how long (30 s, doubling at each repeat up to 300 s). `endpoint restored` follows with
`cause: elapsed`, or `max_percent` when a view lowered the bound. A pod is **not** ejected when it is the only one
of its Service port, or when `⌊n × maxEjectionPercent / 100⌋` pods are already ejected (at 50 %, 1 of 2 or 3);
then it is only avoided for 5 s after each failed connect. A pod that is busy or refuses by admission (`overloaded`)
is not ejected, nor one whose requests time out under a route. Ejection is per caller pod: other callers may still
send to it. Look at the pod (`kubectl logs`, its readiness) rather than at the mesh.

**`504` with `x-mimic-error: timeout`.** A route timeout expired before the pod's response head
([Mesh](Mesh#timeouts)). The access line has `result: timeout` and `timeout: request` or `backendRequest`
(which of the rule's two it was); `mimic_mesh_requests_total{reason="timeout"}` rises. Raise the timeout in the
`HTTPRoute` rule, or find why the pod is slow. A timeout is never retried, and the pod may still have processed
the request. When it expired after the head the app sees a cut response (a closed HTTP/1 connection, an HTTP/2
`INTERNAL_ERROR`) and the access line has the pod's status. A producer route in the callee's namespace binds
callers that have no route of their own: check `kubectl get httproute -A`.

**Any status (often `503`) with `x-mimic-error: fault-injected`.** A fault of the backend Service's `MimicConfig`
aborted the call in the caller's own sidecar ([Mesh](Mesh#fault-injection)); nothing was sent to any pod. The
caller's access line has `fault: abort`, `reason: fault-injected`, the configured `status` and the `backend`
whose policy injected it, and no `upstream`; `mimic_mesh_faults_total{service,kind="abort"}` and
`mimic_mesh_requests_total{reason="fault-injected"}` count them. Find the fault in the namespace of that Service:

```bash
kubectl -n <ns> get mimicconfig -o json \
  | jq '.items[] | select(.spec.faults) | {name: .metadata.name, targets: .spec.targetRefs, faults: .spec.faults}'
```

Remove it when the test is over: the calls are normal again from the caller sidecar's next `mesh view applied`
line. The answer is never retried and names no pod. When the
caller's line has **no** `fault` but an `upstream`, the `fault-injected` answer came from a pod: that app forwarded
an abort its own sidecar injected into one of its calls, so look one tier further down.

**Calls to a Service take a fixed extra time, or all time out together.** The access lines of the caller have
`fault: delay` and `fault_delay_ms` (the time injected; `duration_ms` and `head_ms` include it), and
`mimic_mesh_faults_total{kind="delay"}` rises: a delay fault of the Service, same search as above. The delay counts
against the rule's `timeouts.request`, so a delay longer than it gives `504` `timeout` with `fault: delay` on the
line and nothing sent to a pod. Delayed calls hold their breaker slot during the wait, so `breaker-open` answers
can follow.

**A fault does not apply.** `mimic_mesh_faults_total` has no series for the Service on the caller's sidecar:

- The `MimicConfig` is not the Service's winning one: `Accepted: False` with `Conflicted` (an older config
  targets the Service: put `faults` in that one), `Invalid` (the message names the field and the item index) or
  `TargetNotFound` (no such Service in that namespace; `targetRefs` cannot name another namespace).
- The caller sidecar has not applied the view yet: wait for its next `mesh view applied` line.
- The route sends the call to another backend Service: the faults that apply are the **drawn backend's**, not the
  parent Service's.
- An earlier item matches first: the faults are tried in order and only the first whose `match` holds applies. A
  `match` sees the request as the app sent it, before the route's `RequestHeaderModifier`.
- The call is never faulted: a caller without a sidecar or with the mesh off, mode `mock` (the mock answers), a stale
  view, `Upgrade`, `CONNECT`, an L4 connection (opaque TCP, TLS passed through), inbound traffic, a mirror copy, or a
  connection to a `spec.mock.passthrough` destination accepted while in `mock` (byte relay, #549: run the test in
  `discovery`).
- `percent` is drawn per request by each caller sidecar: at low traffic a small percentage may not show yet.

**Mirrored requests do not arrive, or not all of them.** Look at `mimic_mesh_mirrored_total{backend,result}` of
the caller's sidecar ([Mesh](Mesh#mirroring)); a copy is best effort and each result says why:

- `skipped_body`: the body was larger than `MIMIC_MESH_RETRY_BUFFER` (64 KiB), the shared
  `MIMIC_MESH_RETRY_BUFFER_TOTAL` was in use, the client left in the middle of the upload, or the body had not
  ended at the copy's deadline (a long upload or an open gRPC stream is never mirrored). Raise the buffers (the
  sidecar memory limit with the total) or accept the skips; with `MIMIC_MESH_RETRY_BUFFER=0` no request with a
  body is mirrored.
- `skipped_limit`: 256 copies to that Service port, or 1024 for the sidecar, were already in flight: the
  mirror is slow or silent. The primary is not affected.
- `error`: no copy got a response head: the mirror Service has no ready endpoint, refuses connections, resets,
  or did not answer within 10 s (or the rule's `timeouts`). Over intercepted TLS the mirror pods must present a
  certificate valid for the name the app sent. A copy is never retried.
- no series at all: the mirror was never drawn (`percent: 0`), the request was not routed to a backend (`no-route`,
  `Upgrade`, `CONNECT`, L4, mode `mock`, stale view), or the route is not in the view (`kubectl describe httproute`:
  `RefNotPermitted` for a mirror in another namespace without a `ReferenceGrant`).

The app's own answer never depends on the mirror, and the mirror's answer is discarded: a `500` from the mirror
shows only in the mirror's own logs.

**`503` with `x-mimic-error: connect-failed` after 4 tries.** Every pod tried and the ClusterIP refused the
connection (`mimic_mesh_retries_skipped_total{reason="exhausted"}`). Check the pods' ports and, over
intercepted TLS, their certificates (`tls verification failed` lines).

**`503` with `x-mimic-error: connect-failed` and `x-mimic-origin: inbound; pod=<name>`.** The app of that pod
refused the connection (not listening, crashed, wrong port) and its meshed sidecar said so. Callers' sidecars
retry it on another pod for every method (`mimic_mesh_retries_total{reason="app_refused"}`); you only see it
when every try was refused. On the pod, `mimic_mesh_inbound_refusals_total` rises and the sidecar logs
`inbound connect failed` at warn. Over TLS, or with the mesh off, the connection is closed without an answer.

**`x-mimic-error` or `x-mimic-endpoint` missing from a response of my app.** A meshed sidecar removes every
`x-mimic-*` field from the responses of its app (`mimic_mesh_stripped_headers_total`). With `MIMIC_MESH=off` or
`MimicConfig.spec.mesh.enabled: false` they are kept.

**An app's response vanished: the caller sees a closed connection, `mimic_mesh_unframed_closes_total` rises, warn
line in the sidecar log.** While the fields are removed, a response the sidecar cannot frame closes the
connection. Fix the response, or switch the mesh off for that workload.

## No data

**`GET /v1/workloads/<ns>/<dep>` returns `404`.** The controller holds nothing for that deployment.
1. Is the pod injected? Inbound and outbound exchanges are recorded (see
   [Egress aggregation](Egress-aggregation) for the outbound part).
2. Did the app receive requests that got a response? `404`/`405` on unknown paths are not recorded; HTTPS
   inbound is not decrypted; probes are not recorded.
3. A sidecar pushes only when it recorded exchanges, every 30 s after a random initial delay: wait a
   minute.
4. Sidecar log `report shipping disabled`: `MIMIC_CONTROLLER_URL` is missing.
5. Sidecar log WARN `report push failed` (`status` or `error`): `401`/`403` (token or pod name does not
   belong to the deployment), `429` (rate limit), `503` (Valkey or TokenReview unavailable), or a
   connection error (NetworkPolicy blocking the sidecar API port 8444). It retries with backoff.
6. `mimic_sidecar_api_requests_total{result}` on the controller shows what it received.

**Read API: `403`.** Not a loopback peer, or the `Host` header is not `localhost`/`127.0.0.1`/`[::1]`.
Use `kubectl port-forward` and `localhost`. **Sidecar `/discovery` `403`:** same, use a port-forward.
**`405`:** only `GET`. **Connection refused on 8080:** the read API is inside the controller pod; forward
to the pod or Deployment, not the Service.

**Read API: `503`.** Valkey is unavailable. **`500`** with ERROR `stored report unreadable` (or `stored
meta unreadable`, `stored pods unreadable`): a value in Valkey does not parse. mimic never overwrites
it. Reset that deployment's keys ([Operations](Operations#valkey)).

**Counts look low.** Best effort under load: check `mimic_discovery_exchanges_total{result}` for
`dropped`, `endpoint_limit`, and `mimic_discovery_samples_total{result="budget"}`; WARN
`endpoint limit reached`, `sample budget full`.

**Too many or too few endpoints.** More than 32 literals at one path position are absorbed into a
parameter (INFO `template absorbed`); ids are only recognised when id-like (digits, UUID, ULID, hex
8+, long mixed). See [Discovery and redaction](Discovery-and-redaction#path-templating).

**`kubectl get mimicworkload` shows empty columns.** The status refreshes every 30 s; a deployment
without data has zero counts and no last push. WARN `status not updated` means Valkey is unavailable.
The status requires a CRD with the status subresource (upgrade the chart).

## Store limits

**Pushes get `507`; sidecar WARN `report rejected` with `status` 507.** One report had more endpoints
than `MIMIC_STORE_MAX_PUSH_ENDPOINTS` in a direction (256), or the namespace already has
`MIMIC_STORE_MAX_DEPLOYMENTS` (50) deployments with data. The report is dropped whole.

**Controller WARN `store limit reached` / `store evicted`.** A deployment exceeded
`MIMIC_STORE_MAX_ENDPOINTS` or the sample budget; the busiest endpoints are kept. Check
`mimic_store_refused_total` and `mimic_store_evicted_total`.

**Pushes get `503`; `valkey command failed` (e.g. `OOM`).** Valkey reached `maxmemory` under
`noeviction`. Raise `valkey.resources.limits.memory` or delete data. Sidecars keep merging locally and
retry. `store write kept conflicting` means five optimistic attempts lost; it clears by itself.

**Pushes get `413`.** The body is over `MIMIC_SIDECAR_API_MAX_REPORT`; the sidecar removes the samples and
retries the same sequence.

## TLS

**An HTTPS client fails right after injection with a certificate error.** Its trust store does not
include the mimic root (it pins, ships its own roots or defines its own trust variable). The first
connection fails; later ones pass through (WARN `tls passthrough`, `reason: client_rejected`). To avoid
the failure add the host to `spec.tls.exclude` and restart.

**WARN `tls passthrough` with `upstream_untrusted`.** The destination's certificate is not trusted by
any CA in the pod's images; the app judges it itself. Visibility is lost for it.

**No HTTPS visibility at all; `mimic_tls_connections_total{interception="mode_none"}`.** The pods are in mode
`none` (the default): they record and intercept nothing. Set `spec.mode: discovery` on a `MimicWorkload` of the Deployment;
no restart is needed.

**No HTTPS visibility at all; `mimic_tls_connections_total{interception="no_ca"}`.** No valid
intermediate: look for ERROR `tls ca mismatch` (the seed changed; recreate the pod), ERROR `trust volume
not published`, or WARN `intermediate request failed` (sidecar API unreachable or `503` while the seed
is not loaded).

**mTLS to a provider breaks.** Add the host to `spec.tls.exclude` (the connection is also remembered as
`client_cert_requested` after the first).

## Shutdown

**WARN `report not shipped`.** The shutdown grace ended before the last report was acknowledged. Make
the app exit on `SIGTERM`, and keep `terminationGracePeriodSeconds` above its stop time plus
`MIMIC_SHUTDOWN_GRACE`. **WARN `grace period over`** with `connections: n`: n connections were still
open and were closed.

**Liveness probes of the app fail while the sidecar restarts.** Probes go through the sidecar; an
outage longer than `failureThreshold` x `periodSeconds` restarts the app. Investigate why the sidecar
restarts (`kubectl describe pod`, memory limit `inject.sidecarResources`).

**Logs are missing lines.** `mimic_log_lines_dropped_total` is rising: stdout cannot keep up.
