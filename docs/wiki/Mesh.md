# Mesh

The **mimic mesh** makes the sidecar of a caller pick the pod that serves a call, instead of leaving
the choice to kube-proxy. Today (milestones M1 and M2) it does client-side **load balancing** and **retries** of failures that
happened before the app could process a request; the retry budget, routing, outlier detection and the rest
come in later milestones (see [What is coming](#what-is-coming)).
It works the same in modes `none` and `discovery` (see [Concepts](Concepts)); in `mock` the mock
answers and nothing is sent to a pod. It is on by default and has its own kill switches (below).

## What it does today

The controller builds, for each deployment, a **view**: the Services of its namespace (and the
declared destinations) with their ports and their ready pod IPs. Each sidecar follows its view by
long polling the controller, and keeps the last accepted one in `<MIMIC_TRUST_DIR>/state/mesh.json`.
A call from the app to the ClusterIP and port of a Service in the view is **in scope**; any other call
is **unscoped** and goes to its destination exactly as without the mesh.

For an in-scope call the sidecar waits up to 50 ms for the client's first bytes, then:

| First bytes of the connection | What the sidecar does |
|---|---|
| HTTP/1 request head (not `Upgrade`, not `CONNECT`) or the HTTP/2 preface (`h2c`) | **Per request**: each request goes to a pod, over a pooled connection to `<pod IP>:<target port>`. |
| TLS that mimic intercepts ([TLS interception](TLS-interception)) | Decrypted, balanced per request, then **re-originated over TLS** to the pod. SNI is the one the app sent (none: `<name>.<namespace>.svc`) and the pod's certificate is verified against that name, never the pod IP. Hostname verification cannot be turned off, and a pod is never spoken to in cleartext. |
| Anything else client-first (binary protocols, TLS that is not intercepted) | **L4, per connection**: one pod is chosen, the bytes are relayed untouched. |
| Silence for 50 ms, a first request with `Upgrade` or `CONNECT`, an opaque port, mode `mock` | The plain byte relay to the original destination, as without the mesh. |

**Choosing a pod**: power of two choices, least request. Two distinct endpoints are drawn at random
and the one with the lower `(in_flight + 1) / weight` is taken; `in_flight` counts the requests of
this sidecar to that pod whose response is not complete (an L4 connection counts as one for its whole
life). **Slow start**: a pod that joins a Service port after the sidecar first saw it has weight
`max(0.1, min(1, age / window))`, the window being `policy.loadBalancer.slowStartSeconds` (default
30 s, 0 disables). A pod that leaves and comes back is new again.

**Pools** are per (pod IP, port, protocol, and server name for TLS): at most 1024 HTTP/1 connections
and 16 HTTP/2 connections per pod; idle ones close after `MIMIC_MESH_POOL_IDLE_TIMEOUT`; the pool of a
pod that leaves the view is dropped.

Discovery records one exchange per request under the authority the app sent (never the pod's address).
No header is added to a request or a response. A meshed sidecar does remove the `x-mimic-*` fields of its
**own app's** responses (see [Retries](#retries)).

## Scope

- Every Service of the **own namespace**.
- The Services listed in `spec.destinations` of the winning `MimicConfig` of the deployment (at most
  256 `{namespace, name}`). A destination in another namespace needs a `ReferenceGrant` in **that**
  namespace: `from: {group: mimic.platform.io, kind: MimicConfig, namespace: <yours>}`, `to: {group: "",
  kind: Service}` with `name` absent, empty or the Service's. Without it (or without the
  `ReferenceGrant` CRD) the destination is left out and the `MimicConfig` shows `ResolvedRefs: False`,
  reason `RefNotPermitted`. A namespace that did not consent contributes nothing to the view.
- **Opaque ports** keep the byte relay with no wait: `25, 587, 3306, 4444, 5432, 6379, 9300, 11211,
  22, 21` by default (server-first protocols or never HTTP). Replace the list with
  `MIMIC_MESH_OPAQUE_PORTS`.
- The view is bounded: 1024 Services, 10000 endpoints, 4Mi. A Service that does not fit is skipped
  (the Services of the namespace come first, in name order) and counted in
  `mimic_mesh_view_truncated_total`.

## Failure behaviour

| Situation | Result |
|---|---|
| Connecting to the chosen pod fails | The pod is **avoided** for 5 s and the request is tried again on a pod not tried yet, then on the **ClusterIP** (kube-proxy chooses; counted in `mimic_mesh_fallbacks_total`, access line `upstream` is the ClusterIP), at most `1 + retries` tries in all. See [Retries](#retries). |
| Every try failed to connect | `503` with `x-mimic-error: connect-failed` and `content-length: 0`, after 4 tries. L4: the connection is closed without a byte. |
| The Service port has no ready or serving-terminating endpoint | `503` with `x-mimic-error: no-endpoint`, empty body, answered at once. No ClusterIP fallback (kube-proxy would have no endpoint either). Not recorded by discovery; counted as `mimic_discovery_exchanges_total{result="synthetic"}` and `mimic_mesh_requests_total{reason="no-endpoint"}`. |
| All endpoints terminating (none ready) | The serving, terminating pods keep being used until the rollout ends. |
| **Controller down** | Nothing visible: the last view and mode are kept, and the long-poll retries after 1, 2, 4 … 30 s (jittered, never below 1 s). Balancing, pools and the avoidance of failing pods go on. Becomes a stale view after `MIMIC_MESH_STALE_AFTER`. |
| Intercepted TLS: the pod's certificate does not verify | A connect failure on that pod (same steps), then the ClusterIP verified the same way; the warn line `tls verification failed` has `upstream` and `tls_error`. |
| **Stale view** (no contact with the controller for `MIMIC_MESH_STALE_AFTER`) | Pod addresses are not used: new connections keep the byte relay and each request of an already routed connection goes to the ClusterIP (counted as a fallback). A dead controller never turns pod churn into errors. One WARN `mesh view stale` per streak; no pod IP older than the bound is used; back to the pods at the next applied view. |
| Rolling restart of the callee | Endpoints leave the view within about a second; the e2e `msh_87` saw 0 errors in 392 calls during a rollout and none after it ended (the baseline for the retries of M2). |
| A pooled connection the pod had closed before the request was written | Sent once more on a new connection to the same pod (`mimic_mesh_reconnects_total`), whatever the method; this is part of the same try, not a retry. When that new connection cannot be made either, the try is a connect failure and [Retries](#retries) goes on. A request that was written is only sent again as the table of [Retries](#retries) says. |

Kill switches: the chart value `mesh.enabled: false` (the cluster), `MimicConfig.spec.mesh.enabled:
false` (one workload). Then connections keep the byte relay; a connection already routed sends each
request to the ClusterIP.

## Configuration

**Helm** (`mesh.*`, chart `deploy/helm/mimic`):

| Value | Default | Env variable | Notes |
|---|---|---|---|
| `mesh.enabled` | `true` | `MIMIC_MESH` (`on`/`off`) | `false`: the controller serves no view (`404`), watches nothing and asks for no Service, EndpointSlice, Deployment, `MimicConfig` or `ReferenceGrant`; sidecars keep the plain relay. |
| `mesh.clusterName` | `local` | `MIMIC_CLUSTER_NAME` | Name of the cluster in the view. |
| `mesh.watchTimeout` | `30s` | `MIMIC_MESH_WATCH_TIMEOUT` | A held view request ends `304` after this; 100ms to 10m. |
| `mesh.debounce` | `100ms` | `MIMIC_MESH_DEBOUNCE` | Changes coalesced, doubled up to 1s while they keep arriving; 10ms to 1s. |
| `mesh.viewCacheBytes` | `64Mi` | `MIMIC_MESH_VIEW_CACHE_BYTES` | Cache of built views per replica; at least `maxViewBytes`, at most 4Gi. |
| `mesh.maxServices` | `1024` | `MIMIC_MESH_MAX_SERVICES` | 1 to 100000. |
| `mesh.maxEndpoints` | `10000` | `MIMIC_MESH_MAX_ENDPOINTS` | 1 to 1000000. |
| `mesh.maxViewBytes` | `4Mi` | `MIMIC_MESH_MAX_VIEW_BYTES` | 64Ki to 256Mi. |
| `mesh.maxWatches` | `10000` | `MIMIC_SIDECAR_API_MAX_WATCHES` | Held view requests per replica; over it `503` with `Retry-After`. |
| `mesh.staleAfter` | `60s` | `MIMIC_MESH_STALE_AFTER` | 1s to 1h. Handed to the injected sidecars. |
| `mesh.startupWait` | `2s` | `MIMIC_MESH_STARTUP_WAIT` | A sidecar without a view is ready after this long anyway; 0s to 1m. Handed to the sidecars. |

Sidecar-only variables (the webhook does not set them):

| Variable | Default | Meaning |
|---|---|---|
| `MIMIC_MESH_CONNECT_TIMEOUT` | `2s` | Time allowed to connect to a pod; 100ms to 10s. The original destination keeps the relay's 10 s. |
| `MIMIC_MESH_POOL_IDLE_TIMEOUT` | `90s` | Idle pooled connection to a pod is closed; 1s to 1h. |
| `MIMIC_MESH_RETRY_BUFFER` | `64Ki` | Request body bytes held per request so that it can be retried; `<n>`, `<n>Ki`, `<n>Mi`, 0 to 1Mi (`0`: a request with a body is retried only after a connect failure). |
| `MIMIC_MESH_RETRY_BUFFER_TOTAL` | `8Mi` | Request body bytes held for replay by all requests of the sidecar together; 1Mi to 64Mi. Sized for the 128 Mi sidecar limit: raising it needs a larger `MIMIC_INJECT_SIDECAR_RESOURCES` memory limit. |
| `MIMIC_MESH_OPAQUE_PORTS` | `25,587,3306,4444,5432,6379,9300,11211,22,21` | Comma-separated, at most 256 ports, or `none`. |
| `MIMIC_METRICS_MAX_ENDPOINTS` | `256` | Pods with a metrics label of their own; the rest are `other`; 1 to 10000. |

An invalid value exits the process with code 2 (see [Configuration](Configuration)). The avoidance
time of a failed pod (5 s), the number of retries (3) and the delay after a refusal (25 ms plus or minus 50 %)
are not configurable (no `MIMIC_*` variable sets them; the retries become a `MimicConfig` field with the retry
budget, #446).

**`MimicConfig`** (`mimic.platform.io/v1alpha1`, namespaced; see [Configuration](Configuration) for
the other CRDs) attaches to Deployments or Services of its own namespace with `spec.targetRefs`
(1 to 16):

```yaml
apiVersion: mimic.platform.io/v1alpha1
kind: MimicConfig
metadata: {name: cart, namespace: shop}
spec:
  targetRefs:
    - {group: apps, kind: Deployment, name: cart}
    - {group: "", kind: Service, name: cart}
  destinations:              # Deployment target: other Services this workload calls
    - {namespace: risk, name: scoring}
  mesh:
    enabled: true            # Deployment target: false = byte relay for this workload
  loadBalancer:
    slowStart: 30s           # Service target: <n>s, 0 to 3600 s, 0s disables
```

Several configs for the same target: the oldest (`creationTimestamp`, then name) wins as a whole, the
others get `Accepted: False`, reason `Conflicted`. Other `Accepted: False` reasons: `TargetNotFound`,
`Invalid`. Without any `MimicConfig`: slow start 30 s, mesh on, no declared destinations.

## Retries

A mesh-routed request is tried again **only when its failure happened before the app could process it**.
A response of an app, any status (`5xx` included), is never retried. The classification is the pure
function `mesh::retry` of `mimic-core`.

| Failure of one try | Retried | `result` of the try, `reason` of the retry |
|---|---|---|
| Connect refused, connect timeout, failed TLS handshake or verification, a ninth TLS pool of a pod: nothing was written | every method | `connect_failed` |
| HTTP/2 `RST_STREAM` with `REFUSED_STREAM`, or a `GOAWAY` whose last-stream-id is 0 (no stream was processed; any other `GOAWAY` follows the next row) | every method | `refused_stream` |
| A **trusted refusal**: status `503`, exactly one `x-mimic-error` field, `overloaded` or `queue-timeout`, and exactly one `x-mimic-endpoint` field whose value parses as `<ipv4>:<port>` or `[<ipv6>]:<port>` and equals the pod address of this try | every method | `overloaded`, `queue_timeout` |
| A **trusted app refusal**: status `503`, exactly one `x-mimic-error: connect-failed`, exactly one `x-mimic-origin` (`inbound` or `inbound; pod=<name>`) and exactly one `x-mimic-endpoint` equal to the pod address of this try. The callee's sidecar writes it when its app refuses the connection (cleartext HTTP/1 and `h2c` only) | every method | `app_refused` |
| The connection closed or reset, or the stream reset with another code, after the request was written (in part) and before any byte of a response head | `GET`, `HEAD`, `OPTIONS`, `TRACE`, `PUT`, `DELETE` only (method tokens compared exactly) | `reset` |
| Any other response (a `503` that is not a trusted refusal, an `x-mimic-error` of another value, any other status), any failure after the response head | never | `ok` (a response head came back) |

**Bound per tier.** At most `1 + retries` tries per request, `retries` being 3 unless a `MimicConfig` says
otherwise: at most **4 tries** by default. A
reconnect of an unwritten request is part of its try. Each retry goes to a pod not tried yet, chosen as in
the load balancing (avoidance applies); when every endpoint was tried, to any of them. When every try so
far failed to connect and either no untried pod is left or the next try is the last one, that try goes to
the **original destination** (the ClusterIP) instead, at most once per request, and it ends the tries
(counted in `mimic_mesh_fallbacks_total`). A Service with one endpoint is tried on it, then on the
ClusterIP; a request never reaches more than 4 destinations. A try on the original destination (fallback,
stale view, a view that turned `"mesh": false`) is never retried. On an L4 connection only connect failures
are retried (at most 4 connects; with every try failed the connection is closed without a byte): once a
byte was sent to a pod, a close or reset is relayed.

**Retry budget.** Retries are also bounded over time, so that a failing Service is not hit by more retries
than requests. Each caller pod (its sidecar) keeps one budget per backend Service, shared by all the ports of
the Service. Over the last `window` (default 10 s) it counts the requests (every first try of a mesh-routed
request, every mesh-balanced L4 connection) and the retries it granted. A retry is granted when

```
retries in the window + 1  <=  percent / 100 * requests in the window  +  minPerSecond * window seconds
```

With the defaults (`percent` 20, `minPerSecond` 10, `window` 10 s) the second term is 100 retries plus 20 % of
the requests of the window: a pod that sent 500 requests in the last 10 s may retry up to 200 of them
(100 + 0.2 x 500), a nearly idle one up to 100. A retry over budget is **not made**: the failure is returned
as the "when no try is left" rules below say, `mimic_mesh_retry_budget_exhausted_total{backend}` counts it and
the sidecar logs a warning `retry budget exhausted` (at most once per second per backend). The window slides in
tenths, so a count stays between 0.9 and 1 window. `retries: 0` gives one try per request, with no retry and
no fallback to the ClusterIP. Both settings are per Service, in a `MimicConfig` targeting it (each field
that is absent keeps its default):

```yaml
apiVersion: mimic.platform.io/v1alpha1
kind: MimicConfig
metadata: {name: cart, namespace: shop}
spec:
  targetRefs: [{group: "", kind: Service, name: cart}]
  retries: 2                # 0 to 10, default 3
  retryBudget:
    percent: 10             # 0 to 100, default 20
    minPerSecond: 5         # 0 to 1000, default 10
    window: 10s             # 1s to 60s, default 10s
```

A change reaches the callers with the next view (within the sidecar's pull interval); a changed `window`
starts the Service's budget empty.

**Delays.** Connect failures, refused streams, app refusals and resets are retried at once; a trusted refusal after a
delay drawn between 12.5 and 37.5 ms. The body of a refused try is read and discarded up to 64 KiB (at most
2 s) before the delay.

**The binding.** A refusal is trusted only on a **cleartext pod try** whose `x-mimic-endpoint` **names that
pod**:

- A try on the original destination has no pod address: none of its answers is trusted.
- **Never over TLS.** On a try over intercepted TLS no answer is a trusted refusal: the callee's sidecar
  cannot remove `x-mimic-*` from what it does not decrypt, so the fields cannot be bound to it. A `503`
  with them is an answer of the app, passed as received and never retried; connect failures (a failed
  verification included) are retried as on cleartext.
- An answer naming another pod, the ClusterIP, or carrying two `x-mimic-error` or two `x-mimic-endpoint`
  fields is passed as received and not retried.
- A trusted refusal is judged on the complete final response head only.

The refusal is the signal of the callee's own sidecar, which writes the `<pod ip>:<port>` it was reached on.
No sidecar generates one before M5, so in M2 only a peer outside the mesh sets it. A meshed inbound sidecar
removes every `x-mimic-*` field of its app's responses, and the binding covers peers that do not: an app that
forwards a downstream refusal names the downstream pod, never the one its caller sent the try to. A refusal
therefore never makes the callers of a forwarding app retry: each tier retries only its own tries, so the
total is at most 4 tries per tier.

**Inbound stripping.** With `MIMIC_MESH=on`, unless the view says `"mesh": false` (this also holds before the
first view), the inbound side removes every field whose name starts with `x-mimic-` (any case) from the final
response heads of its app, so that only a sidecar ever writes them. HTTP/1: the relay holds each final head
until it is whole (at most 64 KiB; `1xx` heads are not held) and writes it without those lines, every other
byte unchanged; the body and the framing are untouched. HTTP/2: the fields are removed from the response
headers; trailers are not touched. Each field removed counts in `mimic_mesh_stripped_headers_total`. With
`MIMIC_MESH=off` or a view of `"mesh": false` the relay is byte-exact. While the fields are removed, a
response the sidecar cannot frame (a bare CR, a header line without a colon or with a name that is not a
token, a head over 64 KiB, an invalid body framing) is not relayed: **both sockets are closed**, so no
later response of the connection can carry a field that was not removed. The caller sees a close or reset,
which its sidecar takes as an app failure, never as a refusal. It is counted in
`mimic_mesh_unframed_closes_total` and logged once per connection at `warn`, without any byte of the app.
Inbound TLS is relayed unchanged.

**Request bodies and the replay buffer.** A request is retried only while every byte of its body read from
the app is still held for replay: at most `MIMIC_MESH_RETRY_BUFFER` (64 KiB) per request and
`MIMIC_MESH_RETRY_BUFFER_TOTAL` (8 MiB) for all the requests of the sidecar together
(`mimic_mesh_retry_buffer_bytes`). A request whose body grows past its share, or that finds the total in
use, is streamed: it is still retried after a connect failure (nothing of it was written), never once a byte
of it was written (`mimic_mesh_retries_skipped_total{reason="buffer"}`). The bytes held are released when a
try gets its response head or the request can no longer be retried. A retry sends the same head and the same
body bytes.

**When no try is left**: connect failures give `503` `x-mimic-error: connect-failed`; a trusted refusal is
passed to the app **as received**, its `x-mimic-error` and `x-mimic-endpoint` fields included; a reset is
propagated as in M1 (HTTP/1: the app's connection closed without an answer; HTTP/2: the app's stream
reset); a refused stream resets the app's stream with `REFUSED_STREAM`.

**Never retried**: requests answered by the mock (mode `mock`), the `no-endpoint` answer, an `Upgrade` or
`CONNECT` once its head was written (its connect is retried like any), an L4 connection once a byte was
sent, and anything on the byte relay.

One access line is written per request, with `upstream` the address of the last try and `attempts` the number
of tries. Discovery records the final exchange once; a trusted refusal passed to the app is not recorded
(`mimic_discovery_exchanges_total{result="synthetic"}`).

## Routes

With the Gateway API CRDs installed ([Install](Install#gateway-api-optional)), the controller reads every
`HTTPRoute` (`gateway.networking.k8s.io/v1`) whose `parentRefs` name a **Service** (`group: ""`, `kind: Service`;
the GAMMA convention), compiles it and puts it in the views of the callers it applies to. Routes with Gateway
parents are ignored. **The sidecar reads and validates routes but applies none of them yet**: matching,
weights and header modifiers are #448, timeouts #449, mirrors #450. Today routes give you the status below and
a validated view.

**Supported**, per rule: `matches` with `path` (`Exact`, `PathPrefix`), `headers` and `queryParams` of type
`Exact`, `method`; `backendRefs` to Services with `port` and `weight`; the filters `RequestHeaderModifier`,
`ResponseHeaderModifier` and `RequestMirror`; `timeouts.request` and `timeouts.backendRequest`; `name`. `retry`
is ignored (retries are set by `MimicConfig`, [Retries](#retries)); `hostnames` is ignored.

**Refused**: a route is not programmed, and is in no view, when it has a `RegularExpression` match, a filter
`URLRewrite`, `RequestRedirect`, `CORS` or `ExtensionRef`, `backendRefs[].filters`, a field the controller does
not know (experimental channel), a modifier naming `host`, `Content-Length`, `Transfer-Encoding`,
`Connection`, `Keep-Alive`, `Proxy-Connection`, `TE`, `Trailer`, `Upgrade` or any `x-mimic-*` field, or a
`spec` over 64 KiB. Its status says which.

**Producer and consumer routes.** A route in the namespace of its Service is a *producer* route: every caller
that has the Service in its view gets it. A route in another namespace than the Service is a *consumer* route
and applies to the callers of **its own namespace only**; for them it replaces the producer routes of that
Service. A route of a third namespace applies to nobody else.

**Backends in another namespace** need a `ReferenceGrant` in the backend's namespace, `from` `HTTPRoute`:

```yaml
apiVersion: gateway.networking.k8s.io/v1beta1
kind: ReferenceGrant
metadata: {name: allow-routes, namespace: backends}
spec:
  from: [{group: gateway.networking.k8s.io, kind: HTTPRoute, namespace: shop}]
  to: [{group: "", kind: Service}]
```

A grant for `MimicConfig` does not allow a route, and the other way round. Without the grant, or after it is
deleted, the backend is not in the view and the route's `ResolvedRefs` is `False`, reason `RefNotPermitted`.

**Reading the status.** For each Service parent that exists the controller writes one entry in
`status.parents[]` with `controllerName: mimic.platform.io/mesh` and two conditions:

```bash
kubectl -n shop get httproute cart-canary -o jsonpath='{range .status.parents[?(@.controllerName=="mimic.platform.io/mesh")].conditions[*]}{.type}={.status} {.reason} {.message}{"\n"}{end}'
```

| Condition | Reason | Meaning |
|---|---|---|
| `Accepted` `True` | `Accepted` | Programmed. The message says `retry is configured by MimicConfig` when the route has `retry`. |
| `Accepted` `False` | `UnsupportedValue` | Refused; the message is `unsupported: <field path>`, the first such field (`rules[0].filters[0].type`). |
| `Accepted` `False` | `NoMatchingParent` | The entry's `port` is not a port of the Service, or it has a `sectionName`. |
| `ResolvedRefs` `False` | `RefNotPermitted`, `InvalidKind`, `BackendNotFound` | A backend or mirror lacks a `ReferenceGrant`, is not a core Service, or does not exist (or lacks the port). The route stays programmed; that backend's share is answered `500` once the sidecar applies routes. |

Entries of other controllers in the same list are never changed. The status of a route whose Service parent
does not exist stays empty, and entries mimic wrote stay after `helm uninstall`.

> **Warning: route values reach other pods.** Header and query values of a route are copied as written into
> the views of every pod it applies to (producer routes: every namespace that has the Service in its view) and
> into their state files. An `HTTPRoute` is configuration, not a Secret: **do not put a credential in a
> `RequestHeaderModifier`.** See [Security](Security#mesh).

Older Gateway API CRDs than 1.2 make the API server prune `timeouts` before mimic reads a route; with the
experimental channel a route with a field mimic does not know is `UnsupportedValue`, never half-applied.

## Observability

Sidecar metrics (only with `MIMIC_MESH=on`; `service` is `<namespace>/<name>`, `backend` is
`<namespace>/<name>:<port>`):

| Metric | Labels |
|---|---|
| `mimic_mesh_requests_total` (per mesh-routed request) | `service`, `route` (`none` for now), `backend`, `status_class` (`1xx` to `5xx`, `none`), `reason` (`none`, the `x-mimic-error` of an answer of the sidecar, or that of a trusted refusal passed to the app) |
| `mimic_mesh_request_duration_seconds` (until the response head) | `service`, `backend`, `status_class` |
| `mimic_mesh_endpoint_requests_total` | `backend`, `endpoint` (`ip:port` or `other`), `status_class` |
| `mimic_mesh_endpoint_in_flight` | `backend`, `endpoint` |
| `mimic_mesh_reconnects_total`, `mimic_mesh_fallbacks_total` | `backend` |
| `mimic_mesh_attempts_total` (every try, to a pod or to the original destination; a reconnect is not a try) | `backend`, `result` (`ok`, `connect_failed`, `refused_stream`, `reset`, `app_refused`, `overloaded`, `queue_timeout`) |
| `mimic_mesh_retries_total` (tries after the first) | `backend`, `reason` (`connect_failed`, `refused_stream`, `reset`, `app_refused`, `overloaded`, `queue_timeout`) |
| `mimic_mesh_retries_skipped_total` (a retried kind of failure that was not retried) | `backend`, `reason` (`exhausted`: no try left; `method`: a `reset` of a method that is not idempotent; `buffer`: the body was not held) |
| `mimic_mesh_retry_buffer_bytes` (gauge, at most `MIMIC_MESH_RETRY_BUFFER_TOTAL`) | none |
| `mimic_mesh_stripped_headers_total`, `mimic_mesh_unframed_closes_total`, `mimic_mesh_inbound_refusals_total` (one per inbound connection answered `503` `connect-failed` because the app refused it) | none |
| `mimic_mesh_unscoped_connections_total` | none |
| `mimic_mesh_view_pulls_total` | `result` (`ok`, `not_modified`, `older`, `invalid`, `error`) |
| `mimic_mesh_view_age_seconds`, `mimic_mesh_view_services`, `mimic_mesh_view_endpoints` | none |

Series of a pod that left the view are removed 5 minutes later. No label carries a path, header or
other caller-controlled string.

Controller metrics: `mimic_mesh_objects` (`kind`, with `httproute`), `mimic_mesh_gateway_api_present`, `mimic_mesh_routes` (`state`), `mimic_mesh_route_status_total` (`result`), `mimic_mesh_view_builds_total` (`result`),
`mimic_mesh_view_build_duration_seconds`, `mimic_mesh_watches`, `mimic_mesh_view_truncated_total`,
`mimic_mesh_view_cache_bytes`. See [Monitoring](Monitoring).

`mimic_mesh_requests_total` and the duration stay one per request (the duration spans every try);
`mimic_mesh_endpoint_requests_total` counts each try written to the pod. Access lines of mesh-routed
requests and L4-balanced connections gain `backend`, `upstream` (the pod `ip:port` of the last try; the
ClusterIP after a fallback) and `attempts` (the number of tries, 1 without a retry; for L4 the number of
connects tried). `dst` stays the original destination. Lines of relayed
connections have neither.

Warnings to know: `mesh view stale` (once per stale period), `mesh view rejected` (a view that does not
parse, once per streak), `tls verification failed`.

## Limits

- Mesh-routed HTTP/1 goes through `hyper`, so it is not byte-exact: header order across different
  names is not kept (case and the order of fields of one name are), chunk extensions are dropped,
  `Connection`, `Keep-Alive`, `Proxy-Connection` and `TE` are consumed per hop, pipelined requests are
  serialised, a response with neither length nor `chunked` reaches the app `chunked`.
- A request head the sidecar cannot frame (bad tokens, bare `CR`, `obs-fold`, conflicting
  `Content-Length`, a `Transfer-Encoding` not ending in `chunked`, over 64 KiB or 256 lines) is
  answered `400` `x-mimic-error: invalid-request` and the connection closes; the byte relay would have
  forwarded it.
- The **first request** of a connection with `Upgrade` or `CONNECT` keeps the byte relay for the whole
  connection. A later one leaves `hyper`: a pod is chosen, the head is written as received and the
  connection becomes a byte relay to that pod. An HTTP/2 `CONNECT` is answered `501` `unsupported`.
- A client silent for 50 ms after connecting, or whose first request head is not whole within the first
  16 KiB, stays on the byte relay for the life of that connection: per-request balancing holds for
  clients that send at once.
- Intercepted TLS: the app is answered `http/1.1` when it offered it, else `h2`, and the pod is offered
  only that protocol. A pod that **requests a client certificate** fails the handshake (`503`): exclude
  that destination in [TLS interception](TLS-interception).
- Answers of the sidecar itself (`503 no-endpoint`, `503 connect-failed`, `502 invalid-response`,
  `400`, `501`) are not recorded by discovery.
- Balancing is per caller sidecar: each one counts only its own in-flight requests.
- **A pod can make its callers retry requests sent to itself.** An app whose own sidecar does not remove
  `x-mimic-*` (no sidecar, mesh switched off, inbound TLS) can answer a forged `503` refusal bound to its own
  address: its callers' sidecars then send that request to its sibling pods, at most 3 more times, and only
  requests that were sent to that pod. It cannot make a caller retry a request sent to another pod, nor make
  the callers of a pod that forwards its answers retry.
- **Idempotent by method name.** A `GET`, `HEAD`, `OPTIONS`, `TRACE`, `PUT` or `DELETE` that a pod read and
  then dropped without an answer is sent to another pod: an app whose such methods have side effects can see
  one request twice. Until the retry budget (#446) nothing bounds this per caller but `retries`, so a reset
  that every tier propagates as a reset can cost 4 tries per tier; refusals and connect failures never reach
  an app twice, at any depth.
- **An injected callee whose app refuses connections over TLS looks like a reset.** On cleartext HTTP/1 and
  `h2c` its meshed sidecar answers `503` `x-mimic-error: connect-failed` with `x-mimic-endpoint` (its pod
  address) and `x-mimic-origin: inbound; pod=<name>`, and every method is retried on another pod. Over inbound
  TLS, which it does not decrypt, it closes the connection without a byte: only the idempotent methods
  above are retried, a `POST` gets the close. With the mesh off nothing is answered.
- **No timeout yet.** A try that gets neither an answer nor a close waits as long as the app's own connection
  does; per-try and per-request timeouts come with #449.
- **Inbound heads are held when the mesh is on.** The inbound HTTP/1 relay writes a final response head once
  it is whole (at most 64 KiB) instead of as it is read: timing only, no byte changes but the `x-mimic-*`
  lines.
- **A response that cannot be framed closes the inbound connection when the mesh is on** (see
  [Retries](#retries)). Such an app works with the mesh off or a view of `"mesh": false`. A request that
  cannot be framed (client side) is still relayed unchanged, and so is what the app answers to it.
- Not yet: the retry budget, routing by path or header, outlier ejection, circuit breaking, admission control.

## What is coming

- **M2**: retries of proxy-level failures on another pod are in (above); the retry budget and
  `MimicConfig.retries` are next (#446), then timeouts (#449).
- **M3**: Gateway API `HTTPRoute` (routing, canary, timeouts, mirror): the controller reads, validates and reports routes ([Routes](#routes)); the sidecar applies them from #448.
- **M4**: outlier detection, circuit breaker and fault injection.
- **M5**: admission contract, failure signalling and HPA.
- **M6**: north-south gateway.
