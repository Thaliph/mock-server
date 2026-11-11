# Install

Step 1 of [Get started](Home#get-started): everyone starts here, developers and ops. Developers need
only [Try it on kind](#try-it-on-kind) and [Install](#install); the sections marked *ops* are for whoever
runs mimic on a shared cluster. Covers the chart of release 0.2.0-alpha.2. Values are listed in
[Configuration](Configuration).

## Requirements

| Requirement | Why |
|---|---|
| Kubernetes **1.33 or later** | Native sidecars. The chart refuses older clusters (`kubeVersion`). |
| Helm **3.8 or later** | The chart is an OCI artifact. |
| A CNI that **enforces NetworkPolicy** | The chart isolates Valkey (ingress from the controller only) and restricts the controller's ports. Without enforcement the policies are inert. |
| A default StorageClass (or `valkey.persistence.storageClass`) | Valkey keeps its AOF on a PVC. Or set `valkey.persistence.enabled=false` and accept losing the store on pod restart. |
| `iptables-nft` on the nodes (kernel with `nf_tables`) | `mimic-init` uses the nft backend. |
| Application namespaces at Pod Security level **`privileged`** | `mimic-init` needs `NET_ADMIN` and `NET_RAW`; `baseline` and `restricted` namespaces reject injected pods (a `FailedCreate` event on the ReplicaSet). |

The release namespace itself runs at `restricted`: every pod the chart creates satisfies it.

## Try it on kind

To try mimic on a laptop you need [kind](https://kind.sigs.k8s.io/) and Docker. The defaults are enough for
discovery, mock mode and load tests; one command creates the cluster:

```bash
kind create cluster --name mimic
```

Use a kind recent enough that its default node image is Kubernetes 1.33 or later (`kubectl version` shows it).
It has a default StorageClass (`standard`) for Valkey, and the `iptables-nft` requirement was met on kind
0.33 (node 1.37), where the whole of this wiki's Quick start and Load test pages ran as written. kind's
default CNI (kindnet) enforces NetworkPolicy on current versions of kind, so the policies that isolate Valkey,
restrict the controller's ports and confine the load generators are active (the project's own end-to-end
tests on a default kind cluster check that a pod outside the controller cannot reach Valkey). If your kind is
old, or you replaced its CNI, check that yours does. Then follow [Install](#install) below and the
[Quick start](Quick-start). To remove everything: `kind delete cluster --name mimic`.

## Install

Install a released version from the OCI registry. A pre-release chart is only installed with an
explicit `--version`:

```bash
helm install mimic oci://ghcr.io/thaliph/charts/mimic --version 0.2.0-alpha.2 -n mimic-system --create-namespace \
  --set loadTest.enabled=true
kubectl -n mimic-system rollout status deploy/mimic-controller --timeout=5m
```

`--set loadTest.enabled=true` is there for the [Load test](Load-test) page of Get started; load tests are off by
default ([Enable load tests](#enable-load-tests)), so leave the flag out if you will not run any.

`rollout status` returns when the controller is ready (the first start also waits for Valkey and pulls the
images). To check it from outside, forward the **admin port**, 8081 (`/readyz`, `/healthz`, `/metrics`), which is
on the `mimic-controller` Service, and wait for the forward before the request:

```bash
kubectl -n mimic-system port-forward svc/mimic-controller 8081:8081 >/dev/null &
PF=$!
for i in $(seq 30); do curl -fs localhost:8081/readyz && break; sleep 1; done   # prints: ready
kill $PF
```

The controller has a second port, **8080**: the read API and the UI, served on the loopback of the pod only. It is
not on the Service, so it is reached through `deploy/mimic-controller` ([Reading the API](Reading-the-API#reaching-it)).

The chart pins both images by digest (`image.digest`, `inject.initImage.digest`), so installing by
version runs the released digests. To work from a checkout instead, replace the OCI reference with
`deploy/helm/mimic` in the commands below; the images then come from `ghcr.io/thaliph/mimic` and
`ghcr.io/thaliph/mimic-init` at the chart appVersion tag, or from your own build (see
[Developing](Developing#end-to-end-on-kind) and `scripts/e2e.sh`).

### Enable load tests

Load tests are **off by default** (they add RBAC, a CRD and admission policies). To use the
[Load test](Load-test) page, add `--set loadTest.enabled=true` to the install, or switch it on later:

```bash
helm upgrade mimic oci://ghcr.io/thaliph/charts/mimic --version 0.2.0-alpha.2 -n mimic-system \
  --reuse-values --set loadTest.enabled=true
```

Do it before you deploy the apps you want to test: pods injected before the switch must be restarted. The
option needs the images pinned by digest, which a released chart has.

### Verify the release

Images and chart are signed with cosign (keyless, GitHub OIDC); the signature names the release workflow
and the tag. Verifying is recommended for anything shared and optional for a local try-out; if you skip it,
go on to the next section. To verify, install [cosign](https://docs.sigstore.dev/cosign/system_config/installation/)
(for example `brew install cosign`, or download the binary of its releases page), then, before installing:

```bash
ID=https://github.com/Thaliph/mock-server/.github/workflows/release.yml@refs/tags/v0.2.0-alpha.2
ISSUER=https://token.actions.githubusercontent.com
for ref in ghcr.io/thaliph/mimic:0.2.0-alpha.2 ghcr.io/thaliph/mimic-init:0.2.0-alpha.2 ghcr.io/thaliph/charts/mimic:0.2.0-alpha.2; do
  cosign verify "$ref" --certificate-identity "$ID" --certificate-oidc-issuer "$ISSUER"
done
```

To pin what you verified (optional), resolve each tag to its digest (`docker buildx imagetools inspect <ref>` or
`crane digest <ref>`) and verify `<repo>@<digest>`; the release notes list the digests. Images are
tagged with the version only (no `latest`). Signing and verifying use the public Sigstore service.

All names in this wiki (`mimic-controller`, `mimic-valkey-0`, `data-mimic-valkey-0`, the Secret
`mimic-valkey`, ...) assume the release is called `mimic`; replace `mimic` with your release name
otherwise.

*Ops.* To enforce Pod Security on the release namespace as the chart's tests do:

```bash
kubectl label namespace mimic-system \
  pod-security.kubernetes.io/enforce=restricted pod-security.kubernetes.io/warn=restricted
```

Common overrides (all values: [Configuration](Configuration#helm-values)):

```bash
helm install mimic oci://ghcr.io/thaliph/charts/mimic --version 0.2.0-alpha.2 -n mimic-system --create-namespace \
  --set valkey.persistence.size=8Gi \
  --set valkey.resources.limits.memory=2Gi \
  --set metrics.serviceMonitor.enabled=true
```

What the release creates in its namespace: the controller Deployment (2 replicas), Service
(ports 8081 admin, 8443 webhook, 8444 sidecar API), PodDisruptionBudget (`maxUnavailable: 1`),
Valkey StatefulSet and Service, the NetworkPolicies, the Secrets `<release>-valkey` (random
password), `<release>-webhook-tls` and `<release>-tls-ca` (both filled by the controller), RBAC, and,
cluster-wide, the CRD, the `MutatingWebhookConfiguration` `<release>-injector` and a
`ValidatingAdmissionPolicy` that lets the controller change nothing in that configuration except its
`caBundle`.

### Use your own Valkey password

*Ops.* The chart generates a random password by default; to bring your own:

```bash
kubectl -n mimic-system create secret generic my-valkey --from-literal=password="$(openssl rand -base64 30)"
helm install mimic oci://ghcr.io/thaliph/charts/mimic --version 0.2.0-alpha.2 -n mimic-system --create-namespace \
  --set valkey.existingSecret=my-valkey
```

## Namespaces

- **Release namespace** (`mimic-system` above): controller and Valkey. Never injected.
- **Application namespaces**: opt in with a label, at the namespace or the pod:

```bash
# every Deployment of the namespace
kubectl label namespace demo mimic.platform.io/inject=enabled pod-security.kubernetes.io/enforce=privileged
# only Deployments named by a MimicWorkload
kubectl label namespace demo mimic.platform.io/inject=selected pod-security.kubernetes.io/enforce=privileged
```

A pod label `mimic.platform.io/inject: disabled` always opts a pod out. The full table is in
[Concepts](Concepts#injection-which-pods). Existing pods are injected on their next creation:
`kubectl -n demo rollout restart deploy/<name>`.

## Upgrade

*Ops.*

```bash
helm upgrade mimic oci://ghcr.io/thaliph/charts/mimic --version <new-version> -n mimic-system
```

There is no upgrade from the legacy `v0.1.0`: uninstall it and install fresh. Roll back only out of mock: switch
every `MimicWorkload` back to `discovery` before a `helm rollback`, or recreated pods forward for real.
Running pods keep the sidecar version they were created with; `rollout restart` them to pick up a new
one. The Valkey password Secret is kept across upgrades. The CRD is not removed or reset.

### Known issue: upgrading a chart installed from an earlier `main` build (#293)

(This affects only a chart installed from `main` or a checkout before the change, for example a cluster left
by an earlier `scripts/e2e.sh`; a released chart already has `4Gi`.)

The default `valkey.persistence.size` went from `1Gi` to `4Gi`. A StatefulSet's `volumeClaimTemplates`
is immutable, so `helm upgrade` of such an install fails with an error such as
`updates to statefulset spec for fields other than ... are forbidden`. Either:

- **keep the existing size** for that release:

  ```bash
  helm upgrade mimic deploy/helm/mimic -n mimic-system --set valkey.persistence.size=1Gi
  ```

  Risk: AOF rewrites need about twice the data, so a 1Gi volume can fill up. The old chart also used a
  low Valkey memory limit (256Mi); if you keep 1Gi, also keep `valkey.resources.limits.memory` low
  (`maxmemory` is 75 % of it) and watch the volume.

- or **start with the new size** by deleting the Valkey StatefulSet and its PVC before upgrading. The
  stored reports are lost; sidecars rebuild them from new traffic.

  Warning: while Valkey is gone the controller is unready, and the webhook fails closed, so **pod
  creation in injected namespaces is blocked** until `helm upgrade` has recreated Valkey and it is
  ready. Do it in a quiet window.

  ```bash
  kubectl -n mimic-system delete statefulset mimic-valkey
  kubectl -n mimic-system delete pvc data-mimic-valkey-0
  helm upgrade mimic deploy/helm/mimic -n mimic-system
  ```

## Uninstall

*Ops* on a shared cluster; on kind, deleting the cluster does it.

```bash
helm uninstall mimic -n mimic-system
```

| Kept | How to remove |
|---|---|
| The CRD and every `MimicWorkload` (`helm.sh/resource-policy: keep`) | `kubectl delete crd mimicworkloads.mimic.platform.io` |
| The Valkey password Secret `<release>-valkey` (when the chart created it) | `kubectl -n mimic-system delete secret mimic-valkey` |
| The Valkey PVC (Kubernetes does not delete StatefulSet PVCs) | `kubectl -n mimic-system delete pvc data-mimic-valkey-0` |

The TLS seed Secret `<release>-tls-ca` is deleted with the release. A later install with the same
release name and namespace adopts the kept CRD, and a kept password Secret keeps the kept PVC
readable. Emptying or recreating the seed rotates every deployment's root CA: running sidecars then
pass TLS through until their pods are recreated.

Pods that were injected keep their mimic containers after the uninstall (they cannot push reports any
more); restart them to remove the injection:

```bash
kubectl -n demo rollout restart deploy/users
```

## Next

[Quick start](Quick-start), [Operations](Operations), [Security](Security).
