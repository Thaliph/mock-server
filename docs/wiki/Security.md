# Security

The authoritative list of accepted limits is [section 11.1 of the spec](https://github.com/Thaliph/mock-server/blob/main/docs/SPEC.md#111-accepted-limits). To report a
vulnerability, see `SECURITY.md` in the repository.

## Threat model summary

mimic sits on the path of your application traffic and holds a CA that can impersonate hosts to the
pods it injects. The design goals:

- mimic must not change what the app or the server sees (pass-through proxy; TLS interception falls
  back to pass-through whenever in doubt);
- a secret must not reach a log, Valkey or an endpoint (redaction in the sidecar and again in the
  controller);
- keys stay in memory and in one Secret;
- the controller and sidecars have the smallest privileges that work.

Capture is an observability tool, **not a security boundary**: an app container with `NET_ADMIN`, or one
that runs as UID 1337, escapes it.

## What reaches Valkey

Only redacted reports (endpoints, schemas, counters, volatile-field hash sketches, redacted samples), per-pod
sequence numbers and pod names. Never opaque bodies, keys, the seed or tokens. The controller re-applies
redaction (built-in rules plus `spec.discovery.redact`) to every pushed report before it is stored or
logged, so an older or faulty sidecar cannot store what the controller would redact. Valkey requires
a password (random, in a Secret kept across upgrades) and accepts connections from the controller
only (NetworkPolicy). The read API serves stored reports on loopback only, via `kubectl port-forward`.
See [Discovery and redaction](Discovery-and-redaction).

## Privileges and images

| Container | Runs as | Notes |
|---|---|---|
| controller, Valkey, `mimic-sidecar`, `mimic-trust-<n>` | non-root (65532, 999, 1337, 1337) | read-only root filesystem, all capabilities dropped, `RuntimeDefault` seccomp, no privilege escalation; `mimic-trust` has no network |
| `mimic-init` | UID 0 | only `NET_ADMIN` and `NET_RAW`, no privilege escalation, read-only root, exits after installing the rules |

Every pod the chart creates is Pod Security `restricted`. Application namespaces must be `privileged`
for `mimic-init`. No mimic container mounts an app volume or the pod's API token. mimic's images are
distroless (no shell). Valkey is the pinned upstream Alpine image (`valkey/valkey:9.0-alpine`, which has
a shell). mimic's images are pinned by digest only when `image.digest` and `inject.initImage.digest`
are set; the default is the chart appVersion tag, so set both to pin them. CI scans images and checks
dependencies with `cargo deny`.

## RBAC

Controller (ClusterRole / Role):

| Resource | Verbs |
|---|---|
| `mimicworkloads` | `get`, `list`, `watch` |
| `mimicworkloads/status` | `patch` |
| `mutatingwebhookconfigurations`, only its own by name | `get`, `list`, `watch`, `patch` (a `ValidatingAdmissionPolicy` limits changes to `caBundle`) |
| `tokenreviews` | `create` |
| `services`, `endpointslices`, `deployments` (the controller reads metadata only; RBAC grants whole objects), `mimicconfigs`, `referencegrants` (when `mesh.enabled`, the default) | `get`, `list`, `watch` |
| `mimicconfigs/status` (when `mesh.enabled`) | `patch` (server-side apply, field manager `mimic-mesh`) |
| `httproutes`, `gateway.networking.k8s.io` (when `mesh.enabled`) | `get`, `list`, `watch` |
| `httproutes/status`, `gateway.networking.k8s.io` (when `mesh.enabled`) | `patch` (JSON merge patch, field manager `mimic-mesh`) |
| Secrets, only `<release>-webhook-tls` and `<release>-tls-ca` in its namespace | `get`, `update` |

No access to pods, ReplicaSets or other Secrets, no write on Deployments, no `create`, `update` or `delete` on any Gateway API object and nothing on `gateways`, `gatewayclasses` or `customresourcedefinitions`. RBAC cannot narrow the `httproutes/status` patch to mimic's entries: the controller changes only `status.parents[]` entries of `controllerName: mimic.platform.io/mesh`, but a compromised controller could rewrite the status (never the `spec`) of any route. The mesh view a sidecar fetches holds the Services of its own namespace and the destinations its `MimicConfig` declares in a namespace that consents with a `ReferenceGrant`; a pod reads its own deployment's view only, and no Secret, label or annotation reaches a view. Whoever can `exec`/`get` the two Secrets
or the controller pods can obtain the TLS seed: restrict that access. Whoever can use `pods/portforward`
in the release namespace can read every discovered report.

## Mesh

The [mesh](Mesh) is on by default (`mesh.enabled`); with `false` the controller serves no view, watches
nothing and the rules below are not rendered.

**What the controller reads:** the mesh rows of the [RBAC](#rbac) table above, cluster-wide and only with
`mesh.enabled`. RBAC cannot limit a read to metadata: the grant covers whole Deployments, and the controller
asks for their metadata only.

Nothing watched is writable. The ClusterRole has **no Secret rule**: Secrets stay in the namespaced Role,
limited by name to the two chart Secrets. The Gateway API CRD may be absent; the rule is then harmless,
and with no CRD nobody consents to a cross-namespace destination (fail closed).

Deployments: the controller asks the API server for their **metadata only**, but RBAC cannot limit a read
to metadata, so the grant covers whole Deployment objects in every namespace. The same cluster-wide read
applies to Services, EndpointSlices and MimicConfigs. A compromised controller could therefore read
every Deployment spec.

**View isolation.** `GET /v1/workloads/<deployment>/mesh` serves a pod only the view of its own
deployment. The pod's name comes from the `TokenReview` of its projected token (a token without a pod
identity, or whose pod does not belong to the deployment named in the path, gets `403`), and the
namespace comes from the token's service-account username, never from the path. The view cache is keyed
by namespace first, so no view crosses namespaces. Pod identity is a name pattern on the token-bound pod
name; it cannot cross namespaces.

**Watch caps.** A sidecar holds its request open until the view changes. To keep one pod or one
namespace from exhausting the budget for the others: at most `MIMIC_SIDECAR_API_MAX_WATCHES_PER_POD`
(2) held requests per pod, at most `MIMIC_SIDECAR_API_WATCH_NAMESPACE_PERCENT` (25 %) of
`MIMIC_SIDECAR_API_MAX_WATCHES` (10000 per replica) per namespace; over a cap the request gets
`503 watch_limit` at once. Held requests have a budget of their own, so pushes and TLS issuance are
served even when it is full. See [Configuration](Configuration).

**Cross-namespace destinations.** A destination in another namespace is in the view only if a
`ReferenceGrant` in the destination namespace allows it: `from` = (`mimic.platform.io`, `MimicConfig`,
the caller's namespace), `to` = (core group, `Service`, the name or none). Deleting the grant removes the
destination from the view; the `MimicConfig` shows `ResolvedRefs: False`, reason `RefNotPermitted`.

**Routes.** An `HTTPRoute` is configuration, not a Secret: its header and query values are copied into the views
and state files of every pod it applies to (a producer route: every namespace that has the Service in its view),
so **never put a credential in a `RequestHeaderModifier`**. A backend or mirror in another namespace is in a view
only with a `ReferenceGrant` `from` `HTTPRoute` in its namespace; a consumer route changes the calls of its own
namespace only; routes cannot set `host`, framing fields or `x-mimic-*`. No route value is in a log line or a
status message. The status of a route says whether its parent Service exists, which cluster DNS already tells any
pod. Status entries mimic wrote stay after `helm uninstall`. See [Mesh](Mesh#routes).

**TLS re-origination.** A call the sidecar intercepts is re-originated over TLS to the chosen pod, and
the pod's certificate is verified against the Service name (the SNI the app sent, else
`<name>.<namespace>.svc`), never the pod IP. Hostname verification cannot be turned off and a pod is
never spoken to in cleartext. See [TLS interception](TLS-interception).

**Retries and the binding.** A refusal (`503`, `x-mimic-error: overloaded` or `queue-timeout`) makes the
caller's sidecar retry only on a cleartext try to a pod, and only when `x-mimic-endpoint` names that pod;
never over TLS, never the ClusterIP. A meshed inbound sidecar removes every `x-mimic-*` field from its app's
responses, so an app cannot forge a signal for its callers through its own sidecar, and a forwarding app cannot
make its callers retry. Buffers are bounded: `MIMIC_MESH_RETRY_BUFFER` per request, `MIMIC_MESH_RETRY_BUFFER_TOTAL`
for all requests, the inbound held head 64 KiB, a discarded refusal body 64 KiB. No header value reaches a
metric label or a log field. See [Mesh](Mesh#retries).

## Network

- **Webhook (8443):** open to any source by default, because the API server's address depends on the
  cluster; narrow with `networkPolicy.webhookFrom`. It does not authenticate the API server, has no side
  effect, and bounds clients with `MIMIC_WEBHOOK_MAX_CONNECTIONS` and `MIMIC_WEBHOOK_REQUEST_TIMEOUT`.
  `failurePolicy: Fail`: if it is down, pod creation in scope fails.
- **Sidecar API (8444):** HTTPS with the webhook certificate; callers authenticate with a projected
  service-account token of audience `mimic.platform.io` (useless against the Kubernetes API), validated
  by `TokenReview`. Open to any source by default (injected pods can be anywhere); narrow with
  `networkPolicy.sidecarApiFrom`. Per-source rate limit and `TokenReview` limits protect the API server.
- **Admin (8081):** release namespace plus `networkPolicy.metricsNamespaceSelector`.
- **Read API:** bound to loopback, no Service, no NetworkPolicy port. A request with a non-loopback
  peer or a foreign `Host` gets `403`.
- **Valkey:** ingress from the controller only, no egress.
- Sidecar `/discovery` and the TLS-intermediate endpoint are loopback-only and token-authenticated
  respectively.

NetworkPolicies need a CNI that enforces them.

## Accepted limits

The main ones (full list: [the spec](https://github.com/Thaliph/mock-server/blob/main/docs/SPEC.md#111-accepted-limits)):

- **Workload identity is not strongly bound.** A user who can create pods in a namespace can obtain a
  sidecar named after another Deployment of the *same namespace*, push reports into it (within store
  bounds) and get an intermediate CA for it. Other namespaces are out of reach. Treat the namespace as
  the trust boundary.
- A compromised controller, or anyone reading the seed Secret, can impersonate any host to every
  injected pod.
- Redaction is a fixed list plus `spec.discovery.redact`: a secret under an unlisted name **and** in an
  unrecognised form is kept in samples. Non-credential personal data is kept by design.
- The volatile-field sketch holds hashes of values; a low-entropy value left in one could be recovered
  by brute force.
- The AOF file on the PVC is not encrypted by mimic.
- A deleted pod's token works up to 60 s longer (TokenReview cache).
- A user who can create pods can fill the per-namespace share of Valkey (`MIMIC_STORE_MAX_DEPLOYMENTS`);
  once Valkey is full, pushes of every namespace get `503`.
- A pod killed without `SIGTERM` loses its unshipped report.
- TLS: the first connection of a client that pins or ships its own roots fails; ECH, STARTTLS and
  TLS-in-`CONNECT` are not intercepted. See [TLS interception](TLS-interception).
- `exec` probes are left as they are; while the sidecar restarts, rewritten app probes can fail.
- Only the `iptables-nft` backend is supported; another tool that installs iptables rules in the same
  pod is not.
- GitOps tools must ignore the `caBundle` of the webhook configuration and the data of the two Secrets.
- Emptying the seed Secret rotates every root CA; running sidecars pass TLS through until recreated.
- **Mesh retries.** An app whose sidecar does not remove `x-mimic-*` (no sidecar, mesh off, inbound TLS) can
  answer a forged refusal bound to its own address: its callers send that request to its sibling pods, at
  most 3 more times, and only requests sent to that pod. A `GET`, `HEAD`, `OPTIONS`, `TRACE`, `PUT` or
  `DELETE` dropped without an answer is sent to another pod, so an app can see one request twice; until the
  retry budget (#446) a reset that every tier propagates can cost 4 tries per tier. A response the sidecar
  cannot frame closes the inbound connection while the mesh is on. See [Mesh](Mesh#limits).
- **Capture is not a security boundary**: an app container with `NET_ADMIN`, or whose image runs as
  UID 1337 without declaring it, escapes the redirection.
- **Mock.** Whoever may create or edit a `MimicWorkload` in a namespace decides the egress of every
  Deployment there. A mock replays samples with no state across calls. Roll back mimic only out of mock.
- **Load tests.** Requests in flight at a switch out of mock can reach real destinations; the
  application's real datastores are loaded too; load and credentials travel in cleartext. See
  [Load test](Load-test#limits).
- **Release.** Images are rebuilt from the tagged commit; arm64 gets image checks and a scan but no kind
  e2e; repository admins can bypass the tag ruleset (a signature names its tag); signing depends on the public
  Sigstore service. This wiki is not versioned: `docs/wiki/` at a release tag documents that release.
