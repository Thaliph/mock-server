# Mimic mesh — design

> Status: **design, 2026-11-12, revised after the review of PR #469** (B1–B4, N1–N5). Audience: mimic engineers and the product owner. This
> document explains *what* we build next and *why*; the observable contract of each step is
> written into `docs/SPEC.md` (§3 to §11 and the acceptance criteria of §12) by the issue that
> delivers it, and the spec stays the source of truth. The product decisions recorded on
> 2026-11-12 are final and are not discussed here; where this document makes a technical
> choice, it says why the alternatives lost.

## Contents

1. Goals and non-goals
2. Modes: `none`, `discovery`, `mock`
3. Architecture of the mimic mesh
4. Service discovery and the scoped view
5. Gateway API usage
6. The `MimicConfig` CRD
7. Retries
8. Ingress behaviour
9. Admission control
10. Failure signalling, metrics and logs
11. Autoscaling
12. Interaction with mock mode and load tests
13. Failure modes
14. Performance budgets
15. Security notes
16. Multi-cluster, kept in mind
17. Parallel tracks: gRPC, explorer UI, benchmarks, production values, small items
18. Milestones and dependency graph

---

## 1. Goals and non-goals

**Goals.** Turn the sidecar that mimic already injects into a small, opinionated *mimic mesh*
for HTTP traffic between workloads, without a second proxy and without a change to the app:

- caller-side routing and client-side load balancing over pod IPs, with per-pod connection
  pools, power-of-two-choices (P2C) least-request and slow start;
- Service-level canary (weighted `backendRefs`), mirroring and timeouts from the Gateway API
  `HTTPRoute` (GAMMA: `parentRefs` a Service);
- retries of proxy-level failures, outlier detection and a circuit breaker, fault injection,
  declared in mimic's own `MimicConfig` CRD;
- an admission contract per workload (concurrency **and** requests per second) enforced by the
  callee's sidecar, with explicit `503` signalling, and a metric an HPA can scale on;
- a north-south gateway (`GatewayClass` `mimic`) that reuses the same client;
- a new default mode `none`: a plain proxy that records nothing;
- kill switches: the whole mesh (chart `mesh.enabled`) and one workload
  (`MimicConfig.spec.mesh.enabled: false`) fall back to today's byte-exact relay (§3.4).

**Non-goals** (v2 of the mesh or never): mTLS, workload identity and authorization policies;
multi-cluster (only a `cluster` field is kept in mind, §16); Gateway API experimental channel;
`GRPCRoute`, `TLSRoute`, `TCPRoute`, `UDPRoute`; locality/zone-aware routing; rate limiting
across pods (the contract is per pod, §9); app-status retries (`5xx`); traces (logs and metrics
only, as everywhere in mimic); a general-purpose service mesh feature set.

## 2. Modes: `none`, `discovery`, `mock`

| Mode | Inbound | Outbound | TLS | Pushes to the controller |
|---|---|---|---|---|
| `none` (**new default**) | forwarded, not recorded | forwarded (mesh-routed when in scope), not recorded | passed through, never intercepted (L4 balanced, §3.5) | none |
| `discovery` | forwarded, recorded | forwarded (mesh-routed when in scope), recorded | §5 of the spec, unchanged | reports, as today |
| `mock` | forwarded, recorded | HTTP answered from recordings (§8 of the spec, unchanged) | unchanged | reports, as today |

- `MimicWorkload.spec.mode` gains `none` and its CRD default becomes `none`. Objects created
  before the change keep the `discovery` the API server wrote at their creation (defaults are
  persisted on write), so an upgrade does not silently stop recording. A deployment injected by
  label without any `MimicWorkload` is in `none` (it was `discovery`): an upgrade note in the
  changelog says so.
- Effective mode across several `MimicWorkload`s: `mock` > `discovery` > `none` (pure,
  `mimic-core` `mock::effective`, extended).
- In `none` the sidecar still installs trust (the webhook is unchanged), still pulls its mode
  and, from M1, its mesh view, so that switching to `discovery` needs no restart. It does not
  intercept TLS (nothing would read the plaintext), does not run the recorder, holds no
  samples and ships nothing. The mesh (§3) is independent of the mode: it works the same in
  `none` and `discovery`; in `mock` the mocked calls bypass it (§12). `none` means "proxy
  only: nothing recorded", as the owner defined it; it does **not** mean "no mesh". The mesh is
  on by default (owner decision: mimic is a mesh) and has its own kill switches (§3.4).
- Why `none` passes TLS through: interception costs CPU, a handshake and a trust dependency for
  no benefit when nothing is recorded. The alternative (intercept in every mode to route
  HTTPS per request) was rejected: in-cluster mesh traffic is cleartext in the vast majority of
  cases, and HTTPS to an in-scope Service is still load balanced per connection (§3.3).

## 3. Architecture of the mimic mesh

```
 ┌─────────── caller pod ───────────┐            ┌──────────── callee pod ────────────┐
 │ app ─▶ iptables ─▶ mimic-sidecar │── pod IP ─▶│ iptables ─▶ mimic-sidecar ─▶ app    │
 │        (outbound 15001)          │            │ (inbound 15006): admission (§9)     │
 │   route (HTTPRoute) ─▶ backend   │            │  no re-route, app 503 passed through │
 │   Service ─▶ P2C over pod IPs    │            └─────────────────────────────────────┘
 │   pools · retries · breaker      │
 └──────────────▲───────────────────┘
                │ long-poll GET /v1/workloads/<dep>/mesh (sidecar API, HTTPS, token)
      ┌─────────┴───────────┐  watch  ┌──────────────────────────────────────────────┐
      │  mimic-controller   │◀────────│ Services · EndpointSlices · HTTPRoutes ·      │
      │  view = f(objects)  │         │ ReferenceGrants · MimicConfigs · Gateways     │
      └─────────────────────┘         └──────────────────────────────────────────────┘
```

### 3.1 Where routing happens

Routing and load balancing happen **in the caller's sidecar** (like Linkerd): the outbound
listener knows the original destination (`SO_ORIGINAL_DST`), which for a call to a Service is
its ClusterIP and port (kube-proxy rules live on the node, not in the pod's network namespace,
so the sidecar sees the ClusterIP before any DNAT). The sidecar looks the pair up in its view:

1. **Not in the view** (outside the scope, an external host, a pod IP, a headless Service), or
   the mesh is switched off (§3.4): forwarded to the original destination exactly as today,
   byte-exact for HTTP/1. No route, retry, breaker or fault applies. Counted as `mesh="false"`
   (§10).
2. **In the view, HTTP**: cleartext HTTP, or TLS that mimic **already intercepts** under the
   existing rules (`discovery`, `mock` passthrough; spec §5) and re-originates as specified in
   §3.5. The request is parsed by `hyper`, matched against the HTTPRoute rules attached to the
   Service (§5), a backend Service is drawn by weight, a pod of that backend is chosen by P2C,
   the request goes over that pod's pool, with the MimicConfig policies of the backend
   (timeouts, retries, breaker, faults).
3. **In the view, not HTTP**: opaque TCP, TLS that mimic does not intercept (always in `none`;
   §3.5), and an HTTP/1 connection from a request that carries `Upgrade` (websocket, `h2c`)
   or method `CONNECT` on (§3.4). **Connection-level** (L4) balancing: one pod is chosen by
   P2C per connection, then the bytes are relayed untouched; a connect failure is retried on
   another pod (§7).

The callee's sidecar never re-routes (§8).

**Technical choice: HTTP/1 is re-framed for mesh-routed calls.** Per-request balancing, routing,
retries and pools are impossible with the per-connection byte relay of the spec (§4.1), so an
in-scope HTTP/1 call goes through `hyper`'s HTTP/1 client (header case preserved with
`preserve_header_case`, no header added). Every behaviour change this brings is enumerated in
§3.4 and becomes an accepted limit of the spec; everything not in the view, and everything
when the mesh is switched off, keeps the byte relay. Alternatives rejected: per-connection balancing only (no canary per request, no retry,
poor spread with keep-alive clients); writing our own pooled HTTP/1 client on the codec (more
code and risk than `hyper`, which we already ship).

### 3.2 Client-side load balancing

Pure logic in `mimic-core` (`mesh::lb`), clock and random numbers passed in:

- **Endpoints**: the ready addresses of the backend's EndpointSlices for the Service port
  (`ready: true`; `serving && terminating` only when no endpoint is ready, as kube-proxy does).
- **P2C least-request**: draw two distinct endpoints at random, take the lower
  `(in_flight + 1) / weight` (ties: the first draw). With one endpoint, take it. Outlier-ejected
  endpoints are excluded (§6.4).
- **Slow start**: an endpoint first seen ready at `t0` has weight
  `max(0.1, min(1, (now − t0) / window))`, `window` 30 s by default (`MimicConfig`
  `loadBalancer.slowStart`, `0s` disables). The sidecar's own start does not count as "first
  seen": at the first view every endpoint has full weight, so a restarted caller does not starve
  a stable backend.
- **Pools** (sidecar, `mesh::pool`): one pool per (pod IP, port, protocol). HTTP/2 (the app
  spoke h2c or gRPC): one multiplexed connection, a second one when the peer's
  `MAX_CONCURRENT_STREAMS` is reached. HTTP/1: up to `circuitBreaker.maxConnections` (default
  1024) connections per pod, idle ones closed after `MIMIC_MESH_POOL_IDLE_TIMEOUT` (90 s). The
  upstream protocol is the one the app spoke (no h1→h2 upgrade). Endpoints that leave the view
  have their pool drained: in-flight requests finish, idle connections close.
- Why P2C least-request: it is what Linkerd and Envoy use by default, needs only local state,
  and behaves well under uneven latencies; round robin and random lost (no load feedback),
  EWMA latency (Linkerd's "peak EWMA") lost because it needs tuning and gives little more for
  our traffic levels.

### 3.4 Kill switches and the accepted limits of HTTP/1 re-framing

**Kill switches** (review B3; both give back today's byte-exact relay; the switch takes effect
at the next view, without restart; connections already open finish on the path they started
on):

- **Cluster**: chart `mesh.enabled` (default `true`) sets `MIMIC_MESH` (`on`|`off`) on the
  controller and, through the webhook, on every sidecar. `off`: the controller serves no mesh
  view (`404`), sidecars keep pulling the mode through the egress pull as before M1, and no
  connection is mesh-routed.
- **Workload**: `MimicConfig.spec.mesh.enabled: false` on a Deployment target. The view of that
  deployment carries `"mesh": false`: its sidecar routes nothing (outbound byte relay to the
  original destination), strips nothing and enforces no admission contract (inbound byte relay,
  §8, §9); an `admission` block on the same object is reported `Accepted: False`, reason
  `Invalid`. Its pods are still balanced to by meshed callers (the caller decides); the
  `x-mimic-endpoint` binding of §7 keeps their responses from being trusted as signals.

**Accepted limits of mesh-routed HTTP/1** (spec §4.1 gains this list; each item has a
differential test that sends the same corpus through the byte relay and the mesh path and
asserts exactly the stated difference, and the head reader has a fuzz target):

1. Requests that `httparse`/`hyper` reject are answered `400` with `x-mimic-error:
   invalid-request` by the caller's sidecar, where the byte relay would have forwarded them:
   header names that are not tokens, bare `LF` line endings, `obs-fold` continuation lines, a
   `NUL` or `CR` in a value, duplicate or conflicting `Content-Length`, `Content-Length`
   together with `Transfer-Encoding`, a `Transfer-Encoding` whose last coding is not `chunked`.
2. Header **order across different names** is not kept (`HeaderMap` groups by name); the order
   of repeated fields of one name and the case of names are kept.
3. Chunk extensions are dropped; chunked stays chunked and a `Content-Length` body stays
   `Content-Length`. Trailer fields are forwarded only as `hyper` does (when the request
   carried `TE: trailers`), else dropped.
4. Pipelined requests are serialised: request *n+1* is sent upstream after response *n* (each
   may go to another pod); answers come back in order.
5. Keep-alive is per hop: `Connection`, the fields it names, `Keep-Alive`, `Proxy-Connection`
   and `TE` (except `trailers`) are consumed by the sidecar; the app's `Connection: close`
   closes the app side only; upstream connections are pooled.
6. `Expect: 100-continue` is forwarded; the sidecar reads the app's body when the upstream asks
   for it (it relays the upstream's `100 Continue`, or the final status the upstream sends
   early).
7. Responses: `obs-fold` in a response head is unfolded to one space
   (`allow_obsolete_multiline_headers_in_responses`); a response head `hyper` rejects becomes
   `502` `x-mimic-error: invalid-response`; response header order across names is not kept.
8. **`Upgrade` and `CONNECT` mid-connection**: the head reader inspects every request head of
   a mesh-routed connection. A request carrying `Upgrade` (websocket, `h2c`) or method `CONNECT`
   waits until the earlier responses of the connection were delivered, then the connection
   leaves `hyper`: a pod is chosen by P2C, a new upstream connection is opened, the head is
   written as received, and from then on bytes are relayed untouched until either side closes
   (no retry after the head was written; access line `upgrade: true`). A connection that starts
   with the HTTP/2 preface goes to the HTTP/2 path.

### 3.5 TLS

- **TLS that mimic does not intercept** (review B4; every TLS connection in `none`, and in
  `discovery` or `mock` every one the existing rules pass through) gets **no L7 routing and is
  never re-originated**: it is balanced at L4 over the Service's pod IPs (P2C per connection),
  the bytes are untouched, the client keeps its SNI and verifies the server certificate against
  the name it called, exactly as through kube-proxy. Certificate expectation: the same as
  without mimic (valid for the name the client uses, e.g. `cart.shop.svc`); mimic adds none.
  No route, fault, mirror or retry after the first byte applies; connect failures are retried
  on another pod.
- **TLS that mimic intercepts** (the existing spec §5 rules) gets L7 handling. The sidecar
  re-originates TLS to the chosen **pod IP** with: SNI = the SNI the app sent (else the host of
  the request's authority); the server certificate verified against that same name, **never
  against the IP**, with the trust roots used today for intercepted upstreams (spec §5);
  hostname verification always on (no option turns it off). A verification failure is a connect
  failure on that pod (`connect_failed`, retried on another pod, log field `tls_error`), and
  `503 connect-failed` once every try failed. Certificate expectation: valid for the Service
  name the app calls, which it already has to be for the app's own verification today.
- Rejected: re-originating TLS that was not intercepted (it would need interception in `none`,
  see §2, and would change who verifies the server); L7 over TLS verified against the IP
  (certificates do not name pod IPs, so it would fail or tempt an implementation to skip
  verification).

## 4. Service discovery and the scoped view

### 4.1 What the controller watches

Both controller replicas run `kube-runtime` reflectors (no leader, every step idempotent, as
today) on: `Service`, `EndpointSlice` (`discovery.k8s.io/v1`), `HTTPRoute` and
`ReferenceGrant` (`gateway.networking.k8s.io`, when the CRDs exist), `MimicConfig`,
`MimicWorkload`, and from M6 `GatewayClass` and `Gateway`. Only metadata and the fields used are
kept in memory (selectors and annotations dropped).

### 4.2 The scope of one sidecar

A sidecar of deployment `d` in namespace `n` receives the **view** of `(n, d)`:

- every Service of `n`;
- every Service declared in `spec.destinations` of the `MimicConfig`s that target `d`, when it
  is in `n`, or when its namespace **consents** with a Gateway API `ReferenceGrant` (below);
- every Service referenced as a `backendRef` or mirror of an HTTPRoute attached to one of the
  above (closure depth 1: the backends' own routes are not followed — GAMMA routes once, at
  the caller);
- the routes attached to those Services that apply to callers in `n` (producer routes, in the
  Service's namespace, apply to every caller; consumer routes, in `n`, apply only to `n`);
- the policies of `MimicConfig`s targeting those Services, and the admission contract of `d`.

**Consent of the target namespace** (review B2; the standard Gateway API `ReferenceGrant`, the
same object cross-namespace `backendRefs` need): a destination `{namespace: risk, name:
scoring}` declared in `shop` is resolved only when namespace `risk` holds a `ReferenceGrant`
with `from: [{group: mimic.platform.io, kind: MimicConfig, namespace: shop}]` and `to:
[{group: "", kind: Service}]` (optionally `name: scoring`). Without it, or when the
`ReferenceGrant` CRD is not installed, the destination is left out of the view, the
`MimicConfig` gets `ResolvedRefs: False`, reason `RefNotPermitted`, with a message naming the
destination, and calls to it take the out-of-scope path (ClusterIP, §4.4). Deleting the grant
removes the destination at the next view. The depth-1 closure through route `backendRefs` and
mirrors follows the Gateway API rule (a ReferenceGrant from `HTTPRoute`, §5), so no path puts
another namespace's pod IPs in a view without that namespace's consent.

The view is a **pure function** of the watched objects (`mimic-core` `mesh::view`), serialised
as canonical JSON (`BTreeMap`, sorted keys), with an `ETag` = format + SHA-256 of the bytes, so
two replicas with the same objects serve byte-identical views. Draft:

```json
{"format": 1, "cluster": "local", "mode": "none", "mesh": true,
 "admission": {"concurrency": 64, "rps": 200},
 "services": {"shop/cart": {
    "ips": ["10.96.12.7"], "ports": {"80": {"name": "http", "targetPort": "http"}},
    "endpoints": {"80": [{"ip": "10.244.1.17", "port": 8080, "ready": true, "zone": "a"}]},
    "generations": {"endpointslice/shop/cart-x7k2p": 41},
    "routes": [{"name": "shop/cart-canary", "rules": [<compiled HTTPRoute rule>]}],
    "policy": {"retries": {...}, "outlierDetection": {...}, "circuitBreaker": {...},
               "loadBalancer": {...}, "faults": [...]}}}}
```

Bounds (controller variables, each refused value → exit 2): at most
`MIMIC_MESH_MAX_SERVICES` (1024) services and `MIMIC_MESH_MAX_ENDPOINTS` (10 000) endpoints per
view, `MIMIC_MESH_MAX_VIEW_BYTES` (4 MiB). Over a bound, the services of `n` come first, then
declared destinations in name order; the rest is left out, counted
(`mimic_mesh_view_truncated_total`) and logged once per view change, and those Services fall
back to the out-of-scope path.

### 4.3 How the view reaches the sidecar

`GET /v1/workloads/<deployment>/mesh?format=1` on the existing sidecar API (§7.2 of the spec:
same TLS, projected token, `TokenReview` cache, per-source rate limit, a pod reads its own
deployment only), as a **long-poll**: with `If-None-Match` equal to the current ETag the
controller holds the request until the view changes or `MIMIC_MESH_WATCH_TIMEOUT` (30 s)
passes (`304`). Changes are coalesced for `MIMIC_MESH_DEBOUNCE` (100 ms). Views are cached per
`(n, d)` key and shared between deployments with identical inputs. Held requests have their own
budget, `MIMIC_SIDECAR_API_MAX_WATCHES` (10 000), separate from the connection cap of the other
endpoints (a full watch budget answers `503` + `Retry-After`, never starves pushes or TLS).

From M1 the view also carries the **mode**, so a mode change reaches every pod within about one
second instead of one `MIMIC_EGRESS_PULL_INTERVAL`; the egress pull remains for the snapshot.

**Fan-out, memory and generations** (review N3):
- A view is built from its **inputs** (the namespace part plus the deployment's declared
  destinations, mode, admission and mesh switch); deployments with identical inputs share one
  cached entry, so a namespace whose deployments declare nothing costs one build per change,
  not one per deployment. The cache is LRU-bounded by `MIMIC_MESH_VIEW_CACHE_BYTES` (64 MiB)
  and counted inside the controller's 256 MiB budget (§14); an evicted view is rebuilt on
  demand.
- Debounce is **adaptive**: `MIMIC_MESH_DEBOUNCE` (100 ms) after a quiet period, doubled up to
  1 s while changes keep arriving, so a rollout costs at most one build per second per input
  key and each sidecar receives at most one view per second.
- Views are sent gzip-compressed when the sidecar accepts it (endpoint lists compress well).
  Deltas were rejected: full state is self-healing and the size bound already holds.
- **Generations across replicas**: two replicas may lag each other, so a sidecar could get an
  older view after a newer one. The view lists the `metadata.generation` of each EndpointSlice,
  HTTPRoute and MimicConfig it was built from (the API server increments them on every change);
  the sidecar refuses a view in which an object it already knows has a lower generation, keeps
  the current one and polls again (`mimic_mesh_view_pulls_total{result="older"}`). Services and
  deletions carry no comparable counter: a regression there lasts at most the lagging replica's
  watch delay (accepted limit).

Alternatives rejected: an xDS/gRPC stream (protobuf toolchain, `tonic`, a protocol we would only
half implement); a controller-initiated push to every sidecar (needs a listener and
authentication on every sidecar, and N outbound connections from each replica); a plain periodic
pull (seconds of stale endpoints during rollouts). Long-poll keeps the existing authenticated,
sidecar-initiated channel and gives push-like latency.

### 4.4 Destinations outside the scope

A call to a Service that is neither in `n` nor declared (or declared without the target
namespace's consent, §4.2) is **forwarded to its ClusterIP as today** (kube-proxy balances per connection): it works, but without routes, retries, breaker,
faults or per-pod metrics. The sidecar counts it (`mimic_mesh_unscoped_connections_total`) and
the controller, which sees the sidecar's report in `discovery`, does not try to guess: the
explorer UI (§17.2) shows outbound authorities that look like Services of other namespaces
(`<svc>.<ns>.svc…`) without a declaration, with a hint to declare them.

## 5. Gateway API usage

- **Prerequisite**: the Gateway API CRDs, **standard channel only**, version ≥ v1.2 (HTTPRoute
  `timeouts` in Standard); tested with v1.7.0. Experimental fields are pruned by the standard
  CRDs, so anything the controller reads is standard by construction. Without the CRDs the
  controller logs one WARN `gateway api crds missing`, sets `mimic_mesh_gateway_api_present 0`,
  and the mesh works without routes (load balancing, retries and policies still apply). The
  kind e2e installs the standard manifests, pinned by digest-equivalent (a pinned release URL
  and its SHA-256 in `scripts/check-pins.sh`).
- **HTTPRoute with `parentRefs` a Service** (GAMMA) for mesh routing. Supported in M3:
  matches on path (`Exact`, `PathPrefix`), headers and query params (`Exact`), method; weighted
  `backendRefs` (Service-level canary: `cart-v1` 90, `cart-v2` 10); filters
  `RequestHeaderModifier` and `ResponseHeaderModifier` (Core), `RequestMirror`; `timeouts.request`
  and `timeouts.backendRequest`. A route using anything else (`RegularExpression` matches,
  `URLRewrite`, `RequestRedirect`, `CORS`, `ExtensionRef`) is **not programmed**: its parent
  status gets `Accepted: False`, reason `UnsupportedValue`, a message naming the field, and the
  Service keeps its default behaviour. `retry` (promoted to Standard in v1.7.0, released
  2026-10-29: `codes`, `attempts`, `backoff`) is ignored with condition message `retry is
  configured by MimicConfig` (owner decision: never app 5xx). Versions checked against the
  release notes: `timeouts` Standard since v1.2.0, `retry` since v1.7.0.
- **Status**: the controller writes `status.parents[]` for Service parents with
  `controllerName: mimic.platform.io/mesh` and conditions `Accepted`, `ResolvedRefs`
  (server-side apply, field manager `mimic-mesh`, idempotent across replicas).
- **ReferenceGrant** is required, per the Gateway API, for a `backendRef` or mirror in another
  namespace; without one the rule's backend counts as invalid (`ResolvedRefs: False`,
  `RefNotPermitted`) and requests to it get `500` as the spec requires. Mimic also requires one
  (`from: MimicConfig`, `to: Service`) for `MimicConfig.destinations` in another namespace
  (§4.2); this is why the controller watches `ReferenceGrant` from M1, before routes exist.
- **North-south (M6)**: a `GatewayClass` `mimic` (`controllerName:
  mimic.platform.io/gateway-controller`, created by the chart when `gateway.enabled`); for each
  `Gateway` of that class the controller creates a gateway `Deployment` and `Service` in the
  Gateway's namespace (§15 for the privileges), running the sidecar binary in gateway role
  (`MIMIC_ROLE=gateway`): it listens on the Gateway's listeners and routes the HTTPRoutes whose
  `parentRefs` name that Gateway with the same client as the mesh (§3).
- Why Gateway API and not our own route CRD: it is the Kubernetes standard, tools and users
  know it, and GAMMA defines exactly the caller-side semantics we want. Mimic's own CRD carries
  only what the standard does not have (owner decision).

## 6. The `MimicConfig` CRD

Namespaced, `mimic.platform.io/v1alpha1`, status subresource. Attached by `targetRefs`
(GEP-713 direct policy attachment, `LocalPolicyTargetReference`: same namespace only). A
`Deployment` target carries workload settings; a `Service` target carries destination policies
applied by **every caller** of that Service. CEL rules reject fields that do not fit the target
kinds.

```yaml
apiVersion: mimic.platform.io/v1alpha1
kind: MimicConfig
metadata: {name: cart, namespace: shop}
spec:
  targetRefs:                    # 1–16, same namespace
    - {group: apps, kind: Deployment, name: cart}
    - {group: "", kind: Service, name: cart}
  # --- Deployment targets ---
  destinations:                  # Services outside the namespace this workload calls (≤ 256)
    - {namespace: risk, name: scoring}   # resolved only with a ReferenceGrant in `risk` (§4.2)
  mesh:
    enabled: true                # false: byte-exact relay for this workload (§3.4)
  admission:                     # per pod of the deployment (§9); absent: no admission control
    concurrency: 64              # 1–100000 requests in flight
    rps: 200                     # 1–1000000 requests per second
  # --- Service targets ---
  retries: 3                     # 0–10 retries after the first try: 3 → at most 4 tries
  retryBudget: {percent: 20, minPerSecond: 10, window: 10s}
  outlierDetection:
    consecutiveFailures: 5       # proxy-level failures (§7), 1–100; default 5
    consecutive5xx: 0            # app 5xx, 0 = off (default); counted for ejection only, never retried
    baseEjectionTime: 30s        # doubled per repeat ejection, capped at 300s
    maxEjectionPercent: 50
  circuitBreaker:
    maxRequests: 1024            # in flight from one caller pod to the Service; over it: 503 breaker-open
    maxConnections: 1024         # HTTP/1 connections per pod in one caller pod's pool
  loadBalancer:
    slowStart: 30s               # 0s disables
  faults:                        # ≤ 16; applied by callers, first match wins
    - match: {path: {type: PathPrefix, value: /checkout}, method: POST}   # HTTPRouteMatch shape
      delay: {percent: 10, fixed: 500ms}
      abort: {percent: 1, status: 503}
status:
  conditions:
    - {type: Accepted, status: "True"}       # False: Conflicted | TargetNotFound | Invalid
    - {type: ResolvedRefs, status: "True"}   # False: RefNotPermitted (destination without grant)
```

- **Defaults without any `MimicConfig`**: `retries: 3`, i.e. at most 4 tries (review N1: the
  owner's "3 attempts" is read as three retries, the field says so by its name), with the
  budget above, outlier detection on
  (5 consecutive proxy-level failures), breaker 1024/1024, slow start 30 s, no faults, no
  admission, no declared destinations.
- **Conflicts**: several `MimicConfig`s targeting the same object: the oldest
  (`creationTimestamp`, then name) wins **as a whole**; the others get `Accepted: False`,
  reason `Conflicted`. Per-field merging was rejected: it makes the effective policy hard to read
  in `kubectl get`.
- **Effective config is pure** (`mimic-core` `mesh::policy`): validation, precedence, defaults,
  then embedded in the view.

## 7. Retries

Semantics of Linkerd and Istio (owner decision): **only proxy-level failures**, never an app
status.

| Failure | Retried | Why it is safe |
|---|---|---|
| Connect refused, connect timeout (1 s), no route | all methods | the request never left the sidecar |
| HTTP/2 `REFUSED_STREAM`, stream above a `GOAWAY` last-stream-id | all methods | the peer declares it did not process it |
| `503` with `x-mimic-error` `overloaded` or `queue-timeout` **produced by the peer sidecar itself** (rules below) | all methods | the callee's sidecar refused it before the app saw it (§9) |
| Reset or close before any response byte, after the request was written | idempotent methods only (`GET`, `HEAD`, `OPTIONS`, `TRACE`, `PUT`, `DELETE`) | the app may have processed it |
| Any response from the app (`5xx` included), a timeout, a reset after the response head | never | app semantics |

- **Tries**: `retries` retries after the first try (default 3, so at most 4 tries;
  `mimic_mesh_attempts_total` counts every try), each on **another pod** (P2C over the
  endpoints not yet tried; when all were tried, any). No backoff for connect failures and
  refused streams; 25 ms ± 50 % jitter before retrying a mimic `503`.
- **Budget** (`retryBudget`): retries in a window may not exceed `percent` of the requests plus
  `minPerSecond × window` (Linkerd's retry budget and its default floor of 10/s), per caller
  pod and Service. Over budget the
  failure is returned as is and counted (`mimic_mesh_retry_budget_exhausted_total`).
- **Bodies**: a request body is buffered for replay up to `MIMIC_MESH_RETRY_BUFFER` (64 KiB);
  a longer body is streamed and its request is not retried after its first byte was written.
- **Timeouts**: `timeouts.backendRequest` bounds one attempt, `timeouts.request` the whole call
  with its retries (§5). A timeout is not retried.
- **Outcome when retries are exhausted**: connect failures → `503` `x-mimic-error:
  connect-failed`; a callee's `503` → passed to the app as received; a reset → propagated as
  today (connection close or `RST_STREAM`).

**Trusting `x-mimic-error`** (review B1). The signal means "not processed, safe to retry" only
when the peer's sidecar produced it; an app that forwards a downstream response must never make
its own callers retry. Two rules make that hold:

1. **Stripping**: a meshed inbound sidecar removes every `x-mimic-*` field from the app's
   response heads before they leave the pod (HTTP/1: the head reader finds them, and only a
   head that has one is rewritten, others stay byte-exact; HTTP/2: removed from the header
   block). Only the sidecar adds them afterwards. Counted `mimic_mesh_stripped_headers_total`.
2. **Binding to the peer**: every refusal the inbound sidecar generates also carries
   `x-mimic-endpoint: <pod ip>:<port>` (the address it was reached on). The caller's sidecar
   trusts `overloaded`/`queue-timeout` only when that value equals the pod it sent this try to.
   This covers peers that do not strip (no sidecar, or mesh switched off): a response they
   forward from a downstream pod names that pod, not themselves.

Any other `x-mimic-error` response is an app response: never retried, passed through.

**Retry bound per tier.** A trusted signal is consumed by the hop that received it (the app may
see the final one, but its own sidecar strips it on the way back), so signalled and
connect-failure retries never reach an app: a request reaches at most one app per tier, whatever
the depth, and each hop sends at most `1 + retries` tries within its own budget. Only reset
retries of idempotent methods can reach an app twice; at depth *d* they add at most `percent`
per hop (the budget), i.e. a factor `(1 + percent/100)^d` plus the `minPerSecond` floors, never
`(1 + retries)^d`.

## 8. Ingress behaviour

The inbound sidecar forwards to its own app only: **no inbound re-route, no inbound retry**. An
app `503` (or any status) is passed through, minus any `x-mimic-*` field (§7, stripping); the
caller's sidecar does not retry it. The only answers the inbound sidecar
generates are the admission refusals of §9 (and the existing load-marker `599`).

## 9. Admission control

The contract is declared per workload (`MimicConfig.spec.admission`, Deployment target) and
**enforced per pod** by its inbound sidecar: each pod accepts at most `concurrency` requests in
flight **and** at most `rps` requests per second. Per pod because it is the only stateless,
coordination-free option and because it is what an HPA scales: more pods, more capacity.

- **Admission** (pure state machine in `mimic-core` `mesh::admission`, clock passed in):
  a request is admitted when `in_flight < concurrency` **and** a token is available in a bucket
  of rate `rps` and burst `max(1, ceil(rps / 10))`. Breaking either limit stops accepting.
- **Queue** (not configurable, owner decision): otherwise it waits in a FIFO queue of
  `max(1, ceil(concurrency / 10))` places (10 % of the contract) for at most **150 ms**.
  Queue full → refused `overloaded`; 150 ms elapsed → refused `queue-timeout`.
- **Refusal**: `503`, `x-mimic-error: <reason>`, `x-mimic-endpoint: <pod ip>:<port>` (§7),
  `content-length: 0`, written after the answers to earlier requests of the connection (the
  same path as the load marker's refusal, spec §4.1). HTTP/1 keeps the connection (review N5):
  the refused request's body is read and discarded when it is at most 64 KiB and arrives within
  1 s; otherwise the answer carries `connection: close`. On HTTP/2 the stream is answered, the
  connection stays. The app never sees a refused request, so the caller retries it on another
  pod (§7).
- **What `in_flight` counts**: a request from its admission until its response is complete,
  so long-lived responses (SSE, gRPC streams) hold their slot for their whole life (documented:
  size the contract for them, or leave such workloads without one). An `Upgrade` (websocket)
  holds its slot until the `101`, then leaves the count; upgraded bytes are not metered.
- **What is metered**: inbound cleartext HTTP/1 requests and HTTP/2 streams, load-test requests
  included. Not metered (accepted limit): inbound TLS (never intercepted), opaque TCP, probes
  (`/app-health/…` on the admin port). On HTTP/1 the relay holds each request head until it is
  admitted (as it does for the load marker); without a contract nothing is held and the relay is
  byte-exact as today.
- The contract changes with the view (no restart): a lower limit lets in-flight requests finish.

**`x-mimic-error` reasons** (header on every response mimic generates, same value as the
`reason` label and log field):

| Reason | Status | Who answers | Retried by the caller's sidecar |
|---|---|---|---|
| `overloaded` | 503 | callee sidecar, admission queue full | yes, another pod |
| `queue-timeout` | 503 | callee sidecar, 150 ms in queue | yes, another pod |
| `no-endpoint` | 503 | caller sidecar, backend has no ready endpoint | no |
| `breaker-open` | 503 | caller sidecar, `maxRequests` reached or every endpoint ejected | no |
| `connect-failed` | 503 | caller sidecar, every attempt failed to connect | (already retried) |
| `timeout` | 504 | caller sidecar, `HTTPRoute` timeout (Gateway API requires 504) | no |
| `fault-injected` | configured | caller sidecar, `faults[].abort` | no |
| `invalid-request` | 400 | caller sidecar, request head `hyper` rejects (§3.4) | no |
| `invalid-response` | 502 | caller sidecar, response head `hyper` rejects (§3.4) | no |

`timeout`, `fault-injected`, `invalid-request` and `invalid-response` extend the owner's list of
five: a route timeout has to answer `504` per the Gateway API, an injected abort must be told
apart from a real failure, and re-framing rejections must not look like app answers.

## 10. Failure signalling, metrics and logs

Labels stay bounded: `service` and `backend` are `<ns>/<name>:<port>` of the view (bounded by
`MIMIC_MESH_MAX_SERVICES`), `route` is `<ns>/<name>/<rule index>` (bounded by the view), pod
labels are capped by `MIMIC_METRICS_MAX_ENDPOINTS` (256, overflow `other`) and **evicted**
(review N4): the series of an endpoint that left the view are removed 5 minutes after it left
(scrapes see the final counter first), so pod churn frees slots instead of filling `other`. No
label carries a path, a header value or a caller-controlled string.

**Sidecar metrics** (new):

| Metric | Type | Labels |
|---|---|---|
| `mimic_mesh_requests_total` | counter, per call (after retries) | `service`, `route`, `backend`, `status_class`, `reason` (`none` or a §9 reason) |
| `mimic_mesh_request_duration_seconds` | histogram, buckets of `mimic_http_request_duration_seconds` | `service`, `backend`, `status_class` |
| `mimic_mesh_attempts_total` | counter, per attempt | `backend`, `result` (`ok`, `connect_failed`, `refused_stream`, `reset`, `overloaded`, `queue_timeout`, `timeout`) |
| `mimic_mesh_endpoint_requests_total` | counter, per pod | `backend`, `endpoint` (`ip:port` or `other`), `status_class` |
| `mimic_mesh_endpoint_in_flight` | gauge | `backend`, `endpoint` |
| `mimic_mesh_retries_total`, `mimic_mesh_retry_budget_exhausted_total` | counter | `backend`, `reason` |
| `mimic_mesh_ejections_total`, `mimic_mesh_ejected_endpoints` | counter, gauge | `backend` |
| `mimic_mesh_mirrored_total` | counter | `backend`, `result` (`sent`, `skipped_body`, `error`) |
| `mimic_mesh_faults_total` | counter | `service`, `kind` (`delay`, `abort`) |
| `mimic_mesh_unscoped_connections_total` | counter | — |
| `mimic_mesh_view_age_seconds`, `mimic_mesh_view_services`, `mimic_mesh_view_endpoints` | gauge | — |
| `mimic_mesh_view_pulls_total` | counter | `result` (`ok`, `not_modified`, `older`, `invalid`, `error`) |
| `mimic_mesh_stripped_headers_total` | counter, inbound `x-mimic-*` fields removed from app responses | — |
| `mimic_admission_requests_total` | counter, every request seen, refused ones included | `result` (`admitted`, `queued`, `overloaded`, `queue-timeout`) |
| `mimic_admission_in_flight_seconds_total` | counter, integral of `in_flight` over time | — |
| `mimic_admission_in_flight`, `mimic_admission_queue_depth` | gauge | — |
| `mimic_admission_limit` | gauge | `kind` (`concurrency`, `rps`) |
| `mimic_admission_utilization` | gauge, the HPA signal (§11): `max(ewma(in_flight) / concurrency, ewma(demand rate) / rps)`, EWMA with a 15 s time constant updated every 100 ms, demand = admitted + refused, **not capped at 1** | — |

**Controller metrics**: `mimic_mesh_objects{kind}`, `mimic_mesh_view_builds_total{result}`,
`mimic_mesh_view_build_duration_seconds`, `mimic_mesh_watches` (held long-polls),
`mimic_mesh_view_truncated_total`, `mimic_mesh_route_status_total{result}`,
`mimic_mesh_gateway_api_present`, and from M6 `mimic_gateway_reconciles_total{result}`.

**Logs**: the access line of a mesh-routed exchange gains `route`, `backend`, `upstream`
(pod `ip:port`), `attempts`, `reason` (the `x-mimic-error` value when mimic answered),
`mirrored` and `fault`. Component logs: INFO `mesh view applied` (services, endpoints, etag)
at each change; WARN `mesh view stale` once when the view passes `MIMIC_MESH_STALE_AFTER`; INFO
`endpoint ejected` / `endpoint restored` (backend, endpoint, ejection time); WARN
`retry budget exhausted` at most once per second per backend; INFO `admission contract changed`.
Dashboards (§9.1 of the spec) gain a *Mesh* row (calls, attempts, reasons, ejections) and an
*Admission* row (utilisation, refusals by reason).

## 11. Autoscaling

- **Signal** (review N2): the gauge is smoothed (EWMA over 15 s) so that a scrape every ~15 s
  does not alias 1-second noise, and it counts **refused demand**, so it goes above 1.0 under
  overload and the HPA can grow by more than one step at once. The raw counters
  (`mimic_admission_requests_total` by result, `mimic_admission_in_flight_seconds_total`) are
  exported too, for an adapter rule that computes `rate()` itself; the chart's rule uses the
  gauge.
- The HPA scales a deployment on `mimic_admission_utilization` as a `Pods` metric through the
  **custom metrics API**, served by **prometheus-adapter** (owner decision), which reads the
  **chart-installed Prometheus by default** or an external one (`autoscaling.prometheus.url`).
- Chart: `autoscaling.enabled` installs prometheus-adapter (image pinned by digest, distroless
  base, non-root, Pod Security `restricted`) with one rule exposing the gauge per pod, its
  `APIService` `v1beta1.custom.metrics.k8s.io`, RBAC and NetworkPolicy; it turns on the
  sidecar scrape objects it needs. Mimic does not create HPAs (least privilege): the wiki gives
  the HPA manifest (`averageValue: 700m` = 70 % of the contract).
- Risk accepted: prometheus-adapter's last release is v0.12.0 (2024); the repository is not
  archived and still receives commits. The integration is one rule and a standard API, so
  replacing it (KEDA, another adapter) later changes the chart only.
- The cluster can have only one `custom.metrics.k8s.io` APIService: the chart refuses to render
  it when `autoscaling.adapter.existingAPIService` says one is already there, and documents
  adding mimic's rule to the existing adapter instead.

## 12. Interaction with mock mode and load tests

- In `mock`, outbound HTTP is answered by the mock as today: **no route, LB, retry, breaker,
  fault or mirror applies to mocked calls**. `spec.mock.passthrough` destinations and
  non-HTTP connections go through the mesh path like in `none`.
- Discovery records one exchange per app request under the app's authority, whatever the
  attempts; mirrored copies, retries and mimic-generated answers (`x-mimic-error`) are **not
  recorded**, so a mock never replays a mimic `503` (counted `result="synthetic"` in
  `mimic_discovery_exchanges_total`).
- Load tests send to the pods of the deployment directly (spec §7.8): the inbound admission
  contract applies to them, which is the point — a load test without a contract finds the
  saturation point, the contract is then set below it and a second run checks that refusals
  appear as `503 overloaded` instead of latency collapse. `mimic-load` counts responses with
  `x-mimic-error` under their reason (`overloaded`, `queue-timeout`) in its results and summary.
- Fault injection does not apply to mocked calls (the mock has its own latency factor).

## 13. Failure modes

| Situation | Behaviour |
|---|---|
| Controller down | Sidecars keep their last view (and mode) and retry the long-poll with backoff (1 s to 30 s, jitter). Load balancing, retries and outlier detection keep working on the last endpoints. |
| View stale (no successful contact for `MIMIC_MESH_STALE_AFTER`, 60 s) | Routes and policies still apply, but the chosen **backend Service is reached through its ClusterIP** (kube-proxy, fresher endpoints) instead of pod IPs: no stale pod IP is used once the view is old. WARN once, `mimic_mesh_view_age_seconds` shows it. |
| Stale endpoint (pod gone, IP not yet removed) | Connect fails or is reset → retried on another pod (§7) and counted for outlier detection. |
| IP reused by another pod before the view updates | Possible within the view's delay (about 1 s); no identity check without mTLS (non-goal). Accepted limit, bounded by the long-poll latency and the stale fallback. |
| Cold start (sidecar before its first view) | The sidecar becomes ready once listeners are bound and the first view arrived, or after `MIMIC_MESH_STARTUP_WAIT` (2 s) without it; until the first view, every call takes the out-of-scope path (ClusterIP). |
| Restart during a controller outage (review N4) | **Fail closed on mode**: the mode never comes from the absence of a view. A restarted sidecar container takes its mode from the existing state file `/mimic/state/mode` (spec §8 "Mode state"), and from M1 also its last view from `/mimic/state/mesh.json` (same write-then-rename, same emptyDir the app cannot read), aged by its `fetchedAt` so that the stale rule above applies; a new pod gets the mode of its admission in `MIMIC_MODE` from the webhook, as today. A `mock` workload therefore never comes up in `none` or `discovery`. A new pod has no admission contract until its first view (accepted limit, one WARN `admission contract unknown`). |
| Backend with no ready endpoint | `503 no-endpoint` immediately (no ClusterIP fallback: kube-proxy would also reject). |
| Gateway API CRDs absent or removed | Routes dropped from views (§5); the mesh keeps balancing. |
| Valkey down | No effect on the mesh (the view is built from Kubernetes objects only). |

## 14. Performance budgets

Measured with the benchmark harness of §17.3 on this machine (kind, gentle rates: 50, 100 and
200 requests/s, 30 s per run, 1 KiB bodies, a fixed-latency fortio backend), against **B0**,
the baseline of the current sidecar. Each milestone's issue cannot close until its row holds;
a micro-benchmark (`criterion`) covers the pure code.

| Milestone | Budget (at 200 rps unless said) |
|---|---|
| B0 | Recorded, not judged: direct vs `discovery` sidecar, p50/p99 latency, CPU, RSS of caller and callee sidecars. |
| M0 | `none` adds at most 60 % of `discovery`'s added p50, and uses at most 70 % of its CPU. |
| M1 | Mesh-routed call adds ≤ B0 + 0.5 ms p50 and ≤ B0 + 2 ms p99; sidecar RSS ≤ B0 + 16 MiB with a 100-service/1000-endpoint view; EndpointSlice change → sidecar applies it in ≤ 2 s p99; controller RSS ≤ 256 MiB with 1000 Services/10 000 endpoints (synthetic objects), view cache included; rollout of a 50-pod deployment in a namespace of 200 sidecars: each sidecar receives ≤ 1 view/s and the controller averages ≤ 0.5 core during the rollout. P2C pick ≤ 2 µs (criterion). |
| M2 | No failure: no change vs M1 beyond noise (≤ 0.2 ms p50). One of three pods refusing connections: error rate seen by the client 0 %, p99 ≤ M1 + 5 ms. |
| M3 | Route match ≤ 10 µs for 100 rules (criterion); canary 90/10 within ±2 % over 10 000 requests; mirror adds ≤ 0.2 ms p50 to the primary call. |
| M4 | Outlier ejection within 5 failing requests; breaker answers ≤ 1 ms. |
| M5 | Under the contract: ≤ 0.5 ms added p99 vs M4; refusal answered ≤ 1 ms after the decision; queue wait never above 160 ms (150 + scheduling); at 2× the contract the admitted rate stays within ±5 % of `rps`. |
| M6 | Gateway hop p99 ≤ the M1 sidecar hop + 1 ms at 200 rps. |

If B0 already shows that this machine cannot sustain 200 rps cleanly, the budgets are taken at
100 rps and the benchmark report says so.

## 15. Security notes

**Untrusted inputs**: every request and response the sidecar routes (headers used for matching,
bodies buffered for retries and mirrors — bounded), the watched Kubernetes objects (written by
namespace users: HTTPRoutes, MimicConfigs, Services), the view as received by the sidecar
(validated with `deny_unknown_fields` and the same bounds as the controller, like the egress
set), the `x-mimic-error` and `x-mimic-endpoint` fields of upstream responses (trusted only
under the rules of §7), the request and response heads re-framed by `hyper` (§3.4, fuzzed).

**Privileges added**:
- Controller ClusterRole: `get/list/watch` on `services`, `endpointslices`, `httproutes`,
  `referencegrants`, `mimicconfigs`, and from M6 `gateways`, `gatewayclasses`; `patch` on
  `httproutes/status`, `mimicconfigs/status`, `gateways/status`, `gatewayclasses/status`. No
  Secret read is added anywhere.
- M6 only, and only in the namespaces listed in `gateway.namespaces` (RoleBindings, not a
  ClusterRole): create/update/delete `deployments` and `services` labelled
  `mimic.platform.io/gateway`. A `ValidatingAdmissionPolicy` lets the controller's service
  account create only Deployments whose single container is the pinned mimic image in gateway
  role, with mimic's security context and no host namespace, so a stolen controller token cannot
  run arbitrary pods. Listener certificates are mounted by the kubelet from same-namespace
  Secrets named by `certificateRefs`; the controller writes their names, never reads them.
- Sidecars and gateway pods gain no capability and no Kubernetes token: the view comes over the
  sidecar API with the existing projected token.

**What an attacker gains**:
- *In the pod* (app compromised): it can send any request through the sidecar as before. Its
  own `x-mimic-*` response fields are stripped by its meshed sidecar; without a sidecar, or with
  the mesh off, a forged signal is trusted only when bound to its own pod address (§7), so it
  can only make callers retry requests to itself on its sibling pods, within the retry budget
  and never more than `retries`. It cannot read the view or other namespaces' data (no token).
- *A benign app forwarding responses* (proxy, BFF): a downstream refusal never crosses a hop
  (stripping and binding, §7), so there is no retry amplification across tiers (review B1).
- *In the namespace* (can write HTTPRoutes and MimicConfigs): producer routes and Service
  policies only for Services of that namespace, which the attacker already controls; consumer
  routes affect only callers of that namespace; cross-namespace backends, mirrors **and
  declared destinations** need a ReferenceGrant from the target namespace; `targetRefs` cannot
  leave the namespace. Without that consent the controller (which reads EndpointSlices
  cluster-wide) never puts another namespace's pod IPs, ports, zones or readiness in a view, so
  a `MimicConfig` is no confused-deputy path around the RBAC on `endpointslices` (review B2);
  the destination is reported `RefNotPermitted`. Once granted, the target's pod IPs reach the
  declaring namespace's sidecars (not the app): accepted, the target namespace chose it, and
  NetworkPolicy stays the access control. Faults and admission limits can only degrade the
  attacker's own Services. A mirror sends copies of requests, credentials included, to a
  backend chosen by the route's author: a producer route's author owns the Service and already
  receives those requests; a consumer route's author only mirrors its own namespace's calls
  (accepted limit, written in the spec by issue 15).
- *On the network*: mesh traffic is cleartext pod to pod as without mimic (no mTLS, non-goal);
  TLS the app speaks is either untouched (L4, the app verifies the server) or, when
  intercepted, re-originated with verification against the Service name, never the IP, never
  disabled (§3.5); the view travels over the verified sidecar API TLS; a forged view is
  impossible without the controller's certificate.
- *Kill switches*: `mesh.enabled` is chart-level (cluster admin); `MimicConfig.spec.mesh` only
  switches off the mesh for the namespace's own workloads.
- *Resource exhaustion*: view, routes, policies and watches are bounded (§4.2, §4.3); pools and
  retry buffers are bounded per pod; admission protects the app, not the sidecar (the sidecar
  keeps `MIMIC_MAX_CONNECTIONS`).

## 16. Multi-cluster, kept in mind

Not built. To avoid a format break later: the view has a top-level `cluster` field (from
`MIMIC_CLUSTER_NAME`, default `local`) and Service keys are built by one function that can
prefix the cluster; metrics carry no cluster label (Prometheus external labels do that);
`MimicConfig.destinations` entries are objects, so a `cluster` key can be added without a new
version.

## 17. Parallel tracks

### 17.1 gRPC

Today gRPC is recorded as an endpoint per path with opaque messages, and the mock answers an
empty `OK`. Owner decision: gRPC is always recorded and forwarded like HTTP, and mock replays it.

- **Decoding** (pure, `mimic-core` `grpc`): length-prefixed message framing
  (compressed flag; `identity` and `gzip` decoded, any other `grpc-encoding` stays opaque, no
  Brotli), and a **schema-less protobuf wire-format** reader: field numbers, wire types,
  varints, fixed, length-delimited fields read as a nested message when they parse fully, else
  as UTF-8 string when valid, else bytes. Bounded (depth 32, 4096 fields, 256 KiB per message).
  Rendered as canonical JSON keyed by field number (`{"1": "…", "2": {"1": 5}}`), or by name
  when a descriptor is known.
- **Redaction**, applied before anything leaves the recorder: (1) **metadata name rules** —
  the existing header rules on gRPC metadata, and every `-bin` metadata value redacted;
  (2) **value detectors on wire-format strings** — the existing §6 detectors (JWT, PEM, cards,
  cloud keys, token prefixes) on every string field; (3) **high-entropy blobs** — a bytes or
  string field of at least 16 bytes with Shannon entropy ≥ 4.5 bits/byte (strings) or
  ≥ 7 bits/byte (bytes) is replaced by its placeholder; (4) **reflection when available** —
  the inbound sidecar asks the app's `grpc.reflection.v1` (then `v1alpha`) once per port and
  start, over cleartext h2c, with a 2 s timeout, never outbound; with descriptors, field
  **names** feed the name rules (`password`, `token`, …); (5) **per-field rules** —
  `MimicWorkload.spec.discovery.grpc.redact`: entries `<package.Service>/<Method>` +
  `request|response` + a field path by number (`3.1`) or by name when reflected.
- **Mock replay**: unary first — the recorded message bytes, re-encoded from the redacted
  wire form (placeholders as strings), `grpc-status` and trailers as recorded; then **server
  streaming**: the sequence of messages with their recorded inter-message delays (bounded:
  64 messages, delays × latency factor). Client and bidirectional streaming stay passed through.
- Criteria prefix `GRP-`; formats bump the report and egress set format numbers (old formats
  served as the spec requires).

### 17.2 Explorer UI

The UI on the controller's loopback listener (8080) becomes an explorer with four tabs, still
read-only and still served from the store and the controller's caches (never traffic):

- **Map**: a graph of deployments (nodes) and calls (edges) from the stored ingress and egress,
  with in-cluster authorities resolved to Services and their deployments (from M1 using the
  Services the controller watches; before, by `<svc>.<ns>.svc` names); edge labels show rate
  and error share from the stored counters; external authorities are leaf nodes.
- **Workloads**: every deployment with its mode, pods, last push, and **links to the mimic
  custom resources that configure it** (`MimicWorkload`, `MimicConfig`, `MimicLoadTest`, and from
  M3 the HTTPRoutes attached to its Services) with their spec rendered read-only (YAML, managed
  fields dropped) and their status conditions.
- **Endpoints**: today's per-deployment endpoint list, OpenAPI and Swagger UI, under one tab
  with a deployment picker and a filter.
- **Load tests**: runs per deployment with phase, summary, per-endpoint percentiles and gates.

No JavaScript framework beyond what the vendored Swagger UI already brings: server-rendered HTML
and a small self-written SVG layout for the map (deterministic layout, pure, in `mimic-core`).
Criteria prefix `EXP-`, checked in the pinned headless Chromium as UI-*.

### 17.3 Benchmarks

`scripts/bench.sh` on this machine: a kind cluster, a fortio server (fixed 5 ms latency) and a
fortio client pod, three setups (no sidecar, sidecar `discovery`, sidecar `none`; then mesh
setups per milestone), gentle rates (50, 100, 200 rps, 30 s each, one run at a time, a pause
between runs), collecting fortio's JSON, sidecar CPU and RSS from `/metrics`
(`process_cpu_seconds_total`, `process_resident_memory_bytes`). Output: `docs/bench/<date>-<setup>.json`
and a markdown summary appended to `docs/bench/README.md`. Never in CI (shared runners give
noise, and the owner asked for gentle local runs); every milestone issue attaches its run.

### 17.4 Hardened production values

`deploy/helm/mimic/values-production.yaml`, an overlay documented in the wiki: 3 controller
replicas with topology spread, PDB, priority class, resource requests and limits for every
container, `networkPolicy.webhookFrom` narrowed to the API server CIDR, external Prometheus and
PodMonitors instead of the bundled monitoring, Valkey persistence sized, `loadTest.enabled:
false`, images by digest, `MIMIC_LOG=info`. `helm lint --strict` and a render test check it; the
kind e2e installs with it once.

### 17.5 Small items

- The controller **declares `containerPort` 8080** (`name: mimic-ui`) for the UI listener;
  informational only, the listener stays on loopback and is reached by `kubectl port-forward`.
- The wiki **shows sidecar access logs**: sample inbound and outbound lines, every field, and
  `kubectl logs … | jq` recipes (by status, by authority, slow calls, from M1 by `reason` and
  `attempts`).

## 18. Milestones and dependency graph

| Milestone | Content | Issues (drafts) |
|---|---|---|
| B0 | Benchmarks of the current sidecar | 01 |
| — | Port 8080, access-log docs; production values | 02, 03 |
| M0 | Mode `none`, the new default | 04 |
| M1 | Scoped view, client-side LB over pod IPs, pools, P2C, slow start, failure modes | 05–09 |
| M2 | Retries of proxy-level failures, retry budget | 10, 11 |
| M3 | Gateway API HTTPRoute: routing, canary, timeouts, mirror | 12–15 |
| M4 | Outlier detection and circuit breaker, fault injection | 16, 17 |
| M5 | Admission contract, signalling, HPA | 18–20 |
| M6 | North-south gateway | 21–23 |
| gRPC | Decode, redact, record, reflect, mock unary, server streaming | 24–29 |
| UI | Explorer shell + Workloads, Endpoints, Load tests, Map | 30–33 |

```mermaid
graph LR
  B0[01 Bench B0] --> M0[04 M0 none]
  M0 --> V[05 MimicConfig + view] --> C[06 controller view API] --> S[07 sidecar view] --> LB[08 LB pools P2C] --> F1[09 M1 failure modes]
  F1 --> R1[10 retries] --> R2[11 retry budget]
  F1 --> G1[12 HTTPRoute ingest] --> G2[13 route match + canary] --> G3[14 timeouts]
  R1 --> G2
  G2 --> G4[15 mirror]
  R2 --> O[16 outlier + breaker]
  G2 --> FI[17 faults]
  R1 --> A1[18 admission] --> A2[19 signalling] --> H[20 HPA]
  S --> A1
  PV[03 production values] --> H
  G3 --> GW1[21 gateway reconciler] --> GW2[22 gateway HTTP] --> GW3[23 gateway HTTPS]
  G1 --> GW1
  O --> GW2
  P1[24 gRPC decode] --> P2[25 gRPC redact] --> P3[26 gRPC record] --> P5[28 gRPC mock unary] --> P6[29 gRPC streaming]
  P3 --> P4[27 gRPC reflection]
  U1[30 UI shell + Workloads] --> U2[31 Endpoints] & U3[32 Load tests]
  C --> U4[33 Map]
  U1 --> U4
```

ReferenceGrant handling for declared destinations lands with the view (05, 06), so M1's e2e
installs the pinned standard Gateway API CRDs that M3 also needs; the outlier exclusion of 08 is
a no-op until 16 exists.

The gRPC and UI tracks start in parallel with M0 (the UI's MimicConfig links appear when 05
lands; the Map tab needs the Service watch of 06). Benchmarks (01) come first because every
milestone budget is relative to B0; 02 and 03 have no prerequisite.
