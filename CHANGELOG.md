# Changelog

All notable changes to mimic are documented in this file. The format follows
[Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/). The reference for behaviour is
[`docs/SPEC.md`](docs/SPEC.md).

## [Unreleased]

### Upgrade notes

- **Mode `none` is the new default (#439).** A Deployment injected by label with no `MimicWorkload` is now in
  `none`: proxied, nothing recorded, no report shipped, outbound TLS passed through. It was in `discovery`. To
  record it, create a `MimicWorkload` with `spec.mode: discovery`; pods follow within one egress pull interval,
  with no restart. `MimicWorkload`s created before this release keep the `spec.mode: discovery` the API server
  stored at their creation (defaults are persisted on write), so they keep recording. `none` means "proxy only,
  nothing recorded"; it does not mean "no mesh" (the mesh, when it arrives, has its own switches).
  The persisted `discovery` is not a manifest default: recreating a `MimicWorkload` from a manifest without
  `spec.mode` (a GitOps prune and resync, delete and apply, a PUT) gives `none`. Set `spec.mode: discovery`
  explicitly in your manifests.

### Features

- Retry policy and budget per Service (#446, M2): `MimicConfig` targeting a Service gains `retries` (0 to 10,
  default 3; `0` also stops the fallback to the ClusterIP) and `retryBudget` (`percent` 20, `minPerSecond` 10,
  `window` 10s). Each caller pod keeps one budget per Service: a retry is granted while retries in the window
  stay within `percent` of the requests plus `minPerSecond` per second; the defaults allow 100 retries per 10 s
  plus 20 % of the requests. A refused retry returns the failure as is, is counted in
  `mimic_mesh_retry_budget_exhausted_total{backend}` and logged as the warning `retry budget exhausted` (at most
  once per second per backend). The view's `policy` carries both fields: upgrade the controller before the
  sidecars, since a sidecar refuses a view without them.
- Retries of mesh-routed requests (#445, M2): a request whose failure happened before the app could process it
  is tried again on another pod, at most 3 times (4 tries), then on the original destination when every try
  failed to connect. Retried: connect failures and refused HTTP/2 streams for every method, a trusted refusal
  (`503` with `x-mimic-error: overloaded` or `queue-timeout` and `x-mimic-endpoint` naming the pod of the try,
  cleartext only) for every method, and a close or reset before any response head for `GET`, `HEAD`, `OPTIONS`,
  `TRACE`, `PUT` and `DELETE`. A response of an app is never retried. Request bodies are held for replay up to
  `MIMIC_MESH_RETRY_BUFFER` (64Ki) per request and `MIMIC_MESH_RETRY_BUFFER_TOTAL` (8Mi) in all. The access line
  gains `attempts`; new metrics `mimic_mesh_attempts_total`, `mimic_mesh_retries_total`,
  `mimic_mesh_retries_skipped_total`, `mimic_mesh_retry_buffer_bytes`. The retry budget is #446 (below).
- **Meshed sidecars remove `x-mimic-*` fields from app responses** (#445, M2): with `MIMIC_MESH=on` (and a view
  that does not say `"mesh": false`) the inbound side removes every response field whose name starts with
  `x-mimic-`, so that only a sidecar writes them; counted in `mimic_mesh_stripped_headers_total`. A response the
  sidecar cannot frame then closes the inbound connection (`mimic_mesh_unframed_closes_total`). With the mesh off
  the relay is byte-exact as before.
- The sidecar follows its mesh view (#442, M1): with a controller URL it long-polls
  `GET /v1/workloads/<deployment>/mesh` (backoff 1 to 30 s with jitter, token read again for each request, view
  validated and bounded, an older generation refused), keeps the last view in `/mimic/state/mesh.json` for a
  container restart, and classifies each outbound connection as in the view or unscoped
  (`mimic_mesh_unscoped_connections_total`). The mode of the view is applied like a pulled mode. No traffic
  changes: every byte still goes to its original destination. New metrics `mimic_mesh_view_pulls_total`,
  `mimic_mesh_view_age_seconds`, `mimic_mesh_view_services`, `mimic_mesh_view_endpoints`; new variables
  `MIMIC_MESH`, `MIMIC_MESH_STALE_AFTER`, `MIMIC_MESH_STARTUP_WAIT` (chart: `mesh.enabled`, `mesh.staleAfter`,
  `mesh.startupWait`, written to the sidecars by the webhook). `/readyz` waits for the first view, at most
  `MIMIC_MESH_STARTUP_WAIT`. Pods injected before this release have no `MIMIC_MESH` and keep today's behaviour (the sidecar's own default is `off`) until they are recreated.
- The controller serves the mesh view (#441, M1): both replicas watch Services, EndpointSlices, Deployment metadata,
  `MimicConfig`s, `MimicWorkload`s and, when its CRD exists, `ReferenceGrant`s, and a sidecar fetches its own
  deployment's view with `GET /v1/workloads/<deployment>/mesh?format=1` (long-poll, `ETag`, gzip, debounced,
  cached, with a watch budget of its own). `MimicConfig` statuses (`Accepted`, `ResolvedRefs`, `RefNotPermitted`
  without a `ReferenceGrant`) are written by server-side apply, field manager `mimic-mesh`. Chart: `mesh.enabled`
  (`false` sets `MIMIC_MESH=off`: `404`, nothing watched) and the bounds; the ClusterRole gains read-only watches
  on those kinds and `patch` on `mimicconfigs/status`. No Secret access. Nothing consumes the view until #442.
- `MimicConfig` CRD and the pure mesh view (#440, M1): the chart installs `mimicconfigs.mimic.platform.io` (kept on
  uninstall; no controller acts on it yet) and `mimic-core` gains `mesh::policy` and `mesh::view`, the scoped
  view of a deployment with the consent rule for destinations of other namespaces (a `ReferenceGrant` from
  `MimicConfig` to `Service` in the destination's namespace). Nothing changes at run time before the controller
  and sidecar use them (#441, #442).
- Mode `none` (#439): `spec.mode` accepts `none`, `discovery` and `mock` and defaults to `none` for new objects. The
  effective mode of a Deployment is `mock` over `discovery` over `none`. A sidecar in `none` records nothing,
  intercepts no TLS (`interception: mode_none`) and ships nothing, still pulls its mode and follows a switch without
  a restart; new metric `mimic_mode{mode}` and `none` in `mimic_mock_mode_switches_total{to}`; `status.mode` counts
  `none` pods.

- Client-side load balancing, step 1 (#443, M1): an outbound connection to a Service of the mesh view whose client
  speaks cleartext HTTP/1 or `h2c` is balanced per request over the pod IPs of the Service (power of two choices on
  the least request, slow start for new pods, pools per pod with `MIMIC_MESH_POOL_IDLE_TIMEOUT`). Discovery still
  records one exchange under the app's authority; the access line gains `backend` and `upstream`; new metrics
  `mimic_mesh_requests_total`, `mimic_mesh_request_duration_seconds`, `mimic_mesh_endpoint_requests_total`,
  `mimic_mesh_endpoint_in_flight` (pods capped by `MIMIC_METRICS_MAX_ENDPOINTS`, evicted 5 minutes after they leave
  the view). Everything else, and every connection with the mesh switched off (`MIMIC_MESH=off`, `"mesh": false`), keeps
  the byte relay. A mesh-routed HTTP/1 connection has the accepted limits of spec §4.1 (header order across names,
  chunk extensions, hop-by-hop fields consumed, a rejected request answered `400`). A pod that cannot be connected is replaced by another endpoint once, then by the original ClusterIP (`mimic_mesh_fallbacks_total`), and a view older than `MIMIC_MESH_STALE_AFTER` is not used for pod addresses; there is no other retry until #445. A client silent for 50 ms after connecting stays on the byte relay. Ports 25, 587, 3306, 4444, 5432, 6379, 9300, 11211, 22 and 21 are opaque (byte relay, no detection wait; `MIMIC_MESH_OPAQUE_PORTS`).
- The inbound sidecar of a meshed pod answers `503` with `x-mimic-error: connect-failed`, `x-mimic-endpoint` (its own pod address) and `x-mimic-origin: inbound; pod=<name>` when its app refuses the connection, on cleartext HTTP/1 and `h2c` (#518, spec §4.1): the caller's sidecar retries it on another pod for every method (`result="app_refused"`), counted `mimic_mesh_inbound_refusals_total`. Over TLS and with the mesh off the connection is still reset.

- The controller container declares the read API port `mimic-ui` (8080/TCP) in the pod spec, for information: it stays on loopback and no Service exposes it (#437).
- The Map tab (#468): a deterministic layered graph of which deployment calls which, built from the stored outbound counters (exchanges and share of 5xx per edge) and the Services the controller watches, with external hosts as leaves, Services of other namespaces that the caller does not declare flagged `not declared`, links to the Workloads and Endpoints tabs, a bounded size (1000 deployments, 200 other nodes, 2000 edges) and every name escaped; new read route `GET /v1/map` (JSON, or SVG with `?format=svg`). No new Kubernetes permission.
- Map tab follow-ups (#505): `GET /v1/map` reads the stored outbound calls of the first 1000 deployments only and logs a failed read at WARN; `mimic.js` also refuses links with `..`, `.` (percent-encoded too), `//` or a backslash and a root that is not an SVG-namespace `svg`.
- The UI is an explorer with four tabs (Map, Workloads, Endpoints, Load tests; the first and the last two are placeholders). The Workloads tab lists every deployment with its mode, pods and last push and links the `MimicWorkload` and `MimicLoadTest` that configure it, each with its spec as read-only YAML and its status conditions; new read routes `GET /v1/resources` and `GET /v1/workloads/<ns>/<dep>/resources` (#465).
- Production overlay `deploy/helm/mimic/values-production.yaml` (#438): three controllers spread over zones and nodes, a priority class, requests and limits on every rendered container, images by digest, the Valkey exporter and the PodMonitors on, bundled monitoring and load tests off. The chart gains `controller.topologySpreadConstraints`, `controller.priorityClassName` and `valkey.priorityClassName`, empty by default.
- Benchmarks of the sidecar: `scripts/bench.sh` runs fortio against an echo server with and without the
  sidecar at 50, 100 and 200 rps (never above), records the latencies and the sidecar CPU and RSS in
  `docs/bench/`, and cleans up its namespace on exit. Baseline B0 of 2026-11-12 in `docs/bench/README.md` (#436).
- gRPC unary calls are recorded with their messages: the samples of a gRPC endpoint hold the first request and response message (read without a schema, keyed by field number) and the response trailers, redacted before anything leaves the sidecar (JWTs, tokens, cards, high-entropy values, `-bin` metadata) and again by the controller; the OpenAPI document shows the message, and `mimic_discovery_bodies_total` counts gRPC messages as `parsed`, `opaque`, `too_large` or `encoding`. A report with such a sample is `"version": 3` and the egress set has a `format` 2 (a pull of `format=1`, which sidecars up to 0.2.0-alpha.3 make, is served without the gRPC parts); a sidecar whose controller answers `400` to version 3 posts the report again without them (`mimic_discovery_pushes_total{result="grpc_dropped"}`). Streams, per-field rules, reflection and replay are later steps (#461).
- gRPC server streaming is recorded and replayed (#464): when the request holds one message, the samples of the endpoint hold up to 64 response messages (the first as before, the others as `response.stream` with their delay since the message before, rounded up to a latency bucket, never an exact time), each redacted like a unary message by the sidecar and again by the controller, within 256 KiB of JSON together. The steps are stored in `:grpc` only (`:report` and `:outbound` stay as 0.2.0-alpha.3 reads them) and are part of the unreleased `format` 2 egress set; `format` 1 leaves them out. A mock replays the first message, then each step after its recorded delay times the latency factor, then the trailers, within 300 s; a client cancel stops the replay and frees its timer and pending slot. Client and bidirectional streaming stay passed through.
- Mock mode replays unary gRPC calls: the call is matched on its path and first request message and answered with the recorded (redacted) response message, framed, with `grpc-status`, `grpc-message` and the recorded trailers (an error status without message has no frame); placeholders are replayed as strings, `-bin` trailers are not. The sidecar now pulls the egress set as `format=2` and falls back to `format=1` when the controller answers `400` (a controller up to 0.2.0-alpha.3), where gRPC calls still get the empty message; it asks for `format=2` again every 10 pulls (#463).
- The Endpoints tab lists the inbound and outbound endpoints of a picked deployment (`/ui/endpoints?deployment=<ns>/<dep>`), filtered in the browser by method, template, authority and status class, each row linking to the Swagger UI page of its document (#466).
- The Load tests tab lists the stored runs of every deployment (phase, reason, requests, RPS, p99) and opens a run page with the error kinds and one row per endpoint; it says that load testing is disabled when `loadTest.enabled` is false, read from the new `GET /v1/features` (#467).

### Fixed

- Every TCP socket of the sidecar (accepted, connected to the destination, pass-through) and of the controller's listeners and Valkey connections sets `TCP_NODELAY`: HTTP/2 through the sidecar no longer pays a ~40 ms Nagle and delayed-ACK stall per request (#480).
- The read-only YAML of the resource view escapes U+0085, U+2028 and U+2029 in strings and keys (YAML 1.1 line breaks); the UI router serves the resource view only under `/ui/workloads/<ns>/<dep>/resources/` and the document page only under `/openapi/` (#474).

### Documentation

- Wiki: new page "Mesh" (load balancing, scope, failure behaviour, retries, limits) and mesh sections in Configuration, Monitoring, Access logs, Troubleshooting and Security.
- Wiki "Reading the API": the Endpoints tab and its filters; annotation values of the resource view are shown as set by their authors (#466, #474).
- Wiki "Access logs": every field of the sidecar's access lines, and `jq` recipes for status classes, authorities, slow calls, mock results and load-marked requests (#437).

### Upgrade notes

- The gRPC messages and trailers are stored in a key of their own (`mimic:v1:{<ns>/<dep>}:grpc`); `:report` and `:outbound` keep the formats that 0.2.0-alpha.3 reads, so a rolling controller upgrade or a rollback keeps every workload served and only loses the gRPC data (the keys expire after `MIMIC_STORE_TTL`).

## [0.2.0-alpha.3] - 2026-11-11

### Features

- `kubectl get mimicloadtest` shows the p99 latency column as `P99 MICROS` instead of `P99 ΜS`: kubectl upper-cases column names, and the micro sign became a Greek capital Mu (#431).
- Wiki onboarding: a five-page "Get started" path (Install, Quick start, Mock mode, Load test, Reading the API) whose demo injects `front` and `users`, mocks `front` with `users` scaled to 0 and load-tests `front` alone; the load test read API, kind and install accuracy, and plain wording on the user pages.

### Accepted limits

Unchanged from 0.2.0-alpha.2 (see
[`docs/SPEC.md`](https://github.com/Thaliph/mock-server/blob/v0.2.0-alpha.3/docs/SPEC.md#111-accepted-limits)
and the wiki Security page).

### Upgrade notes

- Nothing to migrate from 0.2.0-alpha.2: upgrade with `helm upgrade --version 0.2.0-alpha.3`. The
  printer column rename only changes the output of `kubectl get mimicloadtest`.

## [0.2.0-alpha.2] - 2026-11-11

### Features

- **F13, readable placeholders (#424).** Redacted values are written as a typed placeholder
  `mimic-<type>` (`Bearer mimic-token`, `a=mimic-cookie`, `mimic-jwt`, 13 types in all) instead of
  `<redacted>`, chosen by the rule that fired, never by the value. Mock answers replay the
  placeholders, and a load test sends them by default: `credentials` replaces them with the values of
  a Secret, or the test environment accepts mimic's placeholders (wiki Load test, *Testing with
  credentials*). `<redacted>` remains only in the host of a redacted outbound authority.
- Load test reconciler and read API follow-ups of the F11 reviews (#420, via #425 and #426): the
  generator policy pins `fsGroup` and the credentials Secret mode, the final verdict re-reads the
  posts, outages are logged once per reason, and the e2e checks are tighter.
- The distroless base image moves to a rebuild with the fixed `zlib1g`, and the two Trivy exceptions
  are removed (#371, via #423).
- Dependency: `jsonschema` 0.59.0 to 0.60.2, which fixes a stack overflow on `$ref` cycles (#396).
- CI runs only the jobs a change needs, and `ci-ok` fails closed on any skip its gates did not
  allow (#428, via #429).

### Accepted limits

Unchanged from 0.2.0-alpha.1 (see
[`docs/SPEC.md`](https://github.com/Thaliph/mock-server/blob/v0.2.0-alpha.2/docs/SPEC.md#111-accepted-limits)
and the wiki Security page), plus one: a real value exactly equal to a placeholder (for example
`mimic-token`) is taken for a placeholder, so mock matching skips that field and a load test treats
it as a credential field. Placeholders are public constants, not secrets.

### Upgrade notes

- Data written by 0.2.0-alpha.1 holds `<redacted>`: it is upgraded on read (the field name gives the
  placeholder, else `mimic-secret`) and rewritten at the next push; nothing needs a migration, and
  untouched data expires with `MIMIC_STORE_TTL`.
- A load test without `credentials` now sends placeholders to the endpoints that were skipped as
  `credentials` before: they may answer `4xx` unless the test environment accepts them or a Secret
  is mapped.
- Rolling back to 0.2.0-alpha.1 treats placeholders as ordinary strings and degrades matching, without
  leaking anything (spec §11.1).

## [0.2.0-alpha.1] - 2026-11-11

First pre-release of mimic v2, a ground-up rewrite. It replaces the legacy `v0.1.0` product, which
shares nothing with it. Features are numbered as in the delivery plan (spec §12).

### Features

- **F0, foundations (#233).** Cargo workspace (`mimic-core` pure logic, `mimic-runtime`, thin
  binaries), `MIMIC_*` configuration that exits 2 on an invalid value, JSON logs, `/healthz`,
  `/readyz` and OpenMetrics `/metrics`, graceful `SIGTERM`, distroless non-root multi-arch images,
  the Helm chart with Valkey, a kind end-to-end suite and a CI supply-chain stage (cargo deny,
  zizmor, Trivy, CodeQL, gitleaks, dependency cooldown).
- **F1, injection (#234).** A mutating webhook adds `mimic-init` (traffic redirection) and a native
  sidecar to the pods selected by namespace label, pod label or `MimicWorkload`. Non-HTTP traffic is
  an opaque TCP tunnel. Probes are rewritten so their ports stay reachable.
- **F2, pass-through proxy (#235).** HTTP/1.x and HTTP/2 relayed byte for byte, inbound and
  outbound, with access logs and RED metrics.
- **F3, TLS interception (#236).** Outbound HTTPS is decrypted with a per-deployment CA that the pod
  trusts automatically (trust bundle injected per app image). Interception falls back to pass-through
  whenever it could change what the client or the server sees.
- **F4, ingress endpoint discovery (#237).** Inbound exchanges become endpoints: path templates,
  query, header and body schemas, volatile fields, latency histograms and samples, with credentials
  redacted before anything leaves the pod.
- **F5, collection (#238).** The controller merges the reports of all pods of a Deployment in Valkey,
  exposes them through a read API and writes the `MimicWorkload` status.
- **F6, discovery UI (#239).** An OpenAPI 3.1 document per deployment and port, and a Swagger UI, on
  the read API listener.
- **F7, egress recording (#240).** The sidecar records outbound calls per authority: endpoints,
  samples and latency histograms.
- **F8, egress aggregation (#241).** Sidecars share their egress with the controller, which merges it
  per deployment into outbound documents.
- **F9, egress endpoint (#242).** `GET /v1/workloads/<deployment>/egress` on the sidecar API gives
  every sidecar its deployment's egress set.
- **F10, mock mode (#243).** `spec.mode: mock` on a `MimicWorkload` answers outbound calls from the
  recorded egress instead of calling the real destination, and `discovery` switches back live, with
  convergence in the status.
- **F11, load test of a mocked deployment (#259).** The `MimicLoadTest` CRD (off by default,
  `loadTest.enabled`) runs an Indexed Job that replays the discovered inbound endpoints against the
  pods of one deployment, only while it is in mock, and reports requests, statuses, errors and
  p50/p90/p99/max per endpoint in the status, Valkey, metrics and the UI.
- **Monitoring (#378 to #385, #399, #406).** Optional ServiceMonitor and PodMonitors, the Valkey
  exporter with a read-only ACL user, a bundled Prometheus and Grafana for demos (both off by
  default, no Helm dependency), five provisioned dashboards (Controller, Sidecar traffic, Discovery,
  Mock mode and load tests, Valkey), and the process, limit, store, connection and mock metrics that
  they need.
- **F12, release (#364).** Versioned multi-arch images and chart, a changelog, release notes
  extracted from it, and a wiki that names the release it documents.

### Accepted limits

The full list is in the spec: https://github.com/Thaliph/mock-server/blob/v0.2.0-alpha.1/docs/SPEC.md#111-accepted-limits
and the main ones are summarised on the [wiki Security page](https://github.com/Thaliph/mock-server/wiki/Security).

- Capture is an observability and test tool, not a security boundary: an app container with
  `NET_ADMIN`, or whose image runs as UID 1337 without declaring it, escapes it. A user who can create
  pods in a namespace can pose as another Deployment of the same namespace; treat the namespace as the
  trust boundary.
- Roll back only out of mock: an F9 sidecar has no mock and would call the real destinations.
- Replay fidelity: a mock replays recorded samples chosen per request, with no state across calls and
  no sequence.
- Whoever may create or edit a `MimicWorkload` in a namespace decides the egress of every deployment
  there (mock wins).
- Redaction is a fixed list plus `spec.discovery.redact`; a secret under an unlisted name and in an
  undetected shape can be stored and replayed.
- Load tests: in-flight requests at a switch out of mock can reach real destinations (at most the
  run's concurrency); the app's real datastores and passthrough destinations are loaded too; load and
  credentials travel in cleartext between generator and pods; the generator NetworkPolicy needs a CNI
  that enforces it.
- Release: images are rebuilt from the tagged commit, arm64 is not run on kind (image checks and scan
  only), repository admins can bypass the tag ruleset, and signing depends on the public Sigstore
  service. The wiki is not versioned; `docs/wiki/` at the tag documents the release.

### Upgrade notes

- There is no upgrade from the legacy `v0.1.0`: uninstall it and install fresh.
- A pre-release chart is installed with an explicit version:
  `helm install mimic oci://ghcr.io/thaliph/charts/mimic --version 0.2.0-alpha.1`. Verify the images
  and the chart with `cosign verify` first (wiki Install page).
- Roll back only out of mock: switch every `MimicWorkload` back to `discovery` before a `helm rollback`
  to an earlier release, or pods recreated afterwards forward for real.
- The CRDs are `v1alpha1` and may change incompatibly before `0.2.0`.
- The CRDs and the `MimicWorkload` objects are kept on `helm uninstall`; delete them by hand. The
  Valkey PVC and password Secret are kept too.
- Pods injected before `loadTest.enabled` was turned on need a restart to take part in a load test
  (they must carry the deployment label and the load-aware sidecar). `loadTest.enabled` requires
  `image.digest`, which a released chart has.
- Running pods keep the sidecar version they were created with; `rollout restart` them after an
  upgrade.
