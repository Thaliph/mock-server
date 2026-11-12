# Concepts

## Components

| Component | Kind | Role |
|---|---|---|
| `mimic-init` | init container of each injected pod, exits | Installs the iptables redirection in the pod's network namespace. The only mimic container that runs as root (UID 0), with only `NET_ADMIN` and `NET_RAW`. |
| `mimic-sidecar` | native sidecar (init container with `restartPolicy: Always`), UID 1337 | Transparent proxy for inbound and outbound TCP; records exchanges into a bounded local report; ships it to the controller. |
| `mimic-trust-<n>` | init container per distinct app image, exits | Builds that image's trust bundle plus the deployment's root CA. Runs mimic's own static binary, never the app's entrypoint. |
| `mimic-controller` | Deployment (2 replicas) in the release namespace | Mutating webhook (injection), `MimicWorkload` cache and status, sidecar API (receives reports), read API. Stateless. |
| Valkey | StatefulSet in the release namespace | The only store: merged report per deployment. AOF on a PVC, password-protected, `noeviction`. |

Sidecar ports inside a pod: **15006** inbound, **15001** outbound (loopback only), **15100** admin
(`/healthz`, `/readyz`, `/metrics`, `/discovery`).

## Data flow

```
             +------------------------ injected pod (Deployment "users") ------------------------+
 client ---> | iptables --> mimic-sidecar --> app --> iptables --> mimic-sidecar ----------------+--> other service
             |  (inbound :15006)  |  forward + record         (outbound :15001)                  |    or the internet
             |                    v                                                              |
             |            local report (bounded, in memory, redacted)                            |
             +--------------------|--------------------------------------------------------------+
                                  | every 30 s and at SIGTERM: POST /v1/workloads/<dep>/report
                                  | HTTPS :8444, projected token (audience mimic.platform.io)
                                  v
                       +----------------------+   merge + re-redact    +--------+
                       |   mimic-controller   | ---------------------> | Valkey |
                       |  webhook  sidecar API|   (atomic, once per    +--------+
                       |  read API  status    |    push sequence)
                       +----------------------+
                          ^                 |
   kubectl port-forward --+ read API :8080  +--> status of the MimicWorkload (kubectl get mimicworkload)
```

1. At pod creation, the API server calls the controller's webhook, which adds the mimic containers.
2. `mimic-init` redirects the pod's TCP traffic to the sidecar; the sidecar forwards it unchanged and
   records each HTTP exchange.
3. Every `MIMIC_DISCOVERY_PUSH_INTERVAL` (30 s) each sidecar posts what it recorded since its last
   acknowledged push. The controller merges it into the deployment's report in Valkey.
4. You read the merged report through the read API (reached with `kubectl port-forward`) or the
   summary in `kubectl get mimicworkload`.

## Pods and the workload identity

Only pods owned by a **Deployment** are injected. The Deployment name is the *workload identity*:
reports are keyed by `<namespace>/<deployment>`, and all pods of a Deployment, including pods
created later and pods deleted since, contribute to one report.

## Injection: which pods

Label key `mimic.platform.io/inject`:

| Namespace label | Pod label | Injected |
|---|---|---|
| any | `disabled` | never |
| `enabled` | absent or `enabled` | yes |
| `selected` | absent | only if a `MimicWorkload` in the namespace has `spec.deployment` equal to the pod's Deployment |
| any | `enabled` | yes |
| absent | absent | no (the webhook is not even called) |

`kube-system`, `kube-public`, `kube-node-lease` and the release namespace are never injected.
Injection happens at **pod creation**: labelling a namespace or creating a `MimicWorkload` does not
touch running pods; `kubectl rollout restart` does.

A pod that cannot be injected is admitted unchanged with the annotation
`mimic.platform.io/skipped: <reason>`; reasons: `host-network`, `unsupported-owner`, `port-conflict`
(an app container declares 15001, 15006 or 15100), `uid-conflict` (UID 1337 in use), `name-conflict`
(a container or volume name mimic needs is taken).

HTTP, `tcpSocket` and `grpc` probes of the app are rewritten to go through the sidecar admin port, so
that they keep working while traffic is redirected. Probe requests are never recorded.

The webhook **fails closed** (`failurePolicy: Fail`): while the controller is unavailable, creating
pods in the selected scope fails. This is why the chart runs two replicas and a PodDisruptionBudget.

## `MimicWorkload`

A namespaced custom resource (`mimic.platform.io/v1alpha1`) that names a Deployment of its namespace:

```yaml
apiVersion: mimic.platform.io/v1alpha1
kind: MimicWorkload
metadata: {name: users, namespace: demo}
spec:
  deployment: users
```

It selects the Deployment for injection in `selected` namespaces, carries per-deployment settings
(`spec.tls.exclude`, `spec.discovery.redact`) and shows what the controller holds in `status`. See
[Configuration](Configuration). When several `MimicWorkload`s name the same deployment, any of them
selects it and the settings are merged (union). A Deployment injected by label with no `MimicWorkload`
is in `none`: it is proxied and nothing is recorded until you ask for `discovery` or `mock` with
`spec.mode`. With several `MimicWorkload`s, `mock` wins over `discovery`, which wins over `none`.

`none` means "proxy only, nothing recorded". It does not mean "no mesh": the mimic mesh (a later
milestone) is independent of the mode and has its own switches (the chart value `mesh.enabled`
and `MimicConfig.spec.mesh.enabled`).

## Modes

| Mode | Behaviour | Status |
|---|---|---|
| **none** (default) | Proxy only: traffic is forwarded unchanged, nothing is recorded, nothing is sent to the controller, and outbound TLS is passed through (never intercepted). | Available. Pods in `none` still pull their mode, so a switch needs no restart. |
| **discovery** | Traffic is forwarded unchanged and recorded. | Available. Inbound and outbound exchanges are recorded. |
| **mock** | Outbound calls are answered from the Deployment's recorded egress instead of being forwarded; the real destination is not called. | Available. Switched with `spec.mode`, live, within one egress pull interval. See [Mock mode](Mock-mode). |

## The mesh view

The controller builds, for each Deployment, a **view**: the Services of its own namespace with their ClusterIPs,
ports and endpoints (ready ones, with the zone), plus the Services it declares in a `MimicConfig`
(`spec.destinations`) when the destination namespace consents with a `ReferenceGrant`. The sidecar follows it with a
long-poll (a change reaches it within about a second), keeps it in memory and in `/mimic/state/mesh.json`, and builds
a table from `<ClusterIP>:<port>` to Service. The view also carries the workload's mode and its `mesh` switch.

In this release the table is used to **classify** only. An outbound connection whose destination is a Service port
of the view is *in scope*; any other is *unscoped* and counted in `mimic_mesh_unscoped_connections_total`. Nothing is
routed: every connection still goes to the destination the app addressed, so a call to a Service outside the scope
works exactly as before. To bring a Service of another namespace into the scope, declare it in a `MimicConfig` and
create the `ReferenceGrant` in its namespace. `mesh.enabled: false` (`MIMIC_MESH=off`) and
`MimicConfig.spec.mesh.enabled: false` keep the plain relay and the classification off.

A restarted sidecar container reloads its last view and keeps its mode (from the mode state file, never from the
absence of a view); the view is *stale* after `MIMIC_MESH_STALE_AFTER` without contact with the controller
(`mimic_mesh_view_age_seconds`, one warning).

## Valkey

The controller is the only reader and writer. Keys are `mimic:v1:{<ns>/<dep>}:report`, `:meta`,
`:pods`, `:seq` and two indexes. Valkey runs with a password, `maxmemory` at 75 % of its memory
limit and `noeviction`. Details: [Operations](Operations).

## Where to go next

[Quick start](Quick-start) to try it, [Install](Install) to deploy it,
[Discovery and redaction](Discovery-and-redaction) for what is kept.
