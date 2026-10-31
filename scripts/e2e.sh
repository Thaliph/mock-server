#!/usr/bin/env bash
# End-to-end checks on kind (FND-08, FND-09). Idempotent: reuses the cluster if it exists.
#
#   IMAGE=ghcr.io/thaliph/mimic:e2e        image to test (built here unless SKIP_BUILD=1)
#   INIT_IMAGE=ghcr.io/thaliph/mimic-init:e2e  mimic-init image (built here unless SKIP_BUILD=1)
#   CLUSTER=mimic-e2e                      kind cluster name
#   NODE_IMAGE=kindest/node:v1.33.7@sha256:…   kind node image (default: kind's own)
#   KEEP_CLUSTER=1                         keep the cluster afterwards (default: keep locally, delete in CI)
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE=${IMAGE:-ghcr.io/thaliph/mimic:e2e}
INIT_IMAGE=${INIT_IMAGE:-ghcr.io/thaliph/mimic-init:e2e}
CLUSTER=${CLUSTER:-mimic-e2e}
NS=mimic-system
RELEASE=mimic
KEEP_CLUSTER=${KEEP_CLUSTER:-$([[ -n ${CI:-} ]] && echo 0 || echo 1)}

log() { printf '\n== %s\n' "$*"; }
fail() { echo "FAIL $*" >&2; kubectl -n "$NS" get pods -o wide >&2 || true; kubectl -n "$NS" logs deploy/$RELEASE-controller --tail=50 >&2 || true; exit 1; }
cleanup() {
  [[ -n ${PF_PID:-} ]] && kill "$PF_PID" 2>/dev/null || true
  [[ $KEEP_CLUSTER == 1 ]] || kind delete cluster --name "$CLUSTER"
}
trap cleanup EXIT

if ! kind get clusters | grep -qx "$CLUSTER"; then
  log "create kind cluster $CLUSTER"
  node_args=()
  [[ -z ${NODE_IMAGE:-} ]] || node_args=(--image "$NODE_IMAGE")
  kind create cluster --name "$CLUSTER" --wait 120s "${node_args[@]}"
fi
kubectl config use-context "kind-$CLUSTER" >/dev/null
server=$(kubectl version -o json | sed -n 's/.*"gitVersion": "v\([0-9]*\.[0-9]*\).*/\1/p' | tail -1)
log "kubernetes $server"

if [[ ${SKIP_BUILD:-0} != 1 ]]; then
  log "build $IMAGE and $INIT_IMAGE"
  docker build -q -t "$IMAGE" .
  docker build -q -t "$INIT_IMAGE" --target init .
fi
kind load docker-image --name "$CLUSTER" "$IMAGE" "$INIT_IMAGE"

log "fnd_08: namespace enforces Pod Security restricted"
kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f -
kubectl label --overwrite namespace "$NS" \
  pod-security.kubernetes.io/enforce=restricted pod-security.kubernetes.io/warn=restricted

repo=${IMAGE%:*}; tag=${IMAGE##*:}
install() {
  helm upgrade --install "$RELEASE" deploy/helm/mimic -n "$NS" \
    --set image.repository="$repo" --set image.tag="$tag" --set image.pullPolicy=Never \
    --wait --timeout 4m
}

log "fnd_09: helm install"
install || fail "fnd_09: helm install"
kubectl -n "$NS" rollout status statefulset/$RELEASE-valkey --timeout=2m
kubectl -n "$NS" rollout status deploy/$RELEASE-controller --timeout=2m

log "fnd_09: controller readyz and metrics through a port-forward"
kubectl -n "$NS" port-forward svc/$RELEASE-controller 18081:8081 >/dev/null &
PF_PID=$!
for _ in $(seq 30); do curl -fs localhost:18081/healthz >/dev/null && break; sleep 1; done
[[ $(curl -s -o /dev/null -w '%{http_code}' localhost:18081/readyz) == 200 ]] || fail "fnd_09: /readyz not 200"
curl -fs localhost:18081/metrics | grep -q '^mimic_build_info{component="controller",version="' \
  || fail "fnd_09: mimic_build_info missing"

log "fnd_08: Valkey refuses clients other than the controller"
password=$(kubectl -n "$NS" get secret $RELEASE-valkey -o jsonpath='{.data.password}' | base64 -d)
[[ ${#password} -ge 32 ]] || fail "fnd_08: generated password too short"
valkey_image=$(kubectl -n "$NS" get statefulset $RELEASE-valkey -o jsonpath='{.spec.template.spec.containers[0].image}')
# probe <namespace> <labels>: runs `valkey-cli ping` once and prints its output.
probe() {
  local name="probe-$RANDOM"
  kubectl apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Pod
metadata: {name: $name, namespace: $1, labels: {$2}}
spec:
  restartPolicy: Never
  automountServiceAccountToken: false
  securityContext: {runAsNonRoot: true, runAsUser: 999, seccompProfile: {type: RuntimeDefault}}
  containers:
    - name: probe
      image: $valkey_image
      command: [timeout, "5", valkey-cli, -h, $RELEASE-valkey.$NS, ping]
      env: [{name: REDISCLI_AUTH, value: "$password"}]
      securityContext: {allowPrivilegeEscalation: false, readOnlyRootFilesystem: true, capabilities: {drop: [ALL]}}
YAML
  kubectl -n "$1" wait --for=jsonpath='{.status.phase}'=Succeeded "pod/$name" --timeout=60s >/dev/null 2>&1 \
    || kubectl -n "$1" wait --for=jsonpath='{.status.phase}'=Failed "pod/$name" --timeout=5s >/dev/null 2>&1 || true
  kubectl -n "$1" logs "$name" 2>/dev/null || true
  kubectl -n "$1" delete pod "$name" --wait=false >/dev/null
}
# Positive control: with the controller's labels the same probe gets through, so a refusal
# below is the NetworkPolicy and not a broken probe.
probe "$NS" "app.kubernetes.io/instance: $RELEASE, app.kubernetes.io/name: mimic-controller" | grep -q PONG \
  || fail "fnd_08: probe with the controller labels could not reach Valkey"
if probe default "app: probe" | grep -q PONG; then
  fail "fnd_08: a pod outside the controller reached Valkey"
fi

log "fnd_08: the password survives an upgrade"
install || fail "fnd_08: helm upgrade"
after=$(kubectl -n "$NS" get secret $RELEASE-valkey -o jsonpath='{.data.password}' | base64 -d)
[[ $after == "$password" ]] || fail "fnd_08: password changed on upgrade"
kubectl -n "$NS" rollout status deploy/$RELEASE-controller --timeout=2m
[[ $(curl -s -o /dev/null -w '%{http_code}' localhost:18081/readyz) == 200 ]] || fail "fnd_08: not ready after upgrade"

log "inj_17: mimic-init installs the redirection rules in a pod network namespace"
# Privileged namespace: mimic-init needs NET_ADMIN and NET_RAW as UID 0 (spec §11). Checks that
# the nft-backed iptables-restore accepts the rules on this Kubernetes version.
kubectl create namespace inj-17 --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl label --overwrite namespace inj-17 pod-security.kubernetes.io/enforce=privileged >/dev/null
kubectl -n inj-17 delete pod mimic-init --ignore-not-found --wait=true >/dev/null
kubectl -n inj-17 apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Pod
metadata: {name: mimic-init}
spec:
  restartPolicy: Never
  automountServiceAccountToken: false
  containers:
    - name: init
      image: $INIT_IMAGE
      imagePullPolicy: Never
      securityContext:
        runAsUser: 0
        allowPrivilegeEscalation: false
        readOnlyRootFilesystem: true
        capabilities: {drop: [ALL], add: [NET_ADMIN, NET_RAW]}
        seccompProfile: {type: RuntimeDefault}
YAML
kubectl -n inj-17 wait --for=jsonpath='{.status.phase}'=Succeeded pod/mimic-init --timeout=120s >/dev/null \
  || { kubectl -n inj-17 logs mimic-init >&2 || true; fail "inj_17: mimic-init did not exit 0"; }
kubectl -n inj-17 logs mimic-init | grep -q '"message":"redirection installed"' \
  || fail "inj_17: mimic-init did not log the installed redirection"

log "e2e passed on kubernetes $server"
