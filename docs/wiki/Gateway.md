# Gateway

The **mimic Gateway** brings north-south traffic into the cluster through a Gateway API `Gateway` (milestone M6,
#456). For each `Gateway` of the class `mimic`, the controller creates a gateway `Deployment`, a `Service` and a
`PodDisruptionBudget` in the namespace of the Gateway. The gateway pods run `mimic-sidecar` in the gateway role.
The controller writes the standard Gateway API status of the class and of each Gateway.

**What the gateway does today.** It accepts HTTP connections on the listener ports of the Gateway. It answers
every request with `404`, `x-mimic-error: no-route` and `content-length: 0`, which is the no-match answer of the
[mesh](Mesh). It does not route yet:

- the routing of the `HTTPRoute`s attached to a Gateway comes with #457;
- HTTPS listeners (TLS termination, certificates) come with #458.

GRPCRoute, TLSRoute, TCPRoute, UDPRoute and ListenerSets are v2. The contract is section 7.10 of
[the spec](https://github.com/Thaliph/mock-server/blob/main/docs/SPEC.md#710-the-mimic-gateway-m6).

## Before you start

The gateway is **off by default**. The chart renders it only when all of these are true. If one is missing, the
chart fails with a message that names it.

| Requirement | Why |
|---|---|
| `mesh.enabled: true` (the default) | The data plane of the gateway is the mesh client. |
| `image.digest` set | The admission policy of the gateway pins the image by its digest. A released chart sets it. From a checkout, set it yourself. |
| `gateway.namespaces` not empty | Only the Gateways of these namespaces get objects. Each namespace must exist at install: the chart renders a RoleBinding in it. |
| The Gateway API CRDs, standard channel, at install | The chart checks them with `.Capabilities.APIVersions`. Install them as [Install](Install#gateway-api-optional) shows. |

The gateway pods are never injected (their pods have the label `mimic.platform.io/inject: disabled`). They pass Pod
Security `restricted`, so a namespace of `gateway.namespaces` can enforce `restricted`. An injected app in the same
namespace still needs `privileged` for `mimic-init`.

## Enable it

```bash
helm upgrade mimic oci://ghcr.io/thaliph/charts/mimic --version <version> -n mimic-system --reuse-values \
  --set gateway.enabled=true --set 'gateway.namespaces={shop}'
```

| Value | Default | Meaning |
|---|---|---|
| `gateway.enabled` | `false` | Renders the GatewayClass `mimic`, its RBAC, its admission policy and the controller variables `MIMIC_GATEWAY_*`. |
| `gateway.namespaces` | `[]` | Namespaces whose Gateways get objects (`MIMIC_GATEWAY_NAMESPACES`). Each must exist at install. |
| `gateway.serviceType` | `ClusterIP` | `type` of every gateway Service: `ClusterIP`, `NodePort` or `LoadBalancer` (`MIMIC_GATEWAY_SERVICE_TYPE`). |
| `gateway.maxReplicas` | `10` | Highest replica count that a Gateway can ask for (`MIMIC_GATEWAY_MAX_REPLICAS`), 1 to 100. |
| `gateway.maxConnections` | `1024` | Connections that one gateway pod serves at once (`MIMIC_GATEWAY_MAX_CONNECTIONS`, given to the pod as `MIMIC_MAX_CONNECTIONS`). It must fit the memory limit: see [the memory rule](#the-memory-rule). |
| `gateway.admittedAnnotations` | `[]` | Annotation keys that a mutating webhook of your platform adds to the gateway objects, for example `cloud.google.com/neg`. The admission policy admits these keys. The controller never writes them. |
| `gateway.resources` | requests `100m` / `128Mi`; limit memory `192Mi` | `resources` of the gateway container (`MIMIC_GATEWAY_RESOURCES`). It must have `limits.memory`. |

With `gateway.enabled: false` the chart renders none of these objects. The controller then watches no Gateway API
object other than those of the mesh (`HTTPRoute`, `ReferenceGrant`).

The chart creates the GatewayClass `mimic` with `controllerName: mimic.platform.io/gateway-controller` and no
`parametersRef`. A cluster admin can create other GatewayClasses with the same `controllerName`. The controller
handles all of them as mimic classes.

## Create a Gateway

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: web
  namespace: shop
spec:
  gatewayClassName: mimic
  listeners:
    - name: http
      protocol: HTTP
      port: 80
```

```bash
kubectl apply -f web-gateway.yaml
kubectl -n shop wait --for=condition=Programmed gateway/web --timeout=60s
kubectl -n shop get gateway web
```

The Gateway is `Programmed` when its Deployment has an available replica and its Service has an address. Then send
a request from a pod in the cluster that has `curl`:

```bash
curl -si http://web-mimic.shop/
```

The answer is `404` with `x-mimic-error: no-route` and `content-length: 0`.

A port below 1024 needs no capability. The gateway pod sets the safe, namespaced sysctl
`net.ipv4.ip_unprivileged_port_start=0`, which Pod Security `restricted` permits.

## The generated objects

The Deployment, the Service and the PodDisruptionBudget have **one name**:

- `<gateway>-mimic`, when that is a DNS-1035 label of at most 63 characters (GEP-1762);
- else a hashed form: the Gateway name with `.` changed to `-`, a `g` prefix if it does not start with a letter,
  cut to 46 characters, trailing `-` removed, then `-`, the first 10 hex digits of the SHA-256 of the Gateway
  name, and `-mimic`.

| Gateway | Generated name |
|---|---|
| `web` | `web-mimic` |
| `web.prod.example` | `web-prod-example-f17b22d722-mimic` |
| `1web` | `g1web-bac40f31e8-mimic` |

The three objects are in the namespace of the Gateway. They have these labels:

- `app.kubernetes.io/managed-by: mimic`;
- `app.kubernetes.io/name: mimic-gateway`;
- `mimic.platform.io/gateway: <gateway>`;
- `gateway.networking.k8s.io/gateway-name: <gateway>`;
- `gateway.networking.k8s.io/gateway-class-name: <class>`.

Each object has one owner reference to the Gateway (`controller: true`, no `blockOwnerDeletion`). When you delete
the Gateway, the garbage collector deletes the three objects.

| Object | What it holds |
|---|---|
| Deployment | The replicas (below), strategy `RollingUpdate` with `maxSurge: 1` and `maxUnavailable: 0`, one topology spread constraint on `kubernetes.io/hostname` (`ScheduleAnyway`). One container `gateway`: the image of the sidecar, `MIMIC_ROLE=gateway`, `MIMIC_GATEWAY_PORTS` (the distinct ports of the accepted listeners), `MIMIC_LOG` (the log level of the injected sidecars). One container port per listener port, and `15100` named `mimic-admin`. Readiness `/readyz` and liveness `/healthz` on port 15100. |
| Service | The `type` of `gateway.serviceType`. One port per distinct accepted listener port, ascending, named `<protocol>-<port>` in lower case (`http-80`), with `targetPort` equal to the port. Nothing else: no `externalIPs`, `loadBalancerIP`, `loadBalancerSourceRanges`, `loadBalancerClass`, `externalTrafficPolicy`, `sessionAffinity` or `publishNotReadyAddresses`. The admin port 15100 is never a Service port. |
| PodDisruptionBudget | The same selector, `maxUnavailable: 1`. |

The controller applies the objects again on each change of the Gateway, of its class or of its objects, and at least
every 5 minutes. It uses server-side apply (field manager `mimic-gateway`, `force: true`). An apply that changes
nothing writes nothing, so the two controller replicas agree without a resource version change.

**Managed objects.** An object is managed when it has the labels `app.kubernetes.io/managed-by: mimic` and
`mimic.platform.io/gateway: <gateway>`. The controller never changes or deletes an object of the generated name that
is not managed. In that case, the Gateway gets `Programmed: False`, reason `Invalid`. A managed object of an older
Gateway of the same name (another UID) is adopted by the next apply.

**Deletion.** The controller deletes the managed objects of a Gateway in these cases:

- the Gateway is not accepted;
- the Gateway left the mimic classes;
- a managed Deployment or Service names no Gateway of its namespace (with its PodDisruptionBudget).

The controller deletes nothing before its first full list of `gateways`, `deployments` and `services`. It deletes
nothing while the `gateways` CRD is absent.

## Replicas

The default is `min(2, gateway.maxReplicas)` replicas. To change it, set the annotation
`mimic.platform.io/replicas` in `spec.infrastructure.annotations` of the Gateway:

```yaml
spec:
  gatewayClassName: mimic
  infrastructure:
    annotations:
      mimic.platform.io/replicas: "3"
```

The value must be a decimal integer from 1 to `gateway.maxReplicas`, with no sign and no leading zero. `0`, `+3`,
`03`, `2.0` and `two` make the Gateway `Accepted: False`, reason `InvalidParameters`. The controller does not read
the annotations in `metadata.annotations` of the Gateway.

There is **no autoscaling** of the gateways. The apply forces the replica count, so the controller undoes an HPA or a
`kubectl scale` on a gateway Deployment at the next reconcile (at most 5 minutes later).

## The Service type

One value, `gateway.serviceType`, sets the `type` of every gateway Service of the cluster. A Gateway cannot choose
its own type.

> **Warning.** With `LoadBalancer`, every Gateway of a listed namespace gets an address outside the cluster. Whoever
> can create a Gateway of the class `mimic` in a listed namespace gets that Service, even without RBAC on Services.

The controller does not copy labels or annotations to the Service, so no cloud load balancer annotation reaches it.
The Gateway lists in `status.addresses`:

- for `ClusterIP` and `NodePort`, the `clusterIPs` of the Service (type `IPAddress`);
- for `LoadBalancer`, the ingress `ip`s (type `IPAddress`), then the ingress `hostname`s (type `Hostname`).

The list holds at most 16 addresses. It is empty while the Gateway is not programmed.

## Reading the status

```bash
kubectl get gatewayclass mimic -o jsonpath='{.status.conditions}' | jq
kubectl -n shop get gateway web -o json | jq '.status.conditions'
kubectl -n shop get gateway web -o json | jq '.status.listeners[] | {name, attachedRoutes, conditions}'
kubectl -n shop get gateway web -o jsonpath='{.status.addresses}'
```

**GatewayClass.** The controller writes the status of each GatewayClass with the `controllerName` of mimic:

| `Accepted` | Reason | Message | When |
|---|---|---|---|
| `True` | `Accepted` | | The class is valid. |
| `False` | `Unsupported` | `name longer than 63 characters` | The name does not fit the `gateway-class-name` label. |
| `False` | `InvalidParameters` | `parametersRef is not supported` | The class has a `parametersRef`. |

The controller writes no `SupportedVersion` condition and no `supportedFeatures`: they need a CRD read.

**Gateway `Accepted`.** The first row that applies gives the condition:

| # | When | `Accepted` | Reason | Message |
|---|---|---|---|---|
| 1 | The class has `Accepted: False` | `False` | `InvalidParameters` | `GatewayClass <name> is not accepted` |
| 2 | A field that mimic does not support ([What is refused](#what-is-refused-and-why)) | `False` | `UnsupportedValue` | `unsupported: <field path>`, the first in document order |
| 3 | `spec.addresses` is not empty | `False` | `UnsupportedAddress` | `spec.addresses is not supported` |
| 4 | `spec.infrastructure.parametersRef`, or a replicas value that is not valid | `False` | `InvalidParameters` | the field, and `must be 1 to <max>` for the replicas |
| 5 | No listener is accepted | `False` | `ListenersNotValid` | `no valid listener` |
| 6 | Some listeners are not accepted | `True` | `ListenersNotValid` | the names of those listeners |
| 7 | Else | `True` | `Accepted` | |

A Gateway of a mimic class in a namespace that is not in `gateway.namespaces` gets no condition, no status and no
object. mimic does not serve it, so it does not write to it.

**Gateway `Programmed`.** The first row that applies gives the condition:

| When | `Programmed` | Reason | Message |
|---|---|---|---|
| The Gateway is not accepted | `False` | `Invalid` | `not accepted` |
| An object of the generated name is not managed | `False` | `Invalid` | `<kind> <name> exists and is not managed by mimic` |
| The Deployment is missing, its `observedGeneration` is below its `generation`, or no replica is available | `False` | `Pending` | `<a> of <r> replicas available` |
| The Deployment has a `ReplicaFailure` condition `True` | `False` | `NoResources` | `replica creation failed` |
| No address: the Service is missing, has no `clusterIPs`, or a `LoadBalancer` has no ingress | `False` | `AddressNotAssigned` | `no address assigned` |
| Else | `True` | `Programmed` | `<a> of <r> replicas available` |

**Listeners.** Each entry of `status.listeners` has `name`, `supportedKinds`, `attachedRoutes` and four conditions:

- `Accepted`: `True` for an `HTTP` listener that mimic supports; else `False` with the reason of the next section.
- `ResolvedRefs`: `False`, `InvalidRouteKinds` when `allowedRoutes.kinds` has a kind other than `HTTPRoute`. The
  listener stays accepted, with `supportedKinds` that holds `HTTPRoute` only. Else `True`.
- `Conflicted`: always `False`, `NoConflicts`. The CRD makes port, protocol and hostname unique.
- `Programmed`: `True` when the listener is accepted and the Gateway is programmed. `False`, `Invalid` when the
  listener is not accepted. Else `False`, `Pending`.

`attachedRoutes` is 0 until #457 attaches routes.

**What the messages hold.** A status message holds only field paths, annotation keys and identifiers that the API
server validated and bounded (names, kinds, protocols, ports, replica counts). It never holds a hostname, an
annotation value or another `spec` value. Messages are cut to 1024 bytes. Only a Gateway of a mimic class in a
namespace of `gateway.namespaces` gets a status.

## What is refused, and why

mimic refuses a field that it cannot apply in full. It never applies a Gateway in part. A Gateway that is not
accepted has no objects.

| Field | Result | Why |
|---|---|---|
| A namespace not in `gateway.namespaces` | No status and no object | The cluster admin opts namespaces in. The controller has RBAC only there, and it keeps only the namespace, name and class of such a Gateway. |
| A name longer than 63 characters | Gateway `UnsupportedValue` | It does not fit the label values of the generated objects. |
| A `spec` larger than 64 KiB | Gateway `UnsupportedValue` | The controller keeps only the key, generation and class of such a Gateway. |
| A field that the controller does not know, at any level of `spec` | Gateway or listener `UnsupportedValue` | mimic cannot apply a field that it does not know. The controller keeps only its path. |
| `spec.tls` | Gateway `UnsupportedValue` | HTTPS comes with #458. |
| `spec.allowedListeners` with `namespaces.from` other than `None` | Gateway `UnsupportedValue` | ListenerSets are v2. A `ListenerSet` that names a mimic Gateway gets no status from mimic. |
| `spec.infrastructure.labels` | Gateway `UnsupportedValue` | The controller does not copy labels to the generated objects. |
| A key of `spec.infrastructure.annotations` other than `mimic.platform.io/replicas` | Gateway `UnsupportedValue` (the key is named, never the value) | The controller does not copy annotations, so no cloud load balancer annotation reaches the Service. |
| `spec.addresses` | Gateway `UnsupportedAddress` | The address comes from the Service. |
| `spec.infrastructure.parametersRef` | Gateway `InvalidParameters` | Not supported. |
| `protocol: HTTPS` | Listener `UnsupportedProtocol` | HTTPS comes with #458. |
| `protocol` `TLS`, `TCP`, `UDP` or another | Listener `UnsupportedProtocol`, message `protocol <p> is not supported` | v2. |
| Port `15100` | Listener `PortUnavailable` | It is the admin port of the gateway pod. |
| `filters` on a listener | Listener `UnsupportedValue` | Not supported. |
| `allowedRoutes.namespaces.from: Selector` | Listener `UnsupportedValue` | It needs a cluster-wide Namespace read, which the controller does not have. |

## The gateway pod

`mimic-sidecar` with `MIMIC_ROLE=gateway` installs nothing: no iptables rules, no trust, no state file. It records
nothing and mocks nothing. It binds `0.0.0.0:<port>` and `[::]:<port>` for each port of `MIMIC_GATEWAY_PORTS`. It
serves HTTP/1.1 and HTTP/2 with prior knowledge. If a bind fails, the process exits 1 with one ERROR
`gateway listener failed` that names the port.

The listeners face clients that mimic does not trust, so they have bounds:

| Bound | Value |
|---|---|
| Connections over all listeners | `MIMIC_MAX_CONNECTIONS`, which the controller sets from `gateway.maxConnections` (default 1024); the gateway closes the next ones at accept |
| Time for a request head | `MIMIC_GATEWAY_HEADER_TIMEOUT` (default `10s`), from the accept and again after each answer, for HTTP/1 and HTTP/2. An HTTP/2 connection that sends no `HEADERS` is closed at this timeout. |
| Size of a request head | 64 KiB, else `431` or the connection is closed |
| Idle keep-alive connection | closed after `MIMIC_GATEWAY_IDLE_TIMEOUT` (default `60s`), once the connection has had its first answer |
| HTTP/2 | 100 concurrent streams, a 64 KiB header list, connection and stream windows of 64 KiB, and the reset-flood limits of `hyper` |

The controller sets only `MIMIC_ROLE`, `MIMIC_GATEWAY_PORTS`, `MIMIC_LOG` and `MIMIC_MAX_CONNECTIONS`, so the gateway
pods use the defaults of the other variables.

### The memory rule

One held connection uses at most 96 KiB of the gateway's memory, in the worst case: a 63 KiB request head without
its end, or an HTTP/2 stream with 64 KiB of unread data. The process itself uses at most 32 MiB. So
`gateway.maxConnections` × 96 KiB + 32 MiB must fit `gateway.resources.limits.memory`:

| `limits.memory` | Highest `gateway.maxConnections` |
|---|---|
| `128Mi` | 1024 |
| `192Mi` (default) | 1706 |
| `256Mi` | 2389 |
| `512Mi` | 5120 |

The chart fails when the cap does not fit, or when `gateway.resources` has no `limits.memory`. The controller
checks the same rule on `MIMIC_GATEWAY_MAX_CONNECTIONS` and exits 2. The cap counts every client together: a
client with many source addresses can fill it. Per-source limits belong to a load balancer or a WAF in front of
the gateway.

`/readyz` on port 15100 answers `200` when every listener is bound. On `SIGTERM`, `/readyz` answers `503`, the
listeners accept no new connection, and the gateway drains the open connections within `MIMIC_SHUTDOWN_GRACE`. The gateway pod
serves `/metrics` on port 15100 with `component="gateway"` ([Monitoring](Monitoring#gateway-metrics)).

## Disable, unlist, upgrade

> **Warning.** Delete the Gateways before you set `gateway.enabled: false`, or before you remove a namespace from
> `gateway.namespaces`. After that change, the controller has no RBAC in the namespace. The gateway objects there
> keep running without reconcile until their Gateway is deleted.

- **Upgrade.** A new image digest, new `gateway.resources` or a new gateway variable changes every generated pod
  template. Each gateway Deployment then rolls, with `maxUnavailable: 0`. During a chart upgrade, the new admission
  policy refuses the applies of an old controller replica with the old digest. The old replica logs
  `gateway reconcile failed` until it is replaced.
- **Uninstall.** The status that mimic wrote on the Gateways and GatewayClasses stays after `helm uninstall`.

## Limits

See [Security](Security#gateway) for the RBAC, the admission policy and the accepted limits of the gateway, and
[Troubleshooting](Troubleshooting#gateway) for the reasons and log lines.
