# Mimic — specification

> Status: **v2.0-draft**, rewritten from scratch on 2026-10-31. The v0.x codebase is kept under
> the git tag `legacy-v0.37` for reference only. This file is the source of truth: any change of
> behaviour is written here first, then tested, then implemented.

## 1. Purpose

Mimic is a Kubernetes sidecar and its control plane. It serves two goals for an infrastructure
SRE, without any change to application code:

1. **Document** the HTTP APIs that run on a cluster: every injected deployment gets an
   OpenAPI 3.1 description of the endpoints it exposes (ingress) and of the external APIs it
   calls (egress), built from real traffic.
2. **Load-test one workload in isolation**: in *mock* mode, the deployment's egress calls are
   answered by the sidecar from recorded responses, with recorded latencies, so a payment
   service can be pushed to saturation without calling its provider (e.g. Adyen) and without
   being rate-limited.

### 1.1 In scope (v1)

- HTTP/1.1, HTTP/2 and unary gRPC, cleartext or TLS, inbound and outbound.
- Discovery mode (record) and mock mode, switched per deployment.
- Access logs, detailed component logs, Prometheus RED metrics for ingress and egress.
- A read-only UI per deployment (OpenAPI + Swagger UI).

### 1.2 Out of scope (v1)

- Traces (no OpenTelemetry spans).
- Traffic management (retries, routing, fault injection beyond mock latency) and mesh security
  (mTLS between workloads, authorization policies): candidate v2, "mesh" features.
- Recording or mocking WebSocket, SSE, gRPC streaming or non-HTTP TCP: passed through, logged
  and measured only.
- Fail-open when the sidecar is down (would need a privileged node agent): v2.
- Kubernetes older than 1.33 (native sidecars are required).

## 2. Architecture

```
            ┌──────────────────────── pod (injected) ────────────────────────┐
 client ──▶ │ iptables ─▶ mimic-sidecar ─▶ app ─▶ iptables ─▶ mimic-sidecar ─┼─▶ provider
            │  (inbound)   discovery: record + forward      (outbound)        │   (or mock)
            │              local cache (endpoints, egress samples)            │
            └───────────────────────────────┬────────────────────────────────┘
                                            │ HTTP/JSON (push recordings, pull mode + egress)
                                  ┌─────────▼──────────┐       ┌────────┐
                                  │  mimic-controller  │──────▶│ Valkey │
                                  │ webhook · API · UI │       └────────┘
                                  └────────────────────┘
```

| Component | Kind | Role |
|---|---|---|
| `mimic-init` | first init container, exits | Installs the iptables redirection in the pod network namespace (§4). Runs as UID 0 with only `NET_ADMIN` and `NET_RAW`, own image `ghcr.io/thaliph/mimic-init`. |
| `mimic-sidecar` | native sidecar (second init container, `restartPolicy: Always`) | Transparent proxy for inbound and outbound traffic; records, measures, mocks. UID 1337, no capability. |
| `mimic-controller` | Deployment in `mimic-system` | Mutating webhook (injection), `MimicWorkload` reconciler, sidecar API, UI. Stateless: all state is in Valkey. |
| Valkey | StatefulSet in `mimic-system` | Shared store: merged endpoints and egress recordings per deployment. AOF persistence on a PVC. |

Crates: `mimic-core` (pure logic: no tokio, no I/O, no clock), `mimic-runtime` (shared process
plumbing: configuration, logs, metrics, health, shutdown), `mimic-sidecar`, `mimic-controller`,
and from F1 `mimic-init`. `mimic-core` may depend on pure data crates (`serde`, `serde_json`,
`k8s-openapi`).

## 3. Modes

| Mode | Inbound | Outbound |
|---|---|---|
| `discovery` (default) | forwarded to the app, recorded | forwarded to the destination, recorded |
| `mock` | forwarded to the app, recorded | answered from recordings; never forwarded (§8) |

The mode is set per deployment on its `MimicWorkload` (`spec.mode`). Every pod of the deployment
follows it, including pods created later. Entering mock: the sidecar pulls the deployment's
egress recordings from the controller, then switches. Leaving mock: the sidecar switches back to
discovery immediately.

## 4. Traffic capture

- Transparent redirection with iptables (`nat` table, `iptables-nft` backend) set by
  `mimic-init` through `iptables-restore` and `ip6tables-restore`, for IPv4 and IPv6:
  - inbound: TCP whose destination is a local address of the pod is redirected to the sidecar
    inbound port **15006**, except the excluded inbound ports;
  - outbound: TCP leaving the pod is redirected to the sidecar outbound port **15001**, except
    traffic from the sidecar UID **1337** and traffic on the loopback interface;
  - `filter` table: outbound UDP to port 443 is rejected (ICMP port unreachable) so that HTTP/3
    clients fall back to TCP. Other UDP (DNS included) is untouched.
  - IPv6 rules are installed only when the pod has a non-loopback IPv6 address (`::1` alone
    means no IPv6). Running `mimic-init` again, also after a partial failure, leaves exactly
    one jump from each built-in chain (idempotent): `mimic-init` owns the built-in chains it
    hooks (`nat` `PREROUTING` and `OUTPUT`, `filter` `OUTPUT`) in the pod's network namespace
    and flushes them before adding its jump. It runs first in a fresh namespace; another
    traffic-capturing tool in the same pod is not supported (§11.1).
- **HTTP probes go through the sidecar** (as Istio does). The webhook rewrites every `httpGet`
  liveness, readiness and startup probe of the app containers to `httpGet` on the sidecar admin
  port 15100, path `/app-health/<container>/<livez|readyz|startupz>`, scheme HTTP, and keeps
  the probe's timings and thresholds. It gives the original probes to the sidecar in
  `MIMIC_APP_PROBES`. The sidecar sends the original request (scheme, host, defaulting to the
  pod IP, port with named ports resolved, path, headers), from UID 1337 so that it is not
  captured. It does not verify TLS and does not follow redirects, like the kubelet. It answers
  with the app's status code, or 503 when the app cannot be reached within the probe's
  `timeoutSeconds`. Only the configured probes can be called: any other `/app-health/` path
  gets 404. Probe requests are not recorded.
- Excluded inbound ports: the ports of the `tcpSocket` and `grpc` probes of the app containers
  (named ports resolved), which the sidecar would otherwise answer for an app that is down, and
  the sidecar admin port 15100. The webhook reads them from the pod spec. API traffic on an
  `httpGet` probe port is captured.
- When the pod has no IPv6 (`/proc/net/if_inet6` absent or empty), the IPv6 rules and listeners
  are skipped with an INFO log; any other failure is fatal.
- The sidecar listens on `0.0.0.0:15006` and `[::]:15006` (inbound, IPv6 socket `V6ONLY`) and
  on loopback only for outbound (`127.0.0.1:15001`, `[::1]:15001`), reads the original
  destination (`SO_ORIGINAL_DST`) and connects to it from UID 1337. A connection whose original destination is missing or is a
  sidecar listener is closed (loop protection). The sidecar is ready only once its listeners are
  bound, and the app containers start only after that (native sidecar startup probe).
- Protocol detection per connection: TLS (ClientHello), HTTP/1.1, HTTP/2 prior knowledge;
  anything else is an opaque TCP tunnel (logged and measured). F1 provides the opaque tunnel
  for everything; detection comes in F2.
- When the sidecar is down, redirected connections are refused: capture is **fail-closed**
  (fail-open is v2, §1.2).

## 5. TLS interception

- Every outbound TLS connection is intercepted. The sidecar terminates TLS with a certificate
  issued on the fly for the requested SNI, then opens its own TLS connection to the destination,
  verified against the system trust store.
- **One CA per deployment**, generated by the controller and stored in a Secret of
  `mimic-system` readable only by the controller. The sidecar receives the CA key at start
  (authenticated sidecar API) and keeps it in memory only. Leaf certificates are short-lived.
- **The app trusts the CA without code change**: the webhook mounts a bundle (the image's own
  system CAs + the mimic CA) and sets the usual variables: `SSL_CERT_FILE` (OpenSSL, Go),
  `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE` (Python), `NODE_EXTRA_CA_CERTS` (Node.js) and a PKCS12
  truststore through `JAVA_TOOL_OPTIONS` (JVM). Existing internal CAs keep working.
- **Fallback**: when a client rejects the forged certificate (pinning, custom truststore), the
  sidecar passes that destination through untouched from then on, logs a WARN once and counts it.
- **Exclusions**: `spec.tls.exclude` on the `MimicWorkload` lists hosts never intercepted
  (internal services, mTLS to a provider).

## 6. Discovery

- An *endpoint* is identified by direction, authority (host), method and path template
  (`/users/{id}`). Query keys, header names and body schemas are inferred and merged.
- Per endpoint, the sidecar keeps: request/response counters by status, a latency histogram,
  and up to **20 samples** (each distinct status code and each distinct response shape first),
  bodies capped at **256 KiB**. Samples expire after **30 days** in Valkey.
- Volatile fields (values that change on every call: idempotency keys, timestamps, ids) are
  detected and marked; mock matching ignores them.
- **Redaction** happens in the sidecar, before anything leaves process memory: secrets in
  headers, query, path segments and bodies are replaced by stable markers.
- Under overload the sidecar stops recording (and counts the dropped exchanges) but keeps
  forwarding traffic.

## 7. Controller and storage

- `MimicWorkload` (namespaced CRD, `mimic.platform.io/v1alpha1`): selects the deployment named
  by `spec.deployment` (required, DNS-1123 name, immutable) in its namespace, holds
  `spec.mode` (`discovery` | `mock`, default `discovery`), `spec.tls.exclude`, latency settings; `status` reports pods, mode convergence and
  counts. F1 introduces the CRD with `spec.deployment` and `spec.mode` only; other fields arrive
  with their feature. When several `MimicWorkload`s name the same deployment, any of them
  selects it for injection; which one sets the mode is decided in F10.
- Sidecars push their local recordings periodically and pull the deployment's mode and, in mock
  mode, its aggregated egress recordings. Valkey keys are namespaced by
  `<namespace>/<deployment>`.
- The controller merges endpoints of all pods of a deployment (same endpoint key → merged
  counters, histograms, schemas, sample sets).
- The UI and its API are read-only and served on the controller's HTTP port, reached by
  `kubectl port-forward` (Kubernetes RBAC is the authentication).

### 7.1 Injection

- The controller serves a mutating admission webhook (HTTPS, port 8443) for pod `CREATE`.
  Kubernetes ≥ 1.33 (native sidecars); the chart refuses older clusters (`kubeVersion`).
- **Selection** (label key `mimic.platform.io/inject`):

  | Namespace label | Pod label | Injected |
  |---|---|---|
  | any | `disabled` | never |
  | `enabled` | absent or `enabled` | yes |
  | `selected` | absent | only if a `MimicWorkload` in the namespace has `spec.deployment` equal to the pod's deployment |
  | any | `enabled` | yes |
  | absent | absent | no (the webhook is not even called) |

  The namespaces `kube-system`, `kube-public`, `kube-node-lease` and the controller's own
  namespace are never injected. The `MutatingWebhookConfiguration` has three webhooks, one per
  path, so the API server only calls the controller for pods in scope (namespace and object
  selectors, no namespace read by the controller): `/inject/namespace` (namespace `enabled`),
  `/inject/workload` (namespace `selected`), `/inject/pod` (pod `enabled`). A pod matched twice
  is handled once (idempotence below). Only pods of a Deployment are injected (owner
  ReplicaSet named `<deployment>-<pod-template-hash>`); the deployment name is the workload
  identity.
- **Skipped** pods are admitted unchanged except for the annotation
  `mimic.platform.io/skipped: <reason>`: `host-network`, `unsupported-owner`, `port-conflict`
  (an app container declares port 15001, 15006 or 15100), `uid-conflict` (an app container or
  the pod sets `runAsUser: 1337`), `name-conflict` (an app container is named `mimic-init` or
  `mimic-sidecar`, or an init container is named `mimic-init` in a pod without a
  `mimic-sidecar` init container). `unsupported-owner` applies only when injection was asked by
  an `enabled` label (pod or namespace); in a `selected` namespace, an unlabelled pod not owned
  by a Deployment is not selected and stays untouched, without annotation. A pod that already has a `mimic-sidecar` init container is
  admitted unchanged (idempotent, `reinvocationPolicy: IfNeeded`).
- **Injected** pods get, in front of their own init containers: `mimic-init`, then
  `mimic-sidecar` (`restartPolicy: Always`, startup and liveness probes on its admin port), and
  the annotation `mimic.platform.io/injected: <version>`. Neither container mounts a service
  account token or any app volume. In the app's containers, only the `httpGet` probes are
  changed (§4); nothing else is modified. Every literal `env` value the patch writes has each
  `$` doubled, so the kubelet's `$(VAR)` expansion gives the container the value unchanged.
- The webhook uses `failurePolicy: Fail`: while the controller is unavailable, pod creation in
  the selected scope fails (fail-closed, consistent with mock mode never calling the real
  destination). Pods outside the scope are never sent to the webhook.
- Injection happens at pod creation only: labelling a namespace or creating a `MimicWorkload`
  does not restart running pods; `kubectl rollout restart` does.
- **Webhook certificate, self-managed**: the controller keeps a CA (10 years) and a serving
  certificate (1 year, SAN `<service>.<namespace>.svc`) in the Secret `<release>-webhook-tls`
  (created empty by the chart), writes the CA into the `caBundle` of its
  `MutatingWebhookConfiguration` (watched: a `caBundle` changed by anyone else is restored
  within 10 s), and renews the serving certificate when less than 30 days
  remain, without changing the CA. Concurrent replicas converge on one certificate (optimistic
  concurrency on the Secret). A `ValidatingAdmissionPolicy` lets the controller's service
  account change nothing in that configuration except `caBundle`.
- **Webhook availability** (it fails closed, so a stalled webhook stalls pod creation in scope):
  the chart runs 2 controller replicas by default, spread across nodes (preferred pod
  anti-affinity), with a PodDisruptionBudget `maxUnavailable: 1`. The webhook listener serves
  at most `MIMIC_WEBHOOK_MAX_CONNECTIONS` connections at once (further ones wait in the accept
  backlog); a connection must complete its TLS handshake, and then each request (headers and
  body), within `MIMIC_WEBHOOK_REQUEST_TIMEOUT`, or it is closed; an idle keep-alive connection
  is closed after 60 s. A NetworkPolicy admits ingress to the controller only on the webhook
  port (from any source by default, since the API server's source address depends on the
  cluster; `networkPolicy.webhookFrom` narrows it) and on the admin port (from the release
  namespace and the namespaces selected by `networkPolicy.metricsNamespaceSelector`).
- The CRD carries `helm.sh/resource-policy: keep`: `helm uninstall` leaves the CRD and every
  `MimicWorkload`; removing them is explicit (`kubectl delete crd
  mimicworkloads.mimic.platform.io`). A later install with the same release name and namespace
  adopts the kept CRD.

## 8. Mock

- Matching, first hit wins: (1) exact: method, authority, path, query, normalised body;
  (2) same endpoint, closest body ignoring volatile fields; (3) most frequent response of the
  endpoint. The response carries `x-mimic-match: exact|endpoint|fallback`.
- Latency: drawn from the endpoint's recorded histogram; `spec.mock.latencyFactor` scales it
  (`0` disables it).
- No match: status **599**, header `x-mimic-miss: 1`, metric `mimic_mock_miss_total`, one log
  line with the request signature. The real destination is never called.

## 9. Observability

- **Logs**: JSON lines on stdout, one object per event, with at least `timestamp`, `level`,
  `target` and `message`. Access logs (one line per exchange, `target` = `access`) and
  component logs. Level from `MIMIC_LOG` (`tracing` filter syntax, default `info`).
- **Metrics**: OpenMetrics text format (Prometheus-compatible) on the admin port at `/metrics`. Every component exposes
  `mimic_build_info{component,version} 1`. RED metrics per direction, authority and status class
  are defined in F2.
- **Health**: `/healthz` (process alive) and `/readyz` (ready to serve) on the admin port.
- No traces.

## 10. Configuration and runtime (12-factor)

- Configuration comes **only** from environment variables prefixed `MIMIC_`. An invalid value
  stops the process at start with exit code **2** and a message naming the variable.
- Processes are stateless (state is in Valkey or in memory), log to stdout, stop gracefully on
  `SIGTERM`, and bind their ports from configuration.
- One image `ghcr.io/thaliph/mimic` (distroless, non-root UID 65532, no shell) holds
  `mimic-sidecar` and `mimic-controller` (the webhook runs the sidecar as UID 1337).
  `ghcr.io/thaliph/mimic-init` (distroless, no shell) holds only `mimic-init`, `iptables-restore`
  and `ip6tables-restore` (nft backend) and their libraries.
- `mimic-init` has no admin listener (it exits): FND-02 and FND-03 do not apply to it.
- Installation through the Helm chart `oci://ghcr.io/thaliph/charts/mimic`.
- Release artefacts are multi-arch (amd64, arm64), signed with cosign (keyless), with SBOM and
  provenance.

### 10.1 Common variables

| Variable | Default | Meaning |
|---|---|---|
| `MIMIC_LOG` | `info` | Log filter (`tracing` syntax). |
| `MIMIC_ADMIN_ADDR` | sidecar `0.0.0.0:15100`, controller `0.0.0.0:8081` | Admin listener: `/healthz`, `/readyz`, `/metrics`. |
| `MIMIC_SHUTDOWN_GRACE` | `10s` | Maximum drain time after `SIGTERM` (`<n>s` or `<n>ms`). |

### 10.2 Controller variables

| Variable | Default | Meaning |
|---|---|---|
| `MIMIC_VALKEY_URL` | required | `redis://host:port[/db]`, without credentials. |
| `MIMIC_VALKEY_PASSWORD` | empty | Valkey password (from a Secret). |
| `MIMIC_NAMESPACE` | required | Controller namespace (downward API). |
| `MIMIC_WEBHOOK_ADDR` | `0.0.0.0:8443` | Webhook HTTPS listener. |
| `MIMIC_WEBHOOK_SERVICE` | required | Service name of the webhook (certificate SAN). |
| `MIMIC_WEBHOOK_SECRET` | required | Secret holding the webhook CA and serving certificate. |
| `MIMIC_WEBHOOK_MAX_CONNECTIONS` | `256` | Concurrent webhook connections served (1–65535). |
| `MIMIC_WEBHOOK_REQUEST_TIMEOUT` | `5s` | TLS handshake and per-request read deadline (`<n>s` or `<n>ms`, > 0). |
| `MIMIC_WEBHOOK_CONFIGURATION` | required | Name of the `MutatingWebhookConfiguration` whose `caBundle` the controller maintains. |
| `MIMIC_INJECT_SIDECAR_IMAGE` | required | Image of `mimic-sidecar` in injected pods (digest recommended); the shared `mimic` image, so the injected container sets `command: [/usr/local/bin/mimic-sidecar]`. |
| `MIMIC_INJECT_INIT_IMAGE` | required | Image of `mimic-init` in injected pods. |
| `MIMIC_INJECT_SIDECAR_RESOURCES` | `{"requests":{"cpu":"50m","memory":"64Mi"},"limits":{"memory":"128Mi"}}` | Sidecar `resources`, JSON. |
| `MIMIC_INJECT_SIDECAR_LOG` | `info` | `MIMIC_LOG` given to injected sidecars. |

### 10.3 Sidecar variables

Set by the webhook; the defaults are the values it sets.

| Variable | Default | Meaning |
|---|---|---|
| `MIMIC_INBOUND_ADDRS` | `0.0.0.0:15006,[::]:15006` | Inbound listeners. |
| `MIMIC_OUTBOUND_ADDRS` | `127.0.0.1:15001,[::1]:15001` | Outbound listeners, loopback only. |
| `MIMIC_WORKLOAD` | required | Deployment name. |
| `MIMIC_POD_NAMESPACE`, `MIMIC_POD_NAME`, `MIMIC_POD_IP` | required | Downward API. |
| `MIMIC_APP_PROBES` | `{}` | Original `httpGet` probes, JSON object keyed by `<container>/<livez\|readyz\|startupz>`. |

### 10.4 `mimic-init` variables

| Variable | Default | Meaning |
|---|---|---|
| `MIMIC_INBOUND_PORT` | `15006` | Redirection target for inbound TCP. |
| `MIMIC_OUTBOUND_PORT` | `15001` | Redirection target for outbound TCP. |
| `MIMIC_PROXY_UID` | `1337` | UID whose traffic is not redirected. |
| `MIMIC_EXCLUDE_INBOUND_PORTS` | empty | Comma-separated ports (1–65535) never redirected inbound. |

## 11. Security baseline

- Every pod mimic owns runs with `runAsNonRoot`, `readOnlyRootFilesystem`, all capabilities
  dropped, `seccompProfile: RuntimeDefault`, no service account token unless needed (the
  controller needs one from F1). The only exception is `mimic-init`: UID 0 (iptables needs
  effective capabilities, which a non-root process with `no_new_privs` cannot get), only
  `NET_ADMIN` and `NET_RAW`, `allowPrivilegeEscalation: false`, read-only root filesystem,
  `RuntimeDefault` seccomp, exits once the rules are installed.
- The controller's RBAC is minimal: `get`/`list`/`watch` on `mimicworkloads`; `get`/`list`/`watch`/`patch`
  on its own `MutatingWebhookConfiguration` only (by name, and restricted to `caBundle` by a
  `ValidatingAdmissionPolicy`); `get`/`update` on its webhook Secret only (by name, in its
  namespace). No access to pods, Deployments or other Secrets.
- The webhook never logs or stores the pod spec or the patch (env values may hold secrets).
- Images are distroless, pinned by digest, scanned in CI (no fixable HIGH/CRITICAL).
- Dependencies checked by `cargo deny` (advisories, licences, sources).
- No sensitive value reaches Valkey or a log without redaction (§6).

### 11.1 Accepted limits

- Capture is an observability and test tool, not a security boundary: an app container with
  `NET_ADMIN`, or whose image runs as UID 1337 without declaring it in the pod spec, escapes it.
- All traffic to a `tcpSocket` or `grpc` probe port bypasses the sidecar (not only probes): an
  API served on such a port is not captured. `exec` probes are left as they are.
- Rewritten probes depend on the sidecar: while it restarts, the app's liveness probe can fail
  and restart the app if the outage outlasts `failureThreshold` × `periodSeconds`. The
  `/app-health/` endpoints sit on the admin port, so the network can reach them. They only run
  the pod's own probes.
- Loopback traffic (an app calling itself) and non-TCP traffic other than UDP 443 are not
  captured.
- Namespaces whose Pod Security level is `baseline` or `restricted` reject injected pods
  (`mimic-init` needs `NET_ADMIN`/`NET_RAW`); injection needs the `privileged` level. The
  rejection is visible as a `FailedCreate` event on the ReplicaSet.
- The webhook does not authenticate the API server (any client that reaches port 8443 can call
  it); it has no side effect and only computes a patch. Such a client can hold at most
  `MIMIC_WEBHOOK_MAX_CONNECTIONS` connections, each for at most `MIMIC_WEBHOOK_REQUEST_TIMEOUT`
  without completing a request, which bounds how long it can delay admission.
- The workload identity comes from the pod's `ownerReferences`, which the pod's creator
  controls: a user of the namespace can obtain a sidecar named after another deployment of the
  same namespace. Sidecar identity is bound to the real owner when sidecars authenticate (F5).
- The `caBundle` of the webhook configuration and the data of the webhook Secret are owned by
  the controller: GitOps tools must ignore those fields.
- Only the `iptables-nft` backend is supported (the node kernel needs `nf_tables`).
- `mimic-init` flushes the built-in chains it hooks in the pod's network namespace: another
  tool that installs iptables rules in the same pod (another mesh) is not supported.

## 12. Delivery plan

One GitHub issue and one PR (or a short stack) per feature. Acceptance criteria are written in
this section by the architect at the start of each feature, then implemented test-first; test
names contain the criterion ID (`fnd_02_…`).

| Feature | Content | User plan point |
|---|---|---|
| F0 | Foundations: workspace, runtime plumbing, image, Helm chart with Valkey, kind e2e, CI/CD, agent team | — |
| F1 | Injection: webhook, native sidecar, `mimic-init`, selection by label/CRD, opaque TCP tunnel | 1 |
| F2 | HTTP-aware pass-through proxy, access logs, RED metrics, inbound and outbound | 2 |
| F3 | TLS interception, per-deployment CA, trust injection, fallback | — |
| F4 | Ingress endpoint discovery: templating, schemas, redaction | 3 |
| F5 | Controller collects endpoints into Valkey, merges per deployment | 4, 5 |
| F6 | UI: OpenAPI 3.1 + Swagger UI per deployment | 6 |
| F7 | Egress recording in the sidecar: samples, latency histograms | 7 |
| F8 | Egress shared with the controller, aggregated per deployment | 8, 9 |
| F9 | Egress endpoint for sidecars | 10 |
| F10 | Mock mode, and back to discovery | 11, 12 |

### 12.1 F0 — Foundations

| ID | Criterion |
|---|---|
| FND-01 | A `MIMIC_*` variable with an invalid value stops the process before it listens, with exit code 2 and a stderr message that names the variable and the value. A missing required variable does the same. |
| FND-02 | `/healthz` answers 200 as long as the process runs. `/readyz` answers 200 when the component is ready, 503 otherwise. |
| FND-03 | `/metrics` answers 200 in OpenMetrics text format (scraped natively by Prometheus) and contains `mimic_build_info{component="<component>",version="<crate version>"} 1`. |
| FND-04 | Every line written on stdout is one JSON object with `timestamp`, `level`, `target` and `message`. `MIMIC_LOG` filters it. |
| FND-05 | On `SIGTERM` the process stops accepting connections, lets in-flight requests finish, and exits 0 within `MIMIC_SHUTDOWN_GRACE`; it logs the start and the end of the shutdown. |
| FND-06 | The controller is ready only when Valkey answers `PING` (with the configured password); it becomes unready, without exiting, when Valkey is unreachable, and ready again when it comes back. |
| FND-07 | The image runs as UID 65532, contains no shell, and both binaries answer `--version` with the crate version. |
| FND-08 | `helm lint` passes; every rendered pod is Pod Security `restricted` (non-root, read-only root filesystem, no capability, `RuntimeDefault` seccomp, no privilege escalation). Valkey requires a password generated at install and kept on upgrade, and only accepts connections from the controller (NetworkPolicy; needs a CNI that enforces NetworkPolicy, as kind's default does). |
| FND-09 | On kind (Kubernetes ≥ 1.33), `helm install` brings the controller and Valkey to Ready, and the controller's `/readyz` and `/metrics` answer through a port-forward. |
| FND-10 | CI runs as dependent stages: lint → unit and integration tests → image build and scan → kind e2e, plus a security stage (`cargo deny`, `zizmor`); a single `ci-ok` check sums them up. A `v*` tag publishes multi-arch signed images with SBOM and provenance, and the signed Helm chart. |

### 12.2 F1 — Injection

Levels: **U** unit in `mimic-core`, **I** integration in `crates/*/tests`, **E** kind e2e in
`scripts/e2e.sh`, **C** CI pipeline.

| ID | Criterion |
|---|---|
| INJ-01 | **U.** The selection decision follows the table of §7.1 for every combination of namespace label (`enabled`, `selected`, absent), pod label (`enabled`, `disabled`, absent) and presence of a `MimicWorkload` whose `spec.deployment` is the pod's deployment; the excluded namespaces are never injected. The deployment name is derived from the controlling ReplicaSet owner and the `pod-template-hash` label; any other owner gives `unsupported-owner`. |
| INJ-02 | **U.** A pod with `hostNetwork: true`, a container port 15001, 15006 or 15100, or `runAsUser: 1337` (pod or app container) is skipped with the matching reason; the patch only adds `mimic.platform.io/skipped: <reason>`. A pod that already has a `mimic-sidecar` init container gets an empty patch. |
| INJ-03 | **U.** The injection patch is a JSON Patch, byte-identical for the same input, that puts `mimic-init` then `mimic-sidecar` before the pod's own init containers (also when it has none), rewrites the app containers' `httpGet` probes as in §4 and changes nothing else in them, and adds `mimic.platform.io/injected: <version>`. `mimic-init`: UID 0, `allowPrivilegeEscalation: false`, drop `ALL`, add only `NET_ADMIN`/`NET_RAW`, read-only root, `RuntimeDefault`. `mimic-sidecar`: `restartPolicy: Always`, UID/GID 1337, `runAsNonRoot`, no capability, read-only root, `RuntimeDefault`, startup probe `/readyz` and liveness probe `/healthz` on 15100, the configured resources and the §10.3 variables. Neither has a volume mount. |
| INJ-04 | **U.** `MIMIC_EXCLUDE_INBOUND_PORTS` given to `mimic-init` holds, sorted and deduplicated, the ports of the `tcpSocket` and `grpc` liveness/readiness/startup probes of the app containers (named ports resolved against the container's ports, unresolvable names ignored) and 15100; `httpGet` probe ports are not in it. `MIMIC_APP_PROBES` holds every original `httpGet` probe under its `<container>/<kind>` key, with the named port resolved and the probe's `timeoutSeconds`; an `httpGet` probe whose named port does not resolve is left unchanged and absent from `MIMIC_APP_PROBES` (it fails as it would without mimic). Each rewritten probe points to `/app-health/<container>/<kind>` on 15100 with the original timings and thresholds. |
| INJ-05 | **U.** The `iptables-restore` input generated for IPv4 and for IPv6 is deterministic and: redirects inbound TCP to a local destination (`addrtype LOCAL`) to the inbound port except the excluded ports; returns outbound traffic of the proxy UID and on `lo`; redirects other outbound TCP to the outbound port; rejects outbound UDP to port 443 in `filter`; uses only mimic-owned chains (`MIMIC_INBOUND`, `MIMIC_OUTPUT`) plus one jump from each built-in chain it hooks, which it flushes first (§4). |
| INJ-06 | **I.** `mimic-init` pipes the rules to `iptables-restore` and `ip6tables-restore` found on `PATH`, logs one JSON line `redirection installed` with the families and excluded ports, and exits 0; it skips IPv6 with an INFO log when the pod has no IPv6; it exits 1 with an ERROR log naming the family when a restore command fails or is missing, and 2 on an invalid variable (FND-01). It never logs other content of its environment. |
| INJ-07 | **I.** The sidecar binds its inbound and outbound listeners from §10.3 before `/readyz` answers 200; the outbound listeners accept connections on loopback only. A connection without an original destination, or whose original destination is a sidecar listener, is closed and counted in `mimic_tcp_connections_total{direction,result="loop"}`; tunnelled connections are counted with `result="ok"` or `result="connect_error"`. |
| INJ-08 | **I.** The webhook answers only HTTPS on `MIMIC_WEBHOOK_ADDR`. `POST /inject/namespace`, `/inject/pod` and `/inject/workload` with an `AdmissionReview` v1 return an `AdmissionReview` with the same `uid`, `allowed: true` and a base64 `JSONPatch` (or none when the patch is empty). A malformed body gets 400, a body over 4 MiB gets 413, another method gets 405, and an internal error gets `allowed: false` with a message (fail-closed). |
| INJ-09 | **I.** Each admission writes one log line (`target` = `webhook`) with namespace, pod name or `generateName`, deployment, `result` (`injected`, `skipped`, `error`) and `reason`, and increments `mimic_webhook_admissions_total{result,reason}`. Neither the logs nor the metrics contain any env value, annotation value or other pod spec content (checked with a canary env value). |
| INJ-10 | **U.** The certificate decision is a pure function of the stored certificates and the current time (passed in): no or unparsable CA → new CA and serving certificate; serving certificate missing, expiring within 30 days or with the wrong SAN → new serving certificate signed by the existing CA; otherwise keep. |
| INJ-11 | **E.** After install, the webhook Secret holds a CA and a serving certificate for `<service>.<namespace>.svc`, the `caBundle` equals the CA, and `mimic_webhook_certificate_expiry_timestamp_seconds` is exposed. With 2 controller replicas both serve the same certificate; after a restart and a `helm upgrade` the certificate and `caBundle` are unchanged. The controller is unready until its CA is in every webhook's `caBundle`, the certificate is loaded (in that order) and the `MimicWorkload` cache is synced. |
| INJ-12 | **E.** The chart installs the `MimicWorkload` CRD (an invalid `spec.mode` or a missing or invalid `spec.deployment` is rejected, a change of `spec.deployment` is rejected, `spec.mode` defaults to `discovery`), a `MutatingWebhookConfiguration` with the three webhooks of §7.1 (`failurePolicy: Fail`, `sideEffects: None`, `reinvocationPolicy: IfNeeded`, pod `CREATE` only, excluded namespaces), and the RBAC of §11: `kubectl auth can-i` as the controller service account is `yes` exactly for the listed verbs and `no` for creating Secrets, reading other Secrets, reading pods and patching another webhook configuration. Patching anything but `caBundle` of its own configuration as that account is denied by the `ValidatingAdmissionPolicy`. |
| INJ-13 | **E.** In a namespace labelled `enabled` (Pod Security `privileged`), a Deployment with an HTTP readiness and liveness probe on its API port, a `tcpSocket` probe on a second port and an init container that opens a TCP connection to a Service rolls out Ready: `mimic-init` terminated with exit code 0, `mimic-sidecar` running, the injected annotation present, no restart. A request from another pod to the app Service succeeds and increments the sidecar's `mimic_tcp_connections_total{direction="inbound",result="ok"}`; a request from the app container to another Service succeeds and increments `{direction="outbound",result="ok"}`. The pod's readiness probe points to `/app-health/…` on 15100. When the app's readiness path starts failing, the pod becomes NotReady, and Ready again when the path recovers. |
| INJ-18 | **I.** The sidecar serves `GET /app-health/<container>/<kind>` on its admin port only for the probes in `MIMIC_APP_PROBES`, and 404 for any other path under `/app-health/`. It forwards the original method, path, port and headers and answers the app's status code (2xx, 3xx without following, 4xx and 5xx unchanged). It answers 503 when the app refuses the connection or exceeds the probe's `timeoutSeconds`, and talks HTTPS without verifying the certificate when the probe's scheme is HTTPS. An invalid `MIMIC_APP_PROBES` exits with code 2 (FND-01). |
| INJ-14 | **E.** From an injected app container, a UDP datagram to port 443 is refused (connected socket reports `ECONNREFUSED`), while DNS resolution over UDP 53 works. |
| INJ-15 | **E.** On the cluster: pods of an unlabelled namespace are unchanged; in a `selected` namespace only the deployment with a `MimicWorkload` is injected; a pod labelled `disabled` in an `enabled` namespace is unchanged; a pod labelled `enabled` in an unlabelled namespace is injected; the controller's namespace is never injected; labelling a namespace does not restart its running pods. |
| INJ-16 | **E.** With the controller scaled to 0, creating a pod in an `enabled` namespace fails (`FailedCreate` naming the webhook) while pods of unlabelled namespaces are created; once it is back, the ReplicaSet creates injected pods. In an `enabled` namespace enforcing Pod Security `baseline`, the injected pod is rejected by Pod Security (§11.1). |
| INJ-19 | **I.** With `MIMIC_WEBHOOK_MAX_CONNECTIONS=4` and `MIMIC_WEBHOOK_REQUEST_TIMEOUT=500ms`: a TCP client that never starts TLS, and a TLS client that sends headers byte by byte without finishing, are each closed within the timeout (+ 1 s margin); while 4 such clients are connected, a well-formed AdmissionReview is answered once they are closed, within 2 × the timeout; an idle keep-alive connection is closed after 60 s (test with an injectable idle duration). Invalid values of both variables exit with code 2 (FND-01). |
| INJ-20 | **I.** The controller watches its `MutatingWebhookConfiguration`: when a webhook's `caBundle` is changed or emptied by another client, the controller restores the Secret's CA in every webhook within 10 s, without issuing a new certificate (process test against the fake API server). |
| INJ-21 | **U.** Every literal `env` value in the patch has each `$` doubled (`MIMIC_APP_PROBES` with a probe path `/x/$(HOME)/$$` and a header value `a$b` decodes, after applying the kubelet's expansion rules, to the original probes). A pod with an app container named `mimic-init` or `mimic-sidecar`, or with an init container named `mimic-init` and no `mimic-sidecar`, is skipped as `name-conflict`. |
| INJ-22 | **I.** Running `mimic-init` twice, and after a first run whose IPv6 restore failed, results in rules that contain exactly one jump from each built-in chain per family (the fake restore scripts record the inputs; the generated input makes the jumps idempotent). **U.** With only `::1` (or no line) in `/proc/net/if_inet6` content, IPv6 is skipped; with a link-local or global address it is installed (unit test of the parser). |
| INJ-23 | **E.** By default the chart runs 2 controller replicas with a PodDisruptionBudget `maxUnavailable: 1` and preferred anti-affinity on `kubernetes.io/hostname`; the controller NetworkPolicy exists and the webhook still admits pods with it applied; `helm uninstall` leaves the CRD and an existing `MimicWorkload`, and a reinstall with the same release name succeeds and keeps it. |
| INJ-17 | **C.** `ghcr.io/thaliph/mimic-init` is built multi-arch, contains no shell, passes the image scan (no fixable HIGH/CRITICAL), and is signed with SBOM and provenance on release. The kind e2e runs on Kubernetes 1.33 (floor) and on the latest kind node image. |

## 13. Test strategy

- **Unit** tests in `mimic-core` (pure logic) and next to the code.
- **Integration** tests per crate in `tests/`: in-process routers, or the real binary started
  as a child process.
- **End-to-end** on kind with the Helm chart (`scripts/e2e.sh`), locally and in CI.
- A criterion is covered when a test whose name contains its ID (lower case) passes.
