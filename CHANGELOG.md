# Changelog

All notable changes to mimic are documented in this file. The format follows
[Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/). The reference for behaviour is
[`docs/SPEC.md`](docs/SPEC.md).

## [Unreleased]

### Features

- `kubectl get mimicloadtest` shows the p99 latency column as `P99 MICROS` instead of `P99 ΜS`: kubectl upper-cases column names, and the micro sign became a Greek capital Mu (#431).

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
