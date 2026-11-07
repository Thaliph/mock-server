# Troubleshooting

Symptom, then cause and fix. Messages are the ones mimic logs; see [Operations](Operations#logs) for how
to read the logs.

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
