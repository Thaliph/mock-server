#!/usr/bin/env bash
# End-to-end checks on kind (FND-08, FND-09, INJ-11..INJ-17, INJ-23, PRX-14, TLS-24..TLS-26, DSC-20, DSC-22, CTL-19..CTL-21).
# Idempotent:
# reuses the cluster if it exists; the scenario namespaces are recreated on each run.
#
#   TAG=e2e-<sha>-<pid>                         tag of the images built here (unique per run, so
#                                               another session's image on this daemon is never used)
#   IMAGE=ghcr.io/thaliph/mimic:$TAG            image to test (built here unless SKIP_BUILD=1)
#   INIT_IMAGE=ghcr.io/thaliph/mimic-init:$TAG  mimic-init image (built here unless SKIP_BUILD=1)
#   CLUSTER=mimic-e2e                           kind cluster name
#   NODE_IMAGE=kindest/node:v1.33.7@sha256:…    kind node image (default: kind's own)
#   KEEP_CLUSTER=1                              keep the cluster afterwards (default: keep locally, delete in CI)
set -euo pipefail
cd "$(dirname "$0")/.."

TAG=${TAG:-e2e-$(git rev-parse --short HEAD 2>/dev/null || echo local)-$$}
IMAGE=${IMAGE:-ghcr.io/thaliph/mimic:$TAG}
INIT_IMAGE=${INIT_IMAGE:-ghcr.io/thaliph/mimic-init:$TAG}
CLUSTER=${CLUSTER:-mimic-e2e}
NS=mimic-system
RELEASE=mimic
# Application image for the scenarios: Python with a shell. Pinned in images/Dockerfile (#273).
PY=$(scripts/pinned-image.sh e2e-app)
KEEP_CLUSTER=${KEEP_CLUSTER:-$([[ -n ${CI:-} ]] && echo 0 || echo 1)}
BUILT=0
PIDS=()
DERIVED=()
WORK=""

log() { printf '\n== %s\n' "$*"; }
fail() {
  echo "FAIL $*" >&2
  kubectl -n "$NS" get pods -o wide >&2 || true
  kubectl -n "$NS" logs deploy/$RELEASE-controller --tail=50 >&2 || true
  kubectl get events -A --sort-by=.lastTimestamp 2>/dev/null | tail -15 >&2 || true
  exit 1
}
cleanup() {
  for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
  [[ $BUILT == 1 ]] && docker rmi "$IMAGE" "$INIT_IMAGE" >/dev/null 2>&1 || true
  for image in "${DERIVED[@]}"; do docker rmi "$image" >/dev/null 2>&1 || true; done
  [[ -z $WORK ]] || rm -rf "$WORK"
  [[ $KEEP_CLUSTER == 1 ]] || kind delete cluster --name "$CLUSTER"
}
trap cleanup EXIT

if ! kind get clusters | grep -x "$CLUSTER" >/dev/null; then
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
  BUILT=1
fi
kind load docker-image --name "$CLUSTER" "$IMAGE" "$INIT_IMAGE"

log "fnd_08: namespace enforces Pod Security restricted"
kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f -
kubectl label --overwrite namespace "$NS" \
  pod-security.kubernetes.io/enforce=restricted pod-security.kubernetes.io/warn=restricted

repo=${IMAGE%:*}; tag=${IMAGE##*:}
init_repo=${INIT_IMAGE%:*}; init_tag=${INIT_IMAGE##*:}
install() {
  # The injector must use the images under test too: otherwise injected pods pull the chart's default
  # mimic-init tag from the registry. Extra arguments are further helm flags.
  helm upgrade --install "$RELEASE" deploy/helm/mimic -n "$NS" \
    --set image.repository="$repo" --set image.tag="$tag" --set image.pullPolicy=Never \
    --set inject.initImage.repository="$init_repo" --set inject.initImage.tag="$init_tag" \
    "$@" --wait --timeout 4m
}

# admin_forward: (re)starts the port-forward to the controller's admin port on localhost:18081.
admin_forward() {
  if [[ -n ${ADMIN_PF:-} ]]; then kill "$ADMIN_PF" 2>/dev/null || true; wait "$ADMIN_PF" 2>/dev/null || true; fi
  kubectl -n "$NS" port-forward svc/$RELEASE-controller 18081:8081 >/dev/null 2>&1 &
  ADMIN_PF=$!
  PIDS+=("$ADMIN_PF")
  for _ in $(seq 30); do curl -fs localhost:18081/healthz >/dev/null && return 0; sleep 1; done
  fail "admin port-forward to the controller"
}

log "fnd_09: helm install"
install || fail "fnd_09: helm install"
kubectl -n "$NS" rollout status statefulset/$RELEASE-valkey --timeout=2m
kubectl -n "$NS" rollout status deploy/$RELEASE-controller --timeout=2m

log "fnd_09: controller readyz and metrics through a port-forward"
admin_forward
[[ $(curl -s -o /dev/null -w '%{http_code}' localhost:18081/readyz) == 200 ]] || fail "fnd_09: /readyz not 200"
curl -fs localhost:18081/metrics | grep '^mimic_build_info{component="controller",version="' >/dev/null \
  || fail "fnd_09: mimic_build_info missing"

# The chart defaults (the install above sets no replica count) and the controller NetworkPolicy. Admission
# under the policy is exercised by every injection check below, which runs after this install.
log "inj_23: chart defaults: 2 controller replicas, PodDisruptionBudget maxUnavailable 1, hostname anti-affinity, NetworkPolicy, CRD kept"
[[ $(kubectl -n "$NS" get deploy $RELEASE-controller -o jsonpath='{.spec.replicas}') == 2 ]] \
  || fail "inj_23: the controller does not run 2 replicas by default"
[[ $(kubectl -n "$NS" get pdb $RELEASE-controller -o jsonpath='{.spec.maxUnavailable}') == 1 ]] \
  || fail "inj_23: the PodDisruptionBudget of the controller is not maxUnavailable 1"
[[ $(kubectl -n "$NS" get deploy $RELEASE-controller \
  -o jsonpath='{.spec.template.spec.affinity.podAntiAffinity.preferredDuringSchedulingIgnoredDuringExecution[0].podAffinityTerm.topologyKey}') == kubernetes.io/hostname ]] \
  || fail "inj_23: the controller has no preferred anti-affinity on kubernetes.io/hostname"
kubectl -n "$NS" get networkpolicy $RELEASE-controller >/dev/null 2>&1 || fail "inj_23: the controller NetworkPolicy is missing"
[[ $(kubectl get crd mimicworkloads.mimic.platform.io -o jsonpath='{.metadata.annotations.helm\.sh/resource-policy}') == keep ]] \
  || fail "inj_23: the CRD is not annotated helm.sh/resource-policy: keep"

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
probe "$NS" "app.kubernetes.io/instance: $RELEASE, app.kubernetes.io/name: mimic-controller" | grep PONG >/dev/null \
  || fail "fnd_08: probe with the controller labels could not reach Valkey"
if probe default "app: probe" | grep PONG >/dev/null; then
  fail "fnd_08: a pod outside the controller reached Valkey"
fi

# CTL-19: maxmemory is 75 % of the memory limit of the Valkey container, and a full store refuses writes
# (noeviction) instead of evicting. valkey-cli inside the pod authenticates with REDISCLI_AUTH.
log "ctl-19: Valkey maxmemory is 75 % of its memory limit, maxmemory-policy noeviction"
mem_limit=$(kubectl -n "$NS" get statefulset $RELEASE-valkey -o jsonpath='{.spec.template.spec.containers[0].resources.limits.memory}')
mem_num=${mem_limit%%[!0-9]*}
mem_unit=${mem_limit:${#mem_num}}
case $mem_unit in
  "") mem_mult=1 ;;
  Ki) mem_mult=1024 ;;
  Mi) mem_mult=1048576 ;;
  Gi) mem_mult=1073741824 ;;
  *) fail "ctl-19: unsupported Valkey memory limit $mem_limit" ;;
esac
want_maxmemory=$((mem_num * mem_mult * 3 / 4))
# valkey_config <parameter>: prints the value of CONFIG GET for one parameter.
valkey_config() { kubectl -n "$NS" exec statefulset/$RELEASE-valkey -- valkey-cli --raw CONFIG GET "$1" | tail -n 1; }
[[ $(valkey_config maxmemory) == "$want_maxmemory" ]] \
  || fail "ctl-19: maxmemory is not 75 % of the limit $mem_limit ($want_maxmemory bytes)"
[[ $(valkey_config maxmemory-policy) == noeviction ]] || fail "ctl-19: maxmemory-policy is not noeviction"

log "fnd_08: the password survives an upgrade"
install || fail "fnd_08: helm upgrade"
after=$(kubectl -n "$NS" get secret $RELEASE-valkey -o jsonpath='{.data.password}' | base64 -d)
[[ $after == "$password" ]] || fail "fnd_08: password changed on upgrade"
kubectl -n "$NS" rollout status deploy/$RELEASE-controller --timeout=2m
[[ $(curl -s -o /dev/null -w '%{http_code}' localhost:18081/readyz) == 200 ]] || fail "fnd_08: not ready after upgrade"

# ns_fresh <namespace> [key=value...]: the namespace recreated with the given labels.
ns_fresh() {
  kubectl delete namespace "$1" --ignore-not-found --wait=true --timeout=2m >/dev/null
  kubectl create namespace "$1" >/dev/null
  if [[ $# -gt 1 ]]; then kubectl label namespace "$1" "${@:2}" >/dev/null; fi
}

# secret_key <key>: the base64 value of a key of the webhook Secret.
secret_key() { kubectl -n "$NS" get secret $RELEASE-webhook-tls -o jsonpath="{.data.$1}"; }

# serving_fp <pod>: SHA-256 fingerprint of the certificate the pod serves on its webhook port.
serving_fp() {
  local port=$((28000 + RANDOM % 1000)) fp="" pf
  kubectl -n "$NS" port-forward "$1" "$port:8443" >/dev/null 2>&1 &
  pf=$!
  for _ in $(seq 30); do
    fp=$(openssl s_client -connect "127.0.0.1:$port" -servername "$RELEASE-controller.$NS.svc" </dev/null 2>/dev/null \
      | openssl x509 -noout -fingerprint -sha256 2>/dev/null) || true
    [[ -n $fp ]] && break
    sleep 1
  done
  kill "$pf" 2>/dev/null || true
  wait "$pf" 2>/dev/null || true
  printf '%s\n' "$fp"
}

log "inj_11: the webhook certificate is created, served by both replicas, and kept on restart and upgrade"
ca_b64=$(secret_key 'ca\.crt'); tls_b64=$(secret_key 'tls\.crt')
[[ -n $ca_b64 && -n $tls_b64 ]] || fail "inj_11: the webhook Secret has no ca.crt or tls.crt"
# Captured first: under pipefail, `grep -q` exiting early can SIGPIPE openssl and fail the check.
san=$(printf '%s' "$tls_b64" | base64 -d | openssl x509 -noout -ext subjectAltName)
[[ $san == *"DNS:$RELEASE-controller.$NS.svc"* ]] || fail "inj_11: serving certificate lacks the service name in its SAN"
openssl verify -CAfile <(printf '%s' "$ca_b64" | base64 -d) <(printf '%s' "$tls_b64" | base64 -d) >/dev/null \
  || fail "inj_11: serving certificate is not signed by the CA in the Secret"
[[ $(kubectl get mutatingwebhookconfiguration $RELEASE-injector -o jsonpath='{.webhooks[0].clientConfig.caBundle}') == "$ca_b64" ]] \
  || fail "inj_11: caBundle is not the CA of the Secret"
admin_forward
curl -fs localhost:18081/metrics | grep '^mimic_webhook_certificate_expiry_timestamp_seconds' >/dev/null \
  || fail "inj_11: mimic_webhook_certificate_expiry_timestamp_seconds not exposed"
tls_fp=$(printf '%s' "$tls_b64" | base64 -d | openssl x509 -noout -fingerprint -sha256)
# wait_controller_pods <n>: waits until exactly <n> controller pods exist (terminating ones are gone).
wait_controller_pods() {
  for _ in $(seq 120); do
    [[ $(kubectl -n "$NS" get pods -l app.kubernetes.io/name=mimic-controller -o name | wc -l) == "$1" ]] && return 0
    sleep 1
  done
  fail "inj_11: controller pods did not settle at $1"
}
wait_controller_pods 2
served=0
for pod in $(kubectl -n "$NS" get pods -l app.kubernetes.io/name=mimic-controller -o name); do
  [[ $(serving_fp "$pod") == "$tls_fp" ]] || fail "inj_11: $pod does not serve the certificate of the Secret"
  served=$((served + 1))
done
[[ $served == 2 ]] || fail "inj_11: $served controller replicas served the certificate, want 2"

check_kept() {
  [[ $(secret_key 'ca\.crt') == "$ca_b64" && $(secret_key 'tls\.crt') == "$tls_b64" ]] \
    || fail "inj_11: $1 changed the certificate in the Secret"
  [[ $(kubectl get mutatingwebhookconfiguration $RELEASE-injector -o jsonpath='{.webhooks[0].clientConfig.caBundle}') == "$ca_b64" ]] \
    || fail "inj_11: $1 changed the caBundle"
}
kubectl -n "$NS" rollout restart deploy/$RELEASE-controller >/dev/null
kubectl -n "$NS" rollout status deploy/$RELEASE-controller --timeout=2m
check_kept "a restart"
install || fail "inj_11: helm upgrade"
kubectl -n "$NS" rollout status deploy/$RELEASE-controller --timeout=2m
check_kept "a helm upgrade"
wait_controller_pods 2
for pod in $(kubectl -n "$NS" get pods -l app.kubernetes.io/name=mimic-controller -o name); do
  [[ $(serving_fp "$pod") == "$tls_fp" ]] || fail "inj_11: $pod serves another certificate after the upgrade"
done

log "inj_12: the controller's RBAC, the CRD, the webhook fields and the ValidatingAdmissionPolicy hold"
sa="system:serviceaccount:$NS:$RELEASE-controller"
cfg=$RELEASE-injector
# can <yes|no> <description> <verb and resource...>: `kubectl auth can-i` as the controller account.
can() {
  local want=$1 desc=$2 got
  shift 2
  got=$(kubectl auth can-i --as="$sa" "$@" 2>/dev/null) || true
  [[ $got == "$want" ]] || fail "${CAN_ID:-inj_12}: $desc (want $want, got '$got')"
}
# accepted <description> <command...>: the command succeeds. denied: it fails.
accepted() { local desc=$1; shift; "$@" >/dev/null 2>&1 || fail "inj_12: $desc was rejected"; }
denied() { local desc=$1; shift; if "$@" >/dev/null 2>&1; then fail "inj_12: $desc was accepted"; fi; }

can yes "list mimicworkloads" list mimicworkloads.mimic.platform.io -A
can yes "watch mimicworkloads" watch mimicworkloads.mimic.platform.io -A
can yes "get its webhook configuration" get mutatingwebhookconfigurations.admissionregistration.k8s.io/$cfg
can yes "list its webhook configuration" list mutatingwebhookconfigurations.admissionregistration.k8s.io/$cfg
can yes "watch its webhook configuration" watch mutatingwebhookconfigurations.admissionregistration.k8s.io/$cfg
can yes "patch its webhook configuration" patch mutatingwebhookconfigurations.admissionregistration.k8s.io/$cfg
CAN_ID=tls_24 can yes "create tokenreviews (sidecar API authentication)" create tokenreviews.authentication.k8s.io
can no "list all webhook configurations" list mutatingwebhookconfigurations.admissionregistration.k8s.io
can no "watch all webhook configurations" watch mutatingwebhookconfigurations.admissionregistration.k8s.io
can no "list another webhook configuration" list mutatingwebhookconfigurations.admissionregistration.k8s.io/some-other
can no "watch another webhook configuration" watch mutatingwebhookconfigurations.admissionregistration.k8s.io/some-other
can yes "get the webhook secret" get secret/$RELEASE-webhook-tls -n "$NS"
can yes "update the webhook secret" update secret/$RELEASE-webhook-tls -n "$NS"
can no "create secrets" create secrets -n "$NS"
can no "list secrets" list secrets -n "$NS"
can no "get the valkey secret" get secret/$RELEASE-valkey -n "$NS"
can no "get pods" get pods -n "$NS"
can no "list pods" list pods -A
can no "patch another webhook configuration" patch mutatingwebhookconfigurations.admissionregistration.k8s.io/some-other
can no "get another webhook configuration" get mutatingwebhookconfigurations.admissionregistration.k8s.io/some-other
can no "create deployments" create deployments.apps -n "$NS"
can no "create subjectaccessreviews (only tokenreviews)" create subjectaccessreviews.authorization.k8s.io
# CTL-19: the status of a MimicWorkload and nothing else of it; no pods, no ReplicaSets.
CAN_ID=ctl_19 can yes "patch mimicworkloads/status" patch mimicworkloads.mimic.platform.io --subresource=status -A
CAN_ID=ctl_19 can no "update mimicworkloads" update mimicworkloads.mimic.platform.io -A
CAN_ID=ctl_19 can no "patch mimicworkloads" patch mimicworkloads.mimic.platform.io -A
CAN_ID=ctl_19 can no "get pods in every namespace" get pods -A
CAN_ID=ctl_19 can no "list replicasets" list replicasets.apps -A

# The ValidatingAdmissionPolicy: as the controller account only caBundle may change.
orig_cb=$(kubectl get mutatingwebhookconfiguration $cfg -o jsonpath='{.webhooks[0].clientConfig.caBundle}')
cb_patch() {
  printf '[{"op":"replace","path":"/webhooks/0/clientConfig/caBundle","value":"%s"},{"op":"replace","path":"/webhooks/1/clientConfig/caBundle","value":"%s"},{"op":"replace","path":"/webhooks/2/clientConfig/caBundle","value":"%s"}]' "$1" "$1" "$1"
}
accepted "the controller changes caBundle" kubectl patch mutatingwebhookconfiguration $cfg --as="$sa" --type=json -p "$(cb_patch Y2E=)"
# Restore the CA the controller wrote, so the injection checks below keep working.
kubectl patch mutatingwebhookconfiguration $cfg --type=json -p "$(cb_patch "$orig_cb")" >/dev/null
denied "the controller changes failurePolicy" kubectl patch mutatingwebhookconfiguration $cfg --as="$sa" --type=json \
  -p '[{"op":"replace","path":"/webhooks/0/failurePolicy","value":"Ignore"}]'
denied "the controller adds clientConfig.url" kubectl patch mutatingwebhookconfiguration $cfg --as="$sa" --type=json \
  -p '[{"op":"add","path":"/webhooks/2/clientConfig/url","value":"https://evil.example"}]'
denied "the controller adds a label" kubectl patch mutatingwebhookconfiguration $cfg --as="$sa" --type=merge \
  -p '{"metadata":{"labels":{"x":"y"}}}'
denied "the controller adds an annotation" kubectl patch mutatingwebhookconfiguration $cfg --as="$sa" --type=merge \
  -p '{"metadata":{"annotations":{"x":"y"}}}'
[[ $(kubectl get mutatingwebhookconfiguration $cfg -o jsonpath='{.webhooks[0].clientConfig.caBundle}') == "$orig_cb" ]] \
  || fail "inj_12: restoring the caBundle failed"

# CTL-19: the CRD has the status subresource and the printer columns of spec §7.4.
log "ctl_19: the CRD has the status subresource and the printer columns Mode, Endpoints, Pods, Last push"
crd_columns=$(kubectl get crd mimicworkloads.mimic.platform.io -o jsonpath='{range .spec.versions[0].additionalPrinterColumns[*]}{.name}{"|"}{end}')
[[ $crd_columns == 'Mode|Endpoints|Pods|Last push|' ]] || fail "ctl_19: printer columns are '$crd_columns'"
[[ $(kubectl get crd mimicworkloads.mimic.platform.io -o jsonpath='{.spec.versions[0].subresources.status}') == '{}' ]] \
  || fail "ctl_19: the CRD has no status subresource"

# The CRD: spec.deployment is required, DNS-1123, immutable; spec.mode defaults to discovery.
workload() {
  printf 'apiVersion: mimic.platform.io/v1alpha1\nkind: MimicWorkload\nmetadata: {name: %s, namespace: default}\n%s\n' "$1" "$2" \
    | kubectl apply -f - >/dev/null
}
denied "a MimicWorkload without spec" workload t1 ""
denied "a MimicWorkload without spec.deployment" workload t2 "spec: {mode: mock}"
denied "a MimicWorkload with an invalid spec.deployment" workload t3 "spec: {deployment: Bad_Name}"
denied "a MimicWorkload with an invalid spec.mode" workload t4 "spec: {deployment: api, mode: nope}"
accepted "a valid MimicWorkload" workload t5 "spec: {deployment: api}"
[[ $(kubectl -n default get mimicworkload t5 -o jsonpath='{.spec.mode}') == discovery ]] \
  || fail "inj_12: spec.mode does not default to discovery"
denied "a change of spec.deployment" kubectl -n default patch mimicworkload t5 --type=merge -p '{"spec":{"deployment":"web"}}'
kubectl -n default delete mimicworkload t5 --ignore-not-found >/dev/null

# The webhook configuration: three webhooks, one per path, fail closed, pods only, system namespaces excluded.
hooks=$(kubectl get mutatingwebhookconfiguration $cfg -o jsonpath='{range .webhooks[*]}{.failurePolicy} {.sideEffects} {.reinvocationPolicy} {.timeoutSeconds} {.rules[0].operations[*]} {.rules[0].resources[*]} {.clientConfig.service.path} {.clientConfig.service.port}{"\n"}{end}')
want_hooks=$(printf '%s\n' "Fail None IfNeeded 5 CREATE pods /inject/namespace 8443" \
  "Fail None IfNeeded 5 CREATE pods /inject/workload 8443" \
  "Fail None IfNeeded 5 CREATE pods /inject/pod 8443" | sort)
[[ $(printf '%s\n' "$hooks" | sort) == "$want_hooks" ]] || fail "inj_12: webhook fields differ from spec §7.1 (got: $hooks)"
[[ $(kubectl get mutatingwebhookconfiguration $cfg -o jsonpath='{.webhooks[*].namespaceSelector}') == *kube-system* ]] \
  || fail "inj_12: kube-system is not excluded from injection"

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
kubectl -n inj-17 logs mimic-init | grep '"message":"redirection installed"' >/dev/null \
  || fail "inj_17: mimic-init did not log the installed redirection"

# Scenarios below use Deployments of the Python image. `pod` and `jp` read a pod's fields.
jp() { kubectl -n "$1" get pod "$2" -o jsonpath="$3"; }

# tcp_count <namespace> <direction> <result>: the app pod's mimic_tcp_connections_total, read from the
# sidecar's admin port over loopback (not captured) through the app container's Python.
tcp_count() {
  kubectl -n "$1" exec deploy/app -c app -- python3 -c \
    'import sys, urllib.request; sys.stdout.write(urllib.request.urlopen("http://127.0.0.1:15100/metrics", timeout=5).read().decode())' \
    | awk -v want="mimic_tcp_connections_total{direction=\"$2\",result=\"$3\"}" 'index($0, want) == 1 { printf "%d\n", $2 }' \
    | tail -1
}
# wait_tcp_increase <namespace> <direction> <before>: polls until the ok counter exceeds <before>.
wait_tcp_increase() {
  local after=0
  for _ in $(seq 10); do
    after=$(tcp_count "$1" "$2" ok) || true
    after=${after:-0}
    (( after > $3 )) && return 0
    sleep 1
  done
  return 1
}

# deploy_sleeper <namespace> <name> [pod label]: a Deployment of one sleeping Python container.
deploy_sleeper() {
  local labels="app: $2"
  if [[ -n ${3:-} ]]; then labels="$labels, $3"; fi
  kubectl apply -f - >/dev/null <<YAML
apiVersion: apps/v1
kind: Deployment
metadata: {name: $2, namespace: $1}
spec:
  replicas: 1
  selector: {matchLabels: {app: $2}}
  template:
    metadata: {labels: {$labels}}
    spec:
      containers:
        - name: app
          image: $PY
          command: [sleep, "3600"]
YAML
}

log "inj_13: an injected app rolls out Ready, carries traffic both ways and follows its readiness"
ns_fresh inj-13-up
ns_fresh inj-13 mimic.platform.io/inject=enabled pod-security.kubernetes.io/enforce=privileged
kubectl apply -f - >/dev/null <<YAML
apiVersion: apps/v1
kind: Deployment
metadata: {name: echo, namespace: inj-13-up}
spec:
  replicas: 1
  selector: {matchLabels: {app: echo}}
  template:
    metadata: {labels: {app: echo}}
    spec:
      containers:
        - name: echo
          image: $PY
          command: [sh, -c, "mkdir -p /www && touch /www/ok && exec python3 -m http.server 8080 -d /www"]
          ports: [{containerPort: 8080}]
---
apiVersion: v1
kind: Service
metadata: {name: echo, namespace: inj-13-up}
spec:
  selector: {app: echo}
  ports: [{port: 8080, targetPort: 8080}]
---
apiVersion: v1
kind: Pod
metadata: {name: client, namespace: inj-13-up, labels: {app: client}}
spec:
  containers:
    - name: client
      image: $PY
      command: [sleep, "3600"]
---
apiVersion: v1
kind: Service
metadata: {name: app, namespace: inj-13}
spec:
  selector: {app: app}
  ports: [{name: api, port: 8080, targetPort: api}]
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: app, namespace: inj-13}
spec:
  replicas: 1
  selector: {matchLabels: {app: app}}
  template:
    metadata: {labels: {app: app}}
    spec:
      terminationGracePeriodSeconds: 5
      initContainers:
        # Runs after mimic-sidecar has started, so its connection goes through the sidecar.
        - name: upstream
          image: $PY
          command: [python3, -c, "import socket; socket.create_connection(('echo.inj-13-up.svc', 8080), 10).close()"]
      containers:
        - name: app
          image: $PY
          command: [sh, -c, "mkdir -p /www && touch /www/ready /www/live && python3 -m http.server 8080 -d /www & python3 -m http.server 9090 -d /www & wait"]
          ports: [{name: api, containerPort: 8080}, {name: second, containerPort: 9090}]
          startupProbe: {tcpSocket: {port: 9090}, periodSeconds: 1, failureThreshold: 30}
          readinessProbe: {httpGet: {path: /ready, port: api}, periodSeconds: 2, failureThreshold: 1}
          livenessProbe: {httpGet: {path: /live, port: api}, periodSeconds: 5, failureThreshold: 3}
YAML
kubectl -n inj-13 rollout status deploy/app --timeout=3m \
  || { kubectl -n inj-13 describe pods -l app=app >&2 || true; fail "inj_13: app did not roll out Ready"; }
pod=$(kubectl -n inj-13 get pods -l app=app -o jsonpath='{.items[0].metadata.name}')
[[ $(jp inj-13 "$pod" '{.status.initContainerStatuses[?(@.name=="mimic-init")].state.terminated.exitCode}') == 0 ]] \
  || fail "inj_13: mimic-init did not terminate with exit code 0"
[[ -n $(jp inj-13 "$pod" '{.status.initContainerStatuses[?(@.name=="mimic-sidecar")].state.running}') ]] \
  || fail "inj_13: mimic-sidecar is not running"
[[ -n $(jp inj-13 "$pod" '{.metadata.annotations.mimic\.platform\.io/injected}') ]] \
  || fail "inj_13: injected annotation missing"
[[ $(jp inj-13 "$pod" '{.status.containerStatuses[?(@.name=="app")].restartCount}') == 0 ]] \
  || fail "inj_13: the app container restarted"
[[ $(jp inj-13 "$pod" '{.status.initContainerStatuses[?(@.name=="mimic-sidecar")].restartCount}') == 0 ]] \
  || fail "inj_13: mimic-sidecar restarted"
[[ $(jp inj-13 "$pod" '{.spec.containers[?(@.name=="app")].readinessProbe.httpGet.path}') == /app-health/app/readyz ]] \
  || fail "inj_13: readiness probe does not go through the sidecar"
[[ $(jp inj-13 "$pod" '{.spec.containers[?(@.name=="app")].readinessProbe.httpGet.port}') == 15100 ]] \
  || fail "inj_13: readiness probe port is not the sidecar admin port"
[[ $(jp inj-13 "$pod" '{.spec.containers[?(@.name=="app")].startupProbe.httpGet.path}') == /app-health/app/startupz ]] \
  || fail "inj_13: tcpSocket startup probe was not rewritten to go through the sidecar"
[[ $(jp inj-13 "$pod" '{.spec.initContainers[?(@.name=="mimic-init")].env[?(@.name=="MIMIC_EXCLUDE_INBOUND_PORTS")].value}') == 15100 ]] \
  || fail "inj_13: a probe port is excluded from inbound interception"

kubectl -n inj-13-up wait --for=condition=Ready pod/client --timeout=180s >/dev/null \
  || fail "inj_13: client pod in inj-13-up not Ready (image pull?)"
before=$(tcp_count inj-13 inbound ok) || true; before=${before:-0}
kubectl -n inj-13-up exec client -- python3 -c \
  'import urllib.request; urllib.request.urlopen("http://app.inj-13.svc:8080/ready", timeout=10).read()' \
  || fail "inj_13: request from another pod to the app Service failed"
wait_tcp_increase inj-13 inbound "$before" \
  || fail "inj_13: inbound connection not counted as mimic_tcp_connections_total{direction=\"inbound\",result=\"ok\"}"

# The port of the tcpSocket probe is intercepted too.
pod_ip=$(jp inj-13 "$pod" '{.status.podIP}')
before=$(tcp_count inj-13 inbound ok) || true; before=${before:-0}
kubectl -n inj-13-up exec client -- python3 -c \
  "import urllib.request; urllib.request.urlopen('http://$pod_ip:9090/ready', timeout=10).read()" \
  || fail "inj_13: request from another pod to the tcpSocket probe port failed"
wait_tcp_increase inj-13 inbound "$before" \
  || fail "inj_13: connection to the tcpSocket probe port bypassed the sidecar"

before=$(tcp_count inj-13 outbound ok) || true; before=${before:-0}
kubectl -n inj-13 exec "$pod" -c app -- python3 -c \
  'import urllib.request; urllib.request.urlopen("http://echo.inj-13-up.svc:8080/ok", timeout=10).read()' \
  || fail "inj_13: request from the app container to another Service failed"
wait_tcp_increase inj-13 outbound "$before" \
  || fail "inj_13: outbound connection not counted as mimic_tcp_connections_total{direction=\"outbound\",result=\"ok\"}"

kubectl -n inj-13 exec "$pod" -c app -- rm /www/ready
kubectl -n inj-13 wait --for=condition=Ready=False "pod/$pod" --timeout=60s >/dev/null \
  || fail "inj_13: pod still Ready after the app's readiness path started failing"
kubectl -n inj-13 exec "$pod" -c app -- touch /www/ready
kubectl -n inj-13 wait --for=condition=Ready=True "pod/$pod" --timeout=90s >/dev/null \
  || fail "inj_13: pod not Ready again after the app's readiness path recovered"
[[ $(jp inj-13 "$pod" '{.status.containerStatuses[?(@.name=="app")].restartCount}') == 0 ]] \
  || fail "inj_13: the app container restarted while its readiness flapped"

log "inj_14: UDP to port 443 is refused from an injected container, DNS over UDP 53 works"
udp=$(kubectl -n inj-13 exec "$pod" -c app -- python3 -c '
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.settimeout(5)
s.connect(("192.0.2.1", 443))
# The first send fails with EPERM (the REJECT rule); the connected socket then reports ECONNREFUSED.
try:
    s.send(b"x")
except OSError:
    pass
try:
    s.recv(1)
    print("open")
except ConnectionRefusedError:
    print("refused")
') || fail "inj_14: could not run the UDP probe in the app container"
[[ $udp == refused ]] || fail "inj_14: UDP to port 443 was not refused (got '$udp')"
resolved=$(kubectl -n inj-13 exec "$pod" -c app -- python3 -c \
  'import socket; print(socket.getaddrinfo("echo.inj-13-up.svc", 8080, socket.AF_INET, socket.SOCK_DGRAM)[0][4][0])') \
  || fail "inj_14: DNS over UDP 53 failed from the injected container"
[[ -n $resolved ]] || fail "inj_14: DNS returned no address"

# --- PRX-14 (spec §12.3, §4.1, §9) ---------------------------------------------------------------
# The three flows of the INJ-13 scenario: another pod to the app (inbound HTTP), the app to echo
# (outbound HTTP) and the app to the controller's webhook Service (outbound TLS).
#   Group 1: the flows complete through the sidecar and TLS shows the destination's own certificate.
#   Group 2: access lines and RED series of the same flows (F2 steps 5-6).
# The HTTP paths /prx14 exist only for this check, so an access line for them comes from it alone.

# sidecar_series <pod> <metric> [label=value...]: sum of the series of the app pod's sidecar that match
# the labels (0 when none), read over loopback in the app container as tcp_count does.
sidecar_series() {
  kubectl -n inj-13 exec -i "$1" -c app -- python3 - "$2" "${@:3}" <<'PY'
import re, sys, urllib.request
name, want = sys.argv[1], dict(arg.split("=", 1) for arg in sys.argv[2:])
text = urllib.request.urlopen("http://127.0.0.1:15100/metrics", timeout=5).read().decode()
total = 0.0
for line in text.splitlines():
    if not line.startswith(name + "{"):
        continue
    labels, _, rest = line[len(name) + 1:].partition("} ")
    have = dict(re.findall(r'(\w+)="((?:[^"\\]|\\.)*)"', labels))
    if all(have.get(k) == v for k, v in want.items()):
        total += float(rest.split()[0])
print(int(total))
PY
}
# access_count <pod> <jq boolean>: access lines of the app pod's sidecar for which the boolean holds.
access_count() {
  kubectl -n inj-13 logs "$1" -c mimic-sidecar \
    | jq -R -r "fromjson? | select(.target == \"access\" and ($2)) | 1" \
    | wc -l | tr -d ' '
}
# wait_access <pod> <jq boolean>: polls up to 10 s until an access line matches.
wait_access() {
  local count=0
  for _ in $(seq 10); do
    count=$(access_count "$1" "$2") || true
    [[ ${count:-0} -ge 1 ]] && return 0
    sleep 1
  done
  return 1
}
# access_dump <pod>: the sidecar's access lines to stderr, for a failed check.
access_dump() {
  kubectl -n inj-13 logs "$1" -c mimic-sidecar 2>/dev/null | grep '"target":"access"' >&2 || true
}
# wait_series <pod> <before> <metric> [label=value...]: polls up to 10 s until the sum exceeds <before>.
wait_series() {
  local now=0 pod=$1 before=$2
  shift 2
  for _ in $(seq 10); do
    now=$(sidecar_series "$pod" "$@") || true
    (( ${now:-0} > before )) && return 0
    sleep 1
  done
  return 1
}

log "prx_14: the INJ-13 flows complete through the sidecar; TLS shows the destination's certificate"
webhook=$RELEASE-controller.$NS.svc
# Snapshots before the requests below, so the checks of group 2 see exactly their increments.
before_in=$(sidecar_series "$pod" mimic_http_requests_total direction=inbound protocol=http1 \
  authority=app.inj-13.svc:8080 status_class=2xx) || true
before_out=$(sidecar_series "$pod" mimic_http_requests_total direction=outbound protocol=http1 \
  authority=echo.inj-13-up.svc:8080 status_class=2xx) || true
before_tls=$(sidecar_series "$pod" mimic_proxy_connections_total direction=outbound protocol=tls \
  authority="$webhook") || true
kubectl -n inj-13 exec "$pod" -c app -- touch /www/prx14 || fail "prx_14: cannot create /www/prx14 in the app"
kubectl -n inj-13-up exec deploy/echo -- touch /www/prx14 || fail "prx_14: cannot create /www/prx14 in echo"
kubectl -n inj-13-up exec client -- python3 -c \
  'import urllib.request; urllib.request.urlopen("http://app.inj-13.svc:8080/prx14", timeout=10).read()' \
  || fail "prx_14: inbound HTTP request through the sidecar failed"
kubectl -n inj-13 exec "$pod" -c app -- python3 -c \
  'import urllib.request; urllib.request.urlopen("http://echo.inj-13-up.svc:8080/prx14", timeout=10).read()' \
  || fail "prx_14: outbound HTTP request through the sidecar failed"
want_fp=$(secret_key 'tls\.crt' | base64 -d | openssl x509 -outform DER | sha256sum | cut -d' ' -f1) \
  || fail "prx_14: cannot read the webhook certificate"
got_fp=$(kubectl -n inj-13 exec -i "$pod" -c app -- python3 - "$webhook" <<'PY'
import hashlib, socket, ssl, sys
host = sys.argv[1]
ctx = ssl.create_default_context()
ctx.check_hostname = False
ctx.verify_mode = ssl.CERT_NONE  # the proof is the certificate's bytes, not its chain of trust
with socket.create_connection((host, 8443), 10) as sock:
    with ctx.wrap_socket(sock, server_hostname=host) as tls:
        print(hashlib.sha256(tls.getpeercert(binary_form=True)).hexdigest())
        # Close the TLS session (close_notify both ways) before the socket: a server that answers the
        # client's close, or sends late TLS 1.3 session tickets, writes to a socket the client already
        # closed, the client's kernel resets, and the sidecar logs client_abort (a real reset) not ok.
        tls.settimeout(5)
        try:
            tls.unwrap()
        except (OSError, ssl.SSLError):
            pass
PY
) || fail "prx_14: TLS connection to the webhook Service through the sidecar failed"
[[ $got_fp == "$want_fp" ]] || fail "prx_14: TLS through the sidecar does not show the webhook's own certificate"

log "prx_14: access lines and RED series of the same flows"
wait_access "$pod" '.direction == "inbound" and .protocol == "http1" and .method == "GET" and .path == "/prx14" and .status == 200 and .result == "ok" and .authority == "app.inj-13.svc:8080"' \
  || fail "prx_14: no inbound http1 access line for GET /prx14"
wait_access "$pod" '.direction == "outbound" and .protocol == "http1" and .method == "GET" and .path == "/prx14" and .status == 200 and .result == "ok" and .authority == "echo.inj-13-up.svc:8080"' \
  || fail "prx_14: no outbound http1 access line for GET /prx14"
wait_access "$pod" '.direction == "outbound" and .protocol == "tls" and .sni == "mimic-controller.mimic-system.svc" and .result == "ok"' \
  || { access_dump "$pod"; fail "prx_14: no outbound tls access line for the webhook Service"; }
wait_series "$pod" "$before_in" mimic_http_requests_total direction=inbound protocol=http1 \
  authority=app.inj-13.svc:8080 status_class=2xx \
  || fail "prx_14: mimic_http_requests_total inbound 2xx did not increase"
wait_series "$pod" "$before_out" mimic_http_requests_total direction=outbound protocol=http1 \
  authority=echo.inj-13-up.svc:8080 status_class=2xx \
  || fail "prx_14: mimic_http_requests_total outbound 2xx did not increase"
wait_series "$pod" "$before_tls" mimic_proxy_connections_total direction=outbound protocol=tls \
  authority="$webhook" \
  || fail "prx_14: mimic_proxy_connections_total tls did not increase"

log "inj_15: selection follows the namespace and pod labels, and labelling does not restart pods"
ns_fresh inj-15-off
ns_fresh inj-15-sel mimic.platform.io/inject=selected pod-security.kubernetes.io/enforce=privileged
ns_fresh inj-15-on mimic.platform.io/inject=enabled pod-security.kubernetes.io/enforce=privileged
ns_fresh inj-15-run
deploy_sleeper inj-15-off plain
deploy_sleeper inj-15-off labelled "mimic.platform.io/inject: enabled"
deploy_sleeper inj-15-on disabled "mimic.platform.io/inject: disabled"
kubectl apply -f - >/dev/null <<YAML
apiVersion: mimic.platform.io/v1alpha1
kind: MimicWorkload
metadata: {name: web, namespace: inj-15-sel}
spec: {deployment: web}
YAML
# The controller's MimicWorkload cache follows the API server within seconds.
sleep 5
deploy_sleeper inj-15-sel web
deploy_sleeper inj-15-sel other
deploy_sleeper inj-15-run running
for d in inj-15-off/plain inj-15-off/labelled inj-15-on/disabled inj-15-sel/web inj-15-sel/other inj-15-run/running; do
  kubectl -n "${d%/*}" rollout status "deploy/${d#*/}" --timeout=3m >/dev/null \
    || fail "inj_15: deployment $d did not roll out"
done
# has_mimic <namespace> <label selector>: a pod matched carries a mimic.platform.io annotation.
has_mimic() {
  local annotations
  annotations=$(kubectl -n "$1" get pods -l "$2" -o jsonpath='{.items[*].metadata.annotations}')
  [[ $annotations == *'"mimic.platform.io/injected"'* || $annotations == *'"mimic.platform.io/skipped"'* ]]
}
has_mimic inj-15-off app=plain && fail "inj_15: a pod of an unlabelled namespace was changed"
has_mimic inj-15-off app=labelled || fail "inj_15: a pod labelled enabled in an unlabelled namespace was not injected"
has_mimic inj-15-on app=disabled && fail "inj_15: a pod labelled disabled in an enabled namespace was changed"
has_mimic inj-15-sel app=web || fail "inj_15: the deployment named by a MimicWorkload was not injected"
has_mimic inj-15-sel app=other && fail "inj_15: a deployment without a MimicWorkload was injected in a selected namespace"
uid_before=$(kubectl -n inj-15-run get pods -l app=running -o jsonpath='{.items[0].metadata.uid}')
kubectl label namespace inj-15-run mimic.platform.io/inject=enabled --overwrite >/dev/null
sleep 5
[[ $(kubectl -n inj-15-run get pods -l app=running -o jsonpath='{.items[0].metadata.uid}') == "$uid_before" ]] \
  || fail "inj_15: labelling the namespace restarted a running pod"
has_mimic inj-15-run app=running && fail "inj_15: labelling the namespace changed a running pod"
kubectl apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Pod
metadata: {name: not-injected, namespace: $NS, labels: {mimic.platform.io/inject: enabled}}
spec:
  restartPolicy: Never
  automountServiceAccountToken: false
  securityContext: {runAsNonRoot: true, runAsUser: 999, seccompProfile: {type: RuntimeDefault}}
  containers:
    - name: app
      image: $PY
      command: [sleep, "30"]
      securityContext: {allowPrivilegeEscalation: false, readOnlyRootFilesystem: true, capabilities: {drop: [ALL]}}
YAML
[[ -z $(kubectl -n "$NS" get pod not-injected -o jsonpath='{.metadata.annotations.mimic\.platform\.io/injected}{.metadata.annotations.mimic\.platform\.io/skipped}') ]] \
  || fail "inj_15: a pod in the controller's namespace was injected"
kubectl -n "$NS" delete pod not-injected --wait=false >/dev/null

log "inj_16: with the controller down, pods of enabled namespaces fail closed; PSA baseline rejects injected pods"
ns_fresh inj-16-off
ns_fresh inj-16 mimic.platform.io/inject=enabled pod-security.kubernetes.io/enforce=privileged
kubectl -n "$NS" scale deploy/$RELEASE-controller --replicas=0 >/dev/null
kubectl -n "$NS" wait --for=delete pod -l app.kubernetes.io/name=mimic-controller --timeout=120s >/dev/null \
  || fail "inj_16: controller pods still running after scale to 0"
deploy_sleeper inj-16 app
deploy_sleeper inj-16-off plain
kubectl -n inj-16-off wait --for=condition=Available deploy/plain --timeout=120s >/dev/null \
  || fail "inj_16: a pod of an unlabelled namespace was not created while the controller was down"
events=""
for _ in $(seq 60); do
  events=$(kubectl -n inj-16 get events --field-selector reason=FailedCreate -o jsonpath='{.items[*].message}') || true
  [[ $events == *namespace.inject.mimic.platform.io* ]] && break
  sleep 2
done
[[ $events == *namespace.inject.mimic.platform.io* ]] \
  || fail "inj_16: no FailedCreate event naming the webhook in the enabled namespace"
[[ -z $(kubectl -n inj-16 get pods -o name) ]] || fail "inj_16: a pod was created in the enabled namespace while the controller was down"
kubectl -n "$NS" scale deploy/$RELEASE-controller --replicas=2 >/dev/null
kubectl -n "$NS" rollout status deploy/$RELEASE-controller --timeout=2m
kubectl -n inj-16 rollout status deploy/app --timeout=3m \
  || fail "inj_16: deployment did not roll out once the controller was back"
has_mimic inj-16 app=app || fail "inj_16: the pod created after the controller came back is not injected"

ns_fresh inj-16-baseline mimic.platform.io/inject=enabled pod-security.kubernetes.io/enforce=baseline
deploy_sleeper inj-16-baseline app
events=""
for _ in $(seq 60); do
  events=$(kubectl -n inj-16-baseline get events --field-selector reason=FailedCreate -o jsonpath='{.items[*].message}') || true
  [[ $events == *'violates PodSecurity'* ]] && break
  sleep 2
done
[[ $events == *'violates PodSecurity'* ]] \
  || fail "inj_16: the injected pod was not rejected by Pod Security baseline"
[[ -z $(kubectl -n inj-16-baseline get pods -o name) ]] || fail "inj_16: an injected pod violating baseline was admitted"

# --- TLS interception (F3, spec §5, §7.1, §7.2, §12.4 TLS-24..TLS-26) ----------------------------
# Destinations: HTTPS Services with a certificate from the e2e CA (the client images trust it) and
# one from an unknown CA. Clients: one Deployment per runtime (curl, Python urllib and requests,
# Node fetch, the JDK HttpClient), injected, built from a pinned base image with the e2e CA added
# to its trust store. The clients' code talks only to the destinations; mimic is not visible to it.
# Helpers defined here take the namespace as an argument where the other sections use `$NS`.

# tls_pod <client>: the pod of a client Deployment (label app=<client>).
tls_pod() { kubectl -n tls-clients get pods -l "app=$1" -o jsonpath='{.items[0].metadata.name}'; }
# tls_exec <client> <command...>: runs the command in the client's app container; stderr joins stdout.
tls_exec() { local c=$1; shift; kubectl -n tls-clients exec "deploy/$c" -c app -- "$@" 2>&1; }
# sidecar_get <namespace> <pod> <path>: the body of GET <path> on the sidecar's admin port (15100) through a
# port-forward, so the peer is loopback (as for /discovery). Empty when the request fails.
sidecar_get() {
  local port=$((29000 + RANDOM % 1000)) pf out=""
  kubectl -n "$1" port-forward "$2" "$port:15100" >/dev/null 2>&1 &
  pf=$!
  for _ in $(seq 20); do
    out=$(curl -fs "localhost:$port$3") && break
    sleep 0.5
  done
  kill "$pf" 2>/dev/null || true
  wait "$pf" 2>/dev/null || true
  printf '%s\n' "$out"
}
# sidecar_metrics <namespace> <pod>: the Prometheus text of the sidecar's admin port (15100), through a
# port-forward so that the check does not depend on the application image.
sidecar_metrics() { sidecar_get "$1" "$2" /metrics; }
# ready_tls <client>: waits until the client's sidecar holds an intermediate (TLS-15). Before that the
# sidecar passes TLS through (`no_ca`), which would hide what the checks below look for.
ready_tls() {
  local pod
  pod=$(tls_pod "$1")
  for _ in $(seq 90); do
    if sidecar_metrics tls-clients "$pod" | grep -Eq '^mimic_tls_intermediate_expiry_timestamp_seconds [1-9]'; then
      return 0
    fi
    sleep 1
  done
  fail "tls_15: the sidecar of $1 holds no intermediate after 90 s"
}
# tls_line <namespace> <pod> <jq boolean>: waits up to 15 s until a sidecar log line matches.
tls_line() {
  for _ in $(seq 15); do
    if kubectl -n "$1" logs "$2" -c mimic-sidecar | jq -R -e "fromjson? | select($3)" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  return 1
}
# tls_count <namespace> <pod> <jq boolean>: how many sidecar log lines match.
tls_count() {
  kubectl -n "$1" logs "$2" -c mimic-sidecar | jq -R -r "fromjson? | select($3) | 1" | wc -l | tr -d ' '
}
# issue <name> <ca name> <dns name>: a server certificate for the name, signed by the CA in $WORK.
issue() {
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -subj "/CN=$3" \
    -keyout "$WORK/$1.key" -out "$WORK/$1.csr" 2>/dev/null
  printf 'basicConstraints=CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:%s\n' "$3" \
    > "$WORK/$1.ext"
  openssl x509 -req -in "$WORK/$1.csr" -CA "$WORK/$2.crt" -CAkey "$WORK/$2.key" -CAcreateserial \
    -days 30 -extfile "$WORK/$1.ext" -out "$WORK/$1.crt" 2>/dev/null
}
# derive <name> <base image> <RUN instruction> [ENV line]: the client image mimic-e2e-<name> with the
# e2e CA in its system store (Debian and Ubuntu: update-ca-certificates; the curl image: its bundle).
# Node does not read the system store: its ENV names the CA file too (NODE_EXTRA_CA_CERTS). mimic's
# sidecar takes an image's system store as upstream roots, but not NODE_EXTRA_CA_CERTS (spec §7.1).
derive() {
  local dir="$WORK/img-$1" tag="mimic-e2e-$1:$TAG"
  mkdir -p "$dir"
  cp "$WORK/e2e-ca.crt" "$dir/mimic-e2e-ca.crt"
  {
    printf 'FROM %s\nUSER root\n' "$2"
    printf 'COPY mimic-e2e-ca.crt /usr/local/share/ca-certificates/mimic-e2e.crt\n'
    [[ -z ${3:-} ]] || printf 'RUN %s\n' "$3"
    [[ -z ${4:-} ]] || printf 'ENV %s\n' "$4"
  } > "$dir/Dockerfile"
  docker build -q -t "$tag" "$dir" >/dev/null || fail "tls_25: cannot build $tag"
  DERIVED+=("$tag")
  kind load docker-image --name "$CLUSTER" "$tag" >/dev/null
}
# client_deploy <name> <image> [env line]: one injected client Deployment, sleeping until exec'd into.
client_deploy() {
  kubectl apply -f - >/dev/null <<YAML
apiVersion: apps/v1
kind: Deployment
metadata: {name: $1, namespace: tls-clients}
spec:
  replicas: 1
  selector: {matchLabels: {app: $1}}
  template:
    metadata: {labels: {app: $1}}
    spec:
      containers:
        - name: app
          image: $2
          imagePullPolicy: Never
          command: [sleep, "3600"]
          ${3:+$3}
          volumeMounts: [{name: scripts, mountPath: /scripts, readOnly: true}]
      volumes:
        - name: scripts
          configMap: {name: tls-scripts}
YAML
}
# dest_deploy <name>: an HTTPS destination (Deployment and Service on port 443) with the certificate
# Secret <name>-cert.
dest_deploy() {
  kubectl apply -f - >/dev/null <<YAML
apiVersion: apps/v1
kind: Deployment
metadata: {name: $1, namespace: tls-dest}
spec:
  replicas: 1
  selector: {matchLabels: {app: $1}}
  template:
    metadata: {labels: {app: $1}}
    spec:
      containers:
        - name: dest
          image: $PY
          command: [python3, /code/dest.py]
          ports: [{containerPort: 8443}]
          volumeMounts:
            - {name: code, mountPath: /code, readOnly: true}
            - {name: certs, mountPath: /certs, readOnly: true}
      volumes:
        - {name: code, configMap: {name: dest-code}}
        - {name: certs, secret: {secretName: $1-cert}}
---
apiVersion: v1
kind: Service
metadata: {name: $1, namespace: tls-dest}
spec:
  selector: {app: $1}
  ports: [{name: https, port: 443, targetPort: 8443}]
YAML
}

log "tls_24: chart pieces: sidecar API Service port, NetworkPolicy, seed Secret and its RBAC, CRD tls.exclude"
seed_of() { kubectl -n "$NS" get secret $RELEASE-tls-ca -o jsonpath='{.data.seed}'; }
seed0=$(seed_of)
[[ $(printf '%s' "$seed0" | base64 -d | wc -c) == 32 ]] || fail "tls_24: the seed Secret does not hold a 32-byte seed"
[[ $(kubectl -n "$NS" get svc $RELEASE-controller -o jsonpath='{.spec.ports[?(@.name=="sidecar-api")].port}') == 8444 ]] \
  || fail "tls_24: the controller Service has no sidecar-api port 8444"
[[ $(kubectl -n "$NS" get networkpolicy $RELEASE-controller -o jsonpath='{.spec.ingress[*].ports[*].port}') == *8444* ]] \
  || fail "tls_24: the controller NetworkPolicy does not allow port 8444"
CAN_ID=tls_24 can yes "get the TLS seed Secret" get secret/$RELEASE-tls-ca -n "$NS"
CAN_ID=tls_24 can yes "update the TLS seed Secret" update secret/$RELEASE-tls-ca -n "$NS"
CAN_ID=tls_24 can no "get another Secret" get secret/some-other -n "$NS"
admin_forward
curl -fs localhost:18081/metrics | grep '^mimic_tls_seed_loaded 1' >/dev/null || fail "tls_24: mimic_tls_seed_loaded is not 1"
exclude_max=$(seq -s, -f 'h%g.example.test' 0 256)
accepted "a MimicWorkload with tls.exclude: exact host, suffix and IP" workload tls1 \
  'spec: {deployment: api, tls: {exclude: [api.example.com, "*.corp.example", 10.0.0.1]}}'
denied "a tls.exclude entry *" workload tls2 'spec: {deployment: api, tls: {exclude: ["*"]}}'
denied "a tls.exclude entry a.*.com" workload tls3 'spec: {deployment: api, tls: {exclude: ["a.*.com"]}}'
denied "257 tls.exclude entries" workload tls4 "spec: {deployment: api, tls: {exclude: [$exclude_max]}}"
kubectl -n default delete mimicworkload tls1 --ignore-not-found >/dev/null

log "tls_25: destinations (an HTTPS Service signed by the e2e CA, one signed by an unknown CA) and client images"
ns_fresh tls-dest
ns_fresh tls-clients mimic.platform.io/inject=enabled pod-security.kubernetes.io/enforce=privileged
WORK=$(mktemp -d)
for ca in "e2e-ca|mimic e2e test CA" "untrusted-ca|mimic untrusted test CA"; do
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 30 \
    -keyout "$WORK/${ca%%|*}.key" -out "$WORK/${ca%%|*}.crt" -subj "/CN=${ca#*|}" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null \
    || fail "tls_25: cannot create the CA ${ca%%|*}"
done
issue dest e2e-ca tls-dest.tls-dest.svc
issue untrusted untrusted-ca tls-untrusted.tls-dest.svc
kubectl -n tls-dest create secret tls tls-dest-cert --cert="$WORK/dest.crt" --key="$WORK/dest.key" >/dev/null
kubectl -n tls-dest create secret tls tls-untrusted-cert --cert="$WORK/untrusted.crt" --key="$WORK/untrusted.key" >/dev/null
cat > "$WORK/dest.py" <<'PY'
# The HTTPS destination of the e2e: 200 with the body "ok" for any GET, over the certificate in /certs.
import http.server
import ssl

CONTEXT = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
CONTEXT.load_cert_chain("/certs/tls.crt", "/certs/tls.key")


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def setup(self):
        self.request = CONTEXT.wrap_socket(self.request, server_side=True)
        super().setup()

    def do_GET(self):
        body = b"ok\n"
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


http.server.ThreadingHTTPServer(("0.0.0.0", 8443), Handler).serve_forever()
PY
kubectl -n tls-dest create configmap dest-code --from-file=dest.py="$WORK/dest.py" >/dev/null
dest_deploy tls-dest
dest_deploy tls-untrusted
kubectl -n tls-dest rollout status deploy/tls-dest --timeout=2m >/dev/null || fail "tls_25: tls-dest did not roll out"
kubectl -n tls-dest rollout status deploy/tls-untrusted --timeout=2m >/dev/null || fail "tls_25: tls-untrusted did not roll out"

# The pinned base images (images/Dockerfile, #273), with the e2e CA in their trust store.
CURL_BASE=$(scripts/pinned-image.sh e2e-curl)
PY_SLIM=$(scripts/pinned-image.sh e2e-python)
NODE_SLIM=$(scripts/pinned-image.sh e2e-node)
JDK=$(scripts/pinned-image.sh e2e-jdk)
derive curl "$CURL_BASE" 'cat /usr/local/share/ca-certificates/mimic-e2e.crt >> /etc/ssl/certs/ca-certificates.crt'
derive python "$PY_SLIM" 'update-ca-certificates && pip install --no-cache-dir requests==2.32.5'
derive node "$NODE_SLIM" 'apt-get update && apt-get install -y --no-install-recommends ca-certificates && update-ca-certificates && rm -rf /var/lib/apt/lists/*' \
  'NODE_EXTRA_CA_CERTS=/usr/local/share/ca-certificates/mimic-e2e.crt'
derive jdk "$JDK" 'update-ca-certificates'

cat > "$WORK/client.py" <<'PY'
# usage: client.py <url> <host>. A urllib GET (exit 1 when it fails), then the issuer of the
# certificate the client saw, then a requests GET.
import socket
import ssl
import sys
import urllib.request

url, host = sys.argv[1], sys.argv[2]
try:
    with urllib.request.urlopen(url, timeout=10) as response:
        print(f"urllib status={response.status}")
except Exception as error:
    print(f"urllib error={type(error).__name__}: {error}")
    sys.exit(1)
with socket.create_connection((host, 443), 10) as sock:
    with ssl.create_default_context().wrap_socket(sock, server_hostname=host) as tls:
        cert = tls.getpeercert()
issuer = ",".join(value for rdn in cert["issuer"] for key, value in rdn if key == "commonName")
print(f"issuer={issuer}")
import requests

print(f"requests status={requests.get(url, timeout=10).status_code}")
PY
cat > "$WORK/client.mjs" <<'JS'
// usage: node client.mjs <url> <host>: a fetch, then the issuer of the certificate the client saw.
import tls from 'node:tls';

const [url, host] = process.argv.slice(2);
const response = await fetch(url);
await response.text();
const issuer = await new Promise((resolve, reject) => {
  const socket = tls.connect({ host, port: 443, servername: host }, () => {
    resolve(socket.getPeerCertificate().issuer.CN);
    socket.end();
  });
  socket.on('error', reject);
});
console.log(`node status=${response.status} issuer=${issuer}`);
JS
cat > "$WORK/Client.java" <<'JAVA'
// usage: java Client.java <url> <host>: an HttpClient GET, then the issuer of the certificate.
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.security.cert.X509Certificate;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.SSLSocketFactory;

public class Client {
    public static void main(String[] args) throws Exception {
        URI uri = URI.create(args[0]);
        HttpResponse<String> response = HttpClient.newHttpClient()
            .send(HttpRequest.newBuilder(uri).build(), HttpResponse.BodyHandlers.ofString());
        try (SSLSocket socket = (SSLSocket) SSLSocketFactory.getDefault().createSocket(args[1], 443)) {
            socket.startHandshake();
            X509Certificate cert = (X509Certificate) socket.getSession().getPeerCertificates()[0];
            System.out.println("java status=" + response.statusCode()
                + " issuer=" + cert.getIssuerX500Principal().getName());
        }
    }
}
JAVA
kubectl -n tls-clients create configmap tls-scripts --from-file=client.py="$WORK/client.py" \
  --from-file=client.mjs="$WORK/client.mjs" --from-file=Client.java="$WORK/Client.java" \
  --from-file=only-e2e.pem="$WORK/e2e-ca.crt" >/dev/null
client_deploy curl "mimic-e2e-curl:$TAG"
client_deploy python "mimic-e2e-python:$TAG"
client_deploy node "mimic-e2e-node:$TAG"
client_deploy jvm "mimic-e2e-jdk:$TAG"
# SSL_CERT_FILE in the spec is kept by mimic; the file holds only the e2e CA (TLS-26).
client_deploy pinned "mimic-e2e-python:$TAG" 'env: [{name: SSL_CERT_FILE, value: /scripts/only-e2e.pem}]'
for c in curl python node jvm pinned; do
  kubectl -n tls-clients rollout status "deploy/$c" --timeout=4m >/dev/null || fail "tls_25: client $c did not roll out"
done

dest=tls-dest.tls-dest.svc
untrusted=tls-untrusted.tls-dest.svc
pinned_pod=$(tls_pod pinned)
[[ $(kubectl -n tls-clients get pod "$pinned_pod" -o jsonpath='{.spec.containers[?(@.name=="app")].env[?(@.name=="SSL_CERT_FILE")].value}') == /scripts/only-e2e.pem ]] \
  || fail "tls_26: the spec's SSL_CERT_FILE was replaced"

log "tls_25: curl, Python (urllib, requests), Node and the JDK reach the HTTPS destination through mimic"
for c in curl python node jvm; do ready_tls "$c"; done
out=$(tls_exec curl curl -sv -o /dev/null -w 'curl status=%{http_code}\n' "https://$dest/tls25/curl") \
  || fail "tls_25: curl call failed: $out"
grep -q 'curl status=200' <<<"$out" && grep -q 'mimic intermediate' <<<"$out" \
  || fail "tls_25: curl did not get 200 with a certificate of the mimic intermediate"
out=$(tls_exec python python3 /scripts/client.py "https://$dest/tls25/python" "$dest") \
  || fail "tls_25: python call failed: $out"
grep -q 'urllib status=200' <<<"$out" && grep -q 'requests status=200' <<<"$out" \
  && grep -q 'issuer=mimic intermediate' <<<"$out" || fail "tls_25: python: $out"
out=$(tls_exec node node /scripts/client.mjs "https://$dest/tls25/node" "$dest") || fail "tls_25: node call failed: $out"
grep -q 'node status=200 issuer=mimic intermediate' <<<"$out" || fail "tls_25: node: $out"
out=$(tls_exec jvm java /scripts/Client.java "https://$dest/tls25/jvm" "$dest") || fail "tls_25: jvm call failed: $out"
grep -q 'java status=200 issuer=.*CN=mimic intermediate' <<<"$out" || fail "tls_25: jvm: $out"
for c in curl python node jvm; do
  pod=$(tls_pod "$c")
  tls_line tls-clients "$pod" '.target=="access" and .tls==true and .method=="GET" and .path=="/tls25/'"$c"'" and .status==200 and .authority=="'"$dest"'"' \
    || { kubectl -n tls-clients logs "$pod" -c mimic-sidecar | grep '"target":"access"' >&2 || true; fail "tls_25: no decrypted exchange line with tls true for $c"; }
done

rc=0; out=$(tls_exec curl curl -sS -o /dev/null "https://$untrusted/tls25/untrusted") || rc=$?
[[ $rc == 60 ]] || fail "tls_25: curl to the unknown CA exited $rc, want 60 as without mimic: $out"
rc=0; tls_exec curl curl -sS -o /dev/null "https://$untrusted/tls25/untrusted" >/dev/null || rc=$?
[[ $rc == 60 ]] || fail "tls_25: the second call to the unknown CA exited $rc, want 60"
curl_pod=$(tls_pod curl)
tls_line tls-clients "$curl_pod" '.target=="access" and .protocol=="tls" and .sni=="'"$untrusted"'" and .interception=="upstream_untrusted"' \
  || fail "tls_25: no connection line interception upstream_untrusted for $untrusted"
[[ $(tls_count tls-clients "$curl_pod" '.message=="tls passthrough" and .sni=="'"$untrusted"'"') == 1 ]] \
  || fail "tls_25: the passthrough WARN is not logged exactly once for $untrusted"
[[ $(tls_count tls-clients "$curl_pod" '.level=="WARN" and .message=="tls passthrough" and .reason=="upstream_untrusted"') == 1 ]] \
  || fail "tls_25: the WARN does not name reason upstream_untrusted"

log "tls_25: spec.tls.exclude: a MimicWorkload naming the destination makes the next pod's connections excluded"
kubectl apply -f - >/dev/null <<YAML
apiVersion: mimic.platform.io/v1alpha1
kind: MimicWorkload
metadata: {name: excluded, namespace: tls-clients}
spec: {deployment: excluded, tls: {exclude: [$dest]}}
YAML
# The controller's MimicWorkload cache follows the API server within seconds (as in INJ-15).
sleep 5
client_deploy excluded "mimic-e2e-curl:$TAG"
kubectl -n tls-clients rollout status deploy/excluded --timeout=4m >/dev/null || fail "tls_25: the excluded client did not roll out"
excluded_pod=$(tls_pod excluded)
[[ $(kubectl -n tls-clients get pod "$excluded_pod" -o jsonpath='{.spec.initContainers[?(@.name=="mimic-sidecar")].env[?(@.name=="MIMIC_TLS_EXCLUDE")].value}') == "$dest" ]] \
  || fail "tls_25: the pod does not carry MIMIC_TLS_EXCLUDE from the MimicWorkload"
ready_tls excluded
out=$(tls_exec excluded curl -sv -o /dev/null -w 'curl status=%{http_code}\n' "https://$dest/tls25/excluded") \
  || fail "tls_25: the excluded call failed: $out"
grep -q 'curl status=200' <<<"$out" && grep -q 'mimic e2e test CA' <<<"$out" \
  || fail "tls_25: the excluded connection did not reach the destination's own certificate"
tls_line tls-clients "$excluded_pod" '.target=="access" and .protocol=="tls" and .sni=="'"$dest"'" and .interception=="excluded"' \
  || fail "tls_25: the connection is not logged as interception excluded"

log "tls_26: a client trusting only the e2e CA: client_rejected once, then the destination's own certificate"
ready_tls pinned
rc=0; out=$(tls_exec pinned python3 /scripts/client.py "https://$dest/tls26/first" "$dest") || rc=$?
[[ $rc != 0 ]] || fail "tls_26: the first call succeeded, so mimic's certificate was accepted by a client that trusts only the e2e CA"
# The failure must be the trust decision (mimic's root is not in the client's store), not another verification error.
grep -Eq 'unable to get local issuer|self-signed certificate in certificate chain' <<<"$out" \
  || fail "tls_26: the first call failed for another reason than the untrusted mimic root: $out"
out=$(tls_exec pinned python3 /scripts/client.py "https://$dest/tls26/second" "$dest") \
  || fail "tls_26: the second call failed: $out"
grep -q 'urllib status=200' <<<"$out" && grep -q 'issuer=mimic e2e test CA' <<<"$out" \
  || fail "tls_26: the second call did not get the destination's own certificate: $out"
pinned_pod=$(tls_pod pinned)
tls_line tls-clients "$pinned_pod" '.level=="WARN" and .message=="tls passthrough" and .reason=="client_rejected"' \
  || fail "tls_26: no WARN tls passthrough with reason client_rejected"
[[ $(tls_count tls-clients "$pinned_pod" '.message=="tls passthrough"') == 1 ]] \
  || fail "tls_26: the passthrough WARN is not logged exactly once"
tls_line tls-clients "$pinned_pod" '.target=="access" and .interception=="client_rejected" and .sni=="'"$dest"'"' \
  || fail "tls_26: no connection line interception client_rejected"

# The sidecar API (spec §7.2): a token bound to a pod of deployment curl is refused for another name.
log "tls_26: the sidecar API refuses a token for another deployment (403); the token and the key appear in no log"
kubectl -n "$NS" port-forward svc/$RELEASE-controller 18444:8444 >/dev/null 2>&1 &
PIDS+=("$!")
secret_key 'ca\.crt' | base64 -d > "$WORK/webhook-ca.crt"
openssl ecparam -name prime256v1 -genkey -noout -out "$WORK/sidecar.key" 2>/dev/null
pubkey=$(openssl pkey -in "$WORK/sidecar.key" -pubout -outform DER | base64 -w0)
token=$(kubectl -n tls-clients create token default --audience mimic.platform.io \
  --bound-object-kind Pod --bound-object-name "$curl_pod" --duration 10m) || fail "tls_26: cannot create a token"
# sidecar_api <deployment>: the status of an intermediate request with the token bound to $curl_pod.
sidecar_api() {
  curl -s -o /dev/null -w '%{http_code}' --cacert "$WORK/webhook-ca.crt" \
    --resolve "$RELEASE-controller.$NS.svc:18444:127.0.0.1" -H "Authorization: Bearer $token" \
    -H 'Content-Type: application/json' -d "{\"deployment\":\"$1\",\"public_key\":\"$pubkey\"}" \
    "https://$RELEASE-controller.$NS.svc:18444/v1/tls/intermediate"
}
for _ in $(seq 30); do [[ $(sidecar_api curl) == 200 ]] && break; sleep 1; done
[[ $(sidecar_api curl) == 200 ]] || fail "tls_26: the sidecar API did not issue an intermediate to the curl pod"
[[ $(sidecar_api other) == 403 ]] || fail "tls_26: the sidecar API accepted a token for another deployment"
logs="$WORK/all.log"
{
  for p in $(kubectl -n "$NS" get pods -l app.kubernetes.io/name=mimic-controller -o name); do
    kubectl -n "$NS" logs "$p" --all-containers
  done
  for p in $(kubectl -n tls-clients get pods -o name); do kubectl -n tls-clients logs "$p" --all-containers; done
} > "$logs"
grep -F -q "$token" "$logs" && fail "tls_26: the projected token appears in a log line"
grep -F -q "$pubkey" "$logs" && fail "tls_26: the posted public key appears in a log line"
grep -q 'PRIVATE KEY' "$logs" && fail "tls_26: a PRIVATE KEY appears in a log line"
grep -q '"target":"sidecar_api"' "$logs" || fail "tls_26: no sidecar_api log line"

log "tls_24: the seed survives a controller restart; the test namespaces are removed"
kubectl -n "$NS" rollout restart deploy/$RELEASE-controller >/dev/null
kubectl -n "$NS" rollout status deploy/$RELEASE-controller --timeout=2m
[[ $(seed_of) == "$seed0" ]] || fail "tls_24: the seed changed across a controller restart"
kubectl -n tls-clients delete pods --all --wait=false >/dev/null
kubectl delete namespace tls-clients tls-dest --wait=false >/dev/null

# --- Endpoint discovery (F4, F5, spec §6, §7, §12.5 DSC-20, DSC-22, CTL-20, CTL-21) ---------------------------------------
# The CRD takes spec.discovery.redact: an entry with /, an empty entry and 65 entries are refused.
log "dsc_20: the CRD accepts spec.discovery.redact and rejects an entry with /, an empty entry and 65 entries"
accepted "a MimicWorkload with spec.discovery.redact" workload dsc20-ok \
  "spec: {deployment: api, discovery: {redact: [x-tenant-key, pin]}}"
kubectl -n default delete mimicworkload dsc20-ok --ignore-not-found >/dev/null
denied "a spec.discovery.redact entry with /" workload dsc20-slash "spec: {deployment: api, discovery: {redact: [a/b]}}"
denied "an empty spec.discovery.redact entry" workload dsc20-empty 'spec: {deployment: api, discovery: {redact: [""]}}'
denied "65 spec.discovery.redact entries" workload dsc20-many \
  "spec: {deployment: api, discovery: {redact: [$(seq -s, -f 'r%g' 0 64)]}}"

# endpoints <report file>: the endpoints of a /discovery report (spec §6) as a JSON array. The template
# tree is walked: a parameter is named after the literal before it, {<literal>_id}, or {param<n>}.
cat > "$WORK/endpoints.jq" <<'JQ'
def sanitize: ascii_downcase | gsub("[^a-z0-9]"; "_");
def walk_node($node; $prefix; $depth; $lit):
  ( ($node.endpoints // {}) | to_entries[]
    | {method: .key, template: (if $prefix == "" then "/" else $prefix end), stats: .value} ),
  ( ($node.literals // {}) | to_entries[]
    | walk_node(.value; "\($prefix)/\(.key)"; $depth + 1; .key) ),
  ( $node.param // empty
    | walk_node(.; "\($prefix)/{\(if ($lit // "") != "" then ($lit | sanitize) + "_id" else "param\($depth + 1)" end)}"; $depth + 1; null) );
[ .scopes | to_entries[] | .key as $direction | .value | to_entries[] | .key as $port | .value.root
  | walk_node(.; ""; 0; null) | {direction: $direction, port: $port, method, template, stats} ]
JQ
endpoints() { jq -c -f "$WORK/endpoints.jq" "$1"; }

# ui_forward: (re)starts the port-forward to the read API (MIMIC_UI_ADDR, loopback in the pod) of a controller
# pod on localhost:18082. A forward goes to one pod, not to a Service, so it follows the pod it was given.
ui_forward() {
  local pod
  if [[ -n ${UI_PF:-} ]]; then kill "$UI_PF" 2>/dev/null || true; wait "$UI_PF" 2>/dev/null || true; fi
  pod=$(kubectl -n "$NS" get pods -l app.kubernetes.io/name=mimic-controller --field-selector=status.phase=Running \
    -o go-template='{{range .items}}{{if not .metadata.deletionTimestamp}}{{.metadata.name}}{{"\n"}}{{end}}{{end}}' | head -n 1)
  [[ -n $pod ]] || fail "ctl_20: no controller pod to forward to"
  kubectl -n "$NS" port-forward "pod/$pod" 18082:8080 >/dev/null 2>&1 &
  UI_PF=$!
  PIDS+=("$UI_PF")
  for _ in $(seq 30); do curl -fs localhost:18082/v1/workloads >/dev/null && return 0; sleep 1; done
  fail "ctl_20: port-forward to the read API of $pod"
}
# fetch_merged: the merged report of the users deployment, from the read API, into $WORK/merged.json.
# Fails while the controller has none (404) or the forward is broken.
fetch_merged() { curl -fs --max-time 10 localhost:18082/v1/workloads/disc-22/users/report > "$WORK/merged.json"; }
# merged_pods: the pods the controller has seen push for the users deployment, one per line.
merged_pods() { curl -fs --max-time 10 localhost:18082/v1/workloads/disc-22/users | jq -r '.pods | keys[]'; }
# merged_count <method>: the 200s of <method> /users/{users_id} (inbound) in $WORK/merged.json.
merged_count() {
  endpoints "$WORK/merged.json" | jq --arg m "$1" \
    '[.[] | select(.direction == "inbound" and .method == $m and .template == "/users/{users_id}") | .stats.statuses["200"] // 0] | add // 0'
}
# merged_templates: the inbound endpoints of $WORK/merged.json, one "method template" per line, sorted.
merged_templates() { endpoints "$WORK/merged.json" | jq -r '.[] | select(.direction == "inbound") | "\(.method) \(.template)"' | sort; }
# wait_merged <get 200s> <put 200s> <seconds>: polls the read API until the merged counts are exactly those.
wait_merged() {
  local end=$((SECONDS + $3))
  while (( SECONDS < end )); do
    if fetch_merged 2>/dev/null && [[ $(merged_count GET) == "$1" && $(merged_count PUT) == "$2" ]]; then return 0; fi
    sleep 3
  done
  return 1
}
# valkey_dump: every value of the store as text, whatever the type of the key.
valkey_dump() {
  local key kind
  for key in $(kubectl -n "$NS" exec statefulset/$RELEASE-valkey -- valkey-cli --raw --scan --pattern 'mimic:v1:*'); do
    kind=$(kubectl -n "$NS" exec statefulset/$RELEASE-valkey -- valkey-cli --raw TYPE "$key")
    case $kind in
      string) kubectl -n "$NS" exec statefulset/$RELEASE-valkey -- valkey-cli --raw GET "$key" ;;
      hash) kubectl -n "$NS" exec statefulset/$RELEASE-valkey -- valkey-cli --raw HGETALL "$key" ;;
      *) kubectl -n "$NS" exec statefulset/$RELEASE-valkey -- valkey-cli --raw DUMP "$key" ;;
    esac
  done
}

# The e2e apps (scripts/e2e/apps): B, the users API with two replicas behind the Service users, in an
# enabled namespace; A (front) forwards to B, and the client calls A, in a namespace mimic does not inject.
log "dsc_20: a newly created pod gets the sorted union of the MimicWorkloads' redact names; a deployment without one gets none"
ns_fresh disc-22-client
ns_fresh disc-22 mimic.platform.io/inject=enabled pod-security.kubernetes.io/enforce=privileged
kubectl apply -f - >/dev/null <<YAML
apiVersion: mimic.platform.io/v1alpha1
kind: MimicWorkload
metadata: {name: users-a, namespace: disc-22}
spec: {deployment: users, discovery: {redact: [x-tenant-key, pin]}}
---
apiVersion: mimic.platform.io/v1alpha1
kind: MimicWorkload
metadata: {name: users-b, namespace: disc-22}
spec: {deployment: users, discovery: {redact: [pin, badge]}}
YAML
# The controller's MimicWorkload cache follows the API server within seconds (as in INJ-15).
sleep 5
for ns in disc-22 disc-22-client; do
  kubectl -n "$ns" create configmap apps-code --from-file=service.py=scripts/e2e/apps/service.py >/dev/null
done
kubectl -n disc-22 apply -f scripts/e2e/apps/users.yaml >/dev/null
kubectl -n disc-22-client apply -f scripts/e2e/apps/front.yaml >/dev/null
deploy_sleeper disc-22 plain
kubectl -n disc-22 rollout status deploy/users --timeout=3m \
  || { kubectl -n disc-22 describe pods -l app=users >&2 || true; fail "dsc_22: deployment users did not roll out"; }
kubectl -n disc-22 rollout status deploy/plain --timeout=3m >/dev/null || fail "dsc_20: deployment plain did not roll out"
kubectl -n disc-22-client rollout status deploy/front --timeout=3m >/dev/null || fail "dsc_22: deployment front did not roll out"
kubectl -n disc-22-client wait --for=condition=Ready pod/client --timeout=180s >/dev/null || fail "dsc_22: client pod not Ready"
for p in $(kubectl -n disc-22 get pods -l app=users -o jsonpath='{.items[*].metadata.name}'); do
  [[ $(jp disc-22 "$p" '{.spec.initContainers[?(@.name=="mimic-sidecar")].env[?(@.name=="MIMIC_DISCOVERY_REDACT_NAMES")].value}') == badge,pin,x-tenant-key ]] \
    || fail "dsc_20: pod $p does not carry MIMIC_DISCOVERY_REDACT_NAMES=badge,pin,x-tenant-key"
done
plain_pod=$(kubectl -n disc-22 get pods -l app=plain -o jsonpath='{.items[0].metadata.name}')
has_mimic disc-22 app=plain || fail "dsc_20: the plain deployment was not injected"
[[ -z $(jp disc-22 "$plain_pod" '{.spec.initContainers[?(@.name=="mimic-sidecar")].env[?(@.name=="MIMIC_DISCOVERY_REDACT_NAMES")].value}') ]] \
  || fail "dsc_20: a deployment without a MimicWorkload carries MIMIC_DISCOVERY_REDACT_NAMES"

log "dsc_22: calls through A to /users/<id> with a canary token, an x-tenant-key canary and JSON bodies show up per pod, redacted"
token="dsc22-token-$(openssl rand -hex 12)"
tenant="dsc22-tenant-$(openssl rand -hex 12)"
secret="dsc22-password-$(openssl rand -hex 12)"
canaries=("$token" "$tenant" "$secret")
# kube-proxy programs a Ready endpoint a little after the rollout reports it: warm up through the Service
# until each pod has logged a request of its own, so that the calls below reach both pods. The
# readiness probes log /users/0 (from the pod itself), so the warm-up path is /users/9999 (numeric, so that it joins the {users_id} template).
warm_seen() {
  local seen=0 p
  for p in $(kubectl -n disc-22 get pods -l app=users -o jsonpath='{.items[*].metadata.name}'); do
    if (( $(kubectl -n disc-22 logs "$p" -c app | grep -c '"path": "/users/9999"' || true) > 0 )); then seen=$((seen + 1)); fi
  done
  echo "$seen"
}
# warm_ok counts the calls that got a 200: they are recorded like any other (CTL-20 counts them).
warm_ok=0
for _ in $(seq 60); do
  if kubectl -n disc-22-client exec client -- python3 -c \
    'import urllib.request; urllib.request.urlopen("http://front.disc-22-client.svc:8080/front/users/9999", timeout=5).read()' \
    >/dev/null 2>&1; then warm_ok=$((warm_ok + 1)); fi
  [[ $(warm_seen) == 2 ]] && break
  sleep 1
done
[[ $(warm_seen) == 2 ]] || fail "dsc_22: the Service does not reach both pods"
# 40 GETs and 5 PUTs (a canary in the JSON body), spread over both pods; the client counts its 200s.
batch=$(kubectl -n disc-22-client exec -i client -- python3 - "$token" "$tenant" "$secret" <<'PY'
import json, sys, urllib.request
token, tenant, secret = sys.argv[1:4]
base = "http://front.disc-22-client.svc:8080/front/users"
ids = [str(n) for n in range(1, 40)] + ["3f2b9c1d-8a7e-4c5b-9d6e-1a2b3c4d5e6f"]
auth = {"Authorization": f"Bearer {token}", "X-Tenant-Key": tenant}
gets = puts = 0
for uid in ids:
    with urllib.request.urlopen(urllib.request.Request(f"{base}/{uid}", headers=auth), timeout=10) as r:
        r.read()
        gets += r.status == 200
for uid in ids[:5]:
    body = json.dumps({"name": "user " + uid, "password": secret}).encode()
    headers = dict(auth, **{"Content-Type": "application/json"})
    with urllib.request.urlopen(urllib.request.Request(f"{base}/{uid}", data=body, method="PUT", headers=headers), timeout=10) as r:
        r.read()
        puts += r.status == 200
print(gets, puts)
PY
) || fail "dsc_22: the calls through front failed"
[[ $batch == "40 5" ]] || fail "dsc_22: the client got '$batch' 200s for 40 GETs and 5 PUTs"
want_get=$((warm_ok + 40))
want_put=5

dsc_pods=$(kubectl -n disc-22 get pods -l app=users -o jsonpath='{.items[*].metadata.name}')
# What each pod's /discovery holds now, before the pushes: used for the canary checks (it holds only what
# the pod has not shipped yet, DSC-22 as amended by CTL-20).
for p in $dsc_pods; do sidecar_get disc-22 "$p" /discovery > "$WORK/discovery-$p.json"; done
# What each pod's own log says it answered: the merged report must be the sum of both.
pod_gets() {
  kubectl -n disc-22 logs "$1" -c app \
    | jq -R -s 'split("\n") | map(fromjson? | select(. != null and .method == "GET" and .status == 200 and (.path | startswith("/users/")) and .path != "/users/0")) | length'
}
# users_ready <report file>: GET /users/{users_id} has a 200, a response schema with a name, samples whose
# x-tenant-key is <redacted> and whose Authorization is Bearer <redacted>.
users_ready() {
  endpoints "$1" | jq -e '
    .[] | select(.direction == "inbound" and .method == "GET" and .template == "/users/{users_id}")
    | select((.stats.statuses["200"] // 0) > 0
      and ((.stats.observed.response.bodies["application/json"].json.object.props // {}) | has("name"))
      and ((.stats.samples | length) > 0)
      and ([.stats.samples[].request.headers[] | select((.[0] | ascii_downcase) == "x-tenant-key") | .[1]] | any(. == "<redacted>"))
      and ([.stats.samples[].request.headers[] | select((.[0] | ascii_downcase) == "authorization") | .[1]] | any(. == "Bearer <redacted>")))
  ' >/dev/null
}
log "ctl_20: within 2 push intervals the merged report, read through the controller, holds the calls of both pods"
ui_forward
# Default push interval (30 s, the first push jittered within it): two intervals plus the request time.
wait_merged "$want_get" "$want_put" 120 \
  || { echo "ctl_20: merged GET $(merged_count GET) of $want_get, PUT $(merged_count PUT) of $want_put" >&2; \
       kubectl -n disc-22 logs "$(echo $dsc_pods | cut -d' ' -f1)" -c mimic-sidecar --tail=30 >&2 || true; \
       fail "ctl_20: the merged report does not hold $want_get GETs and $want_put PUTs with a 200"; }
users_ready "$WORK/merged.json" || fail "ctl_20: the merged report has no GET /users/{users_id} with a response schema and redacted samples"
endpoints "$WORK/merged.json" | jq -e '.[] | select(.direction == "inbound" and .method == "PUT" and .template == "/users/{users_id}")
  | select((.stats.samples | length) > 0 and ((.stats.observed.response.bodies["application/json"].json.object.props // {}) | has("updated")))' >/dev/null \
  || fail "ctl_20: the merged report has no PUT /users/{users_id} with a response schema and samples"
[[ $(merged_templates) == $'GET /users/{users_id}\nPUT /users/{users_id}' ]] \
  || fail "ctl_20: the merged inbound endpoints are not GET and PUT /users/{users_id}: $(merged_templates | tr '\n' ';')"
# The union of the pods: both pods answered, and the merged count is the sum of what each answered.
sum_gets=0
for p in $dsc_pods; do
  n=$(pod_gets "$p")
  (( n > 0 )) || fail "ctl_20: pod $p answered no GET /users/<id>"
  sum_gets=$((sum_gets + n))
done
[[ $sum_gets == "$want_get" ]] || fail "ctl_20: the pods answered $sum_gets GETs, the client got $want_get 200s"
[[ $(merged_pods | sort | tr '\n' ' ') == "$(echo $dsc_pods | tr ' ' '\n' | sort | tr '\n' ' ')" ]] \
  || fail "ctl_20: the controller does not list both pods: $(merged_pods | tr '\n' ' ')"
cp "$WORK/merged.json" "$WORK/merged-first.json"
merged_templates > "$WORK/templates-first.txt"

# wait_status <jq expression> <seconds>: polls until the expression holds for the status.discovery of users-a
# and users-b, the MimicWorkloads of the users deployment.
wait_status() {
  local end=$((SECONDS + $2)) w ok
  while (( SECONDS < end )); do
    ok=1
    for w in users-a users-b; do
      kubectl -n disc-22 get mimicworkload "$w" -o json 2>/dev/null | jq -e ".status.discovery | $1" >/dev/null || ok=0
    done
    (( ok )) && return 0
    sleep 2
  done
  return 1
}
log "ctl_20: within MIMIC_STATUS_INTERVAL more the MimicWorkloads show endpoints.inbound 2, pods 2 and a lastPushTime"
wait_status '.endpoints.inbound == 2 and .pods == 2 and (.lastPushTime | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]{8}Z$"))' 90 \
  || fail "ctl_20: the status of users-a and users-b is not endpoints.inbound 2, pods 2 with a lastPushTime: $(kubectl -n disc-22 get mimicworkload -o jsonpath='{.items[*].status}')"
status_row=$(kubectl -n disc-22 get mimicworkload users-a --no-headers) || fail "ctl_19: cannot print users-a"
read -r -a row <<< "$status_row"
[[ ${row[1]} == discovery && ${row[2]} == 2 && ${row[3]} == 2 && ${row[4]} =~ ^[0-9]+[smhd]([0-9]+[smhd])?$ ]] \
  || fail "ctl_20: kubectl get mimicworkload prints '$status_row', want Mode discovery, Endpoints 2, Pods 2 and a Last push age"
header=$(kubectl -n disc-22 get mimicworkload users-a | head -n 1)
[[ $header == *"MODE"*"ENDPOINTS"*"PODS"*"LAST PUSH"* ]] || fail "ctl_19: the printed columns are '$header'"
kubectl -n disc-22 get mimicworkload users-a -o json --show-managed-fields \
  | jq -e '[.metadata.managedFields[] | select(.manager == "mimic-controller" and .operation == "Apply" and .subresource == "status")] | length == 1' >/dev/null \
  || fail "ctl_18: the status is not managed by mimic-controller through a server-side apply on the status subresource"

# No canary in any /discovery output or pod log (DSC-22, DSC-15).
for p in $dsc_pods; do
  for c in "${canaries[@]}"; do
    if grep -F "$c" "$WORK/discovery-$p.json" >/dev/null; then fail "dsc_22: a canary appears in the /discovery of $p"; fi
  done
done
{
  for p in $dsc_pods; do kubectl -n disc-22 logs "$p" --all-containers; done
  kubectl -n disc-22-client logs deploy/front --all-containers
  kubectl -n disc-22-client logs client
} > "$WORK/dsc22.log"
for c in "${canaries[@]}"; do
  if grep -F "$c" "$WORK/dsc22.log" >/dev/null; then fail "dsc_22: a canary appears in a pod log"; fi
done
# The merged report, every value in Valkey and the controller's logs hold no canary either (CTL-20).
valkey_dump > "$WORK/valkey.dump" || fail "ctl_20: cannot read the Valkey values"
[[ -s $WORK/valkey.dump ]] || fail "ctl_20: the Valkey dump is empty"
kubectl -n "$NS" logs -l app.kubernetes.io/name=mimic-controller --all-containers --tail=-1 > "$WORK/controller.log"
for c in "${canaries[@]}"; do
  if grep -F "$c" "$WORK/merged.json" >/dev/null; then fail "ctl_20: a canary appears in the merged report"; fi
  if grep -F "$c" "$WORK/valkey.dump" >/dev/null; then fail "ctl_20: a canary appears in a Valkey value"; fi
  if grep -F "$c" "$WORK/controller.log" >/dev/null; then fail "ctl_20: a canary appears in the controller logs"; fi
done
# The logs explain the traffic: B logs its GETs, A logs its forwarded calls.
jq -R -e 'fromjson? | select(.app == "users" and .method == "GET" and .status == 200 and (.path | startswith("/users/")))' \
  "$WORK/dsc22.log" >/dev/null || fail "dsc_22: no access line for GET /users/<id> in the users pods"
jq -R -e 'fromjson? | select(.app == "front" and .method == "GET" and .status == 200 and (.path | startswith("/front/users/")))' \
  "$WORK/dsc22.log" >/dev/null || fail "dsc_22: no access line for the forwarded calls in front"

# dsc_templates <pods...>: the inbound endpoints of each pod's /discovery, one "pod method template" per line.
dsc_templates() {
  local p
  for p in "$@"; do
    sidecar_get disc-22 "$p" /discovery > "$WORK/templates-$p.json"
    endpoints "$WORK/templates-$p.json" | jq -r '.[] | select(.direction == "inbound") | "\(.method) \(.template)"' \
      | sed "s/^/$p /"
  done | sort
}
# not_found_total <pods...>: the sum over the pods of mimic_discovery_exchanges_total{direction="inbound",result="not_found"}.
not_found_total() {
  local total=0 p n
  for p in "$@"; do
    n=$(sidecar_metrics disc-22 "$p" \
      | awk 'index($0, "mimic_discovery_exchanges_total{direction=\"inbound\",result=\"not_found\"}") == 1 { printf "%d\n", $2 }' \
      | tail -1)
    total=$((total + ${n:-0}))
  done
  echo "$total"
}

log "dsc_22: a 404 storm on random root paths, through front, creates no endpoint"
before_tpl=$(dsc_templates $dsc_pods) || fail "dsc_22: cannot read the endpoints before the storm"
before_404=$(not_found_total $dsc_pods)
hits=$(kubectl -n disc-22-client exec -i client -- python3 - <<'PY'
import os, urllib.error, urllib.request
hits = 0
for _ in range(100):
    try:
        urllib.request.urlopen(f"http://front.disc-22-client.svc:8080/front/{os.urandom(8).hex()}", timeout=10).read()
    except urllib.error.HTTPError as e:
        hits += e.code == 404
print(hits)
PY
) || fail "dsc_22: the 404 storm failed"
[[ $hits == 100 ]] || fail "dsc_22: $hits of 100 random root paths answered 404"
after_404=$before_404
for _ in $(seq 20); do
  after_404=$(not_found_total $dsc_pods)
  (( after_404 >= before_404 + 100 )) && break
  sleep 1
done
(( after_404 >= before_404 + 100 )) || fail "dsc_22: the 404s were not counted as not_found ($before_404 -> $after_404)"
after_tpl=$(dsc_templates $dsc_pods) || fail "dsc_22: cannot read the endpoints after the storm"
[[ $after_tpl == "$before_tpl" ]] || fail "dsc_22: the 404 storm changed the endpoints"

log "ctl_21: a deleted pod's data stays in the merged report"
gone=$(echo $dsc_pods | cut -d' ' -f1)
kubectl -n disc-22 delete pod "$gone" --wait >/dev/null || fail "ctl_21: cannot delete pod $gone"
kubectl -n disc-22 rollout status deploy/users --timeout=3m >/dev/null || fail "ctl_21: users did not recover from the deleted pod"
# The pod flushed at SIGTERM: nothing was pending, so the count is the same, and its pod entry stays.
fetch_merged || fail "ctl_21: cannot read the merged report after the pod was deleted"
[[ $(merged_count GET) == "$want_get" && $(merged_count PUT) == "$want_put" ]] \
  || fail "ctl_21: deleting pod $gone changed the merged counts to $(merged_count GET) and $(merged_count PUT)"
pods_now=$'\n'$(merged_pods)$'\n'
[[ $pods_now == *$'\n'"$gone"$'\n'* ]] || fail "ctl_21: the deleted pod $gone is gone from the controller's list"

log "ctl_21: churn: a rollout and a deleted controller pod, with traffic before, during and after"
# The pods of the status count for 30 s after their last push, and the status is applied every 5 s.
install --set controller.status.podWindow=30s --set controller.status.interval=5s \
  || fail "ctl_21: helm upgrade with MIMIC_STATUS_POD_WINDOW=30s"
kubectl -n "$NS" rollout status deploy/$RELEASE-controller --timeout=3m >/dev/null || fail "ctl_21: the controller did not roll out"
# The client calls for 210 s and counts the 200s it gets; the merged count must be exactly that on top of
# what the report held (no exchange lost at SIGTERM of the old pods, none counted twice by a retry).
kubectl -n disc-22-client exec -i client -- python3 - 210 > "$WORK/churn.out" <<'PY' &
import sys, time, urllib.request
end = time.time() + int(sys.argv[1])
ok = n = 0
while time.time() < end:
    n += 1
    try:
        with urllib.request.urlopen(f"http://front.disc-22-client.svc:8080/front/users/{n % 50 + 1}", timeout=15) as r:
            r.read()
            ok += r.status == 200
    except Exception:
        pass
    time.sleep(0.1)
print(ok)
PY
churn_pid=$!
PIDS+=("$churn_pid")
sleep 20
kubectl -n disc-22 rollout restart deploy/users >/dev/null
kubectl -n disc-22 rollout status deploy/users --timeout=3m >/dev/null || fail "ctl_21: users did not roll out"
controller_gone=$(kubectl -n "$NS" get pods -l app.kubernetes.io/name=mimic-controller -o jsonpath='{.items[0].metadata.name}')
kubectl -n "$NS" delete pod "$controller_gone" --wait=false >/dev/null || fail "ctl_21: cannot delete controller pod $controller_gone"
# The traffic goes on: the old pods leave the status after their window and the new ones are counted,
# with the same endpoints.
wait_status '.pods == 2 and .endpoints.inbound == 2' 120 \
  || fail "ctl_21: status.discovery.pods did not come back to 2 with the same endpoints: $(kubectl -n disc-22 get mimicworkload -o jsonpath='{.items[*].status}')"
wait "$churn_pid" || fail "ctl_21: the client of the churn failed"
churn_ok=$(cat "$WORK/churn.out")
[[ $churn_ok =~ ^[0-9]+$ ]] && (( churn_ok > 100 )) || fail "ctl_21: the churn client got '$churn_ok' 200s"
kubectl -n "$NS" rollout status deploy/$RELEASE-controller --timeout=3m >/dev/null || fail "ctl_21: the controller did not recover"
ui_forward
want_get=$((want_get + churn_ok))
wait_merged "$want_get" "$want_put" 150 \
  || { echo "ctl_21: merged GET $(merged_count GET), expected $want_get (client 200s: $churn_ok)" >&2; fail "ctl_21: the merged count is not the number of 200s the client received"; }
[[ $(merged_templates) == "$(cat "$WORK/templates-first.txt")" ]] \
  || fail "ctl_21: the endpoints changed during the churn: $(merged_templates | tr '\n' ';')"
kubectl delete namespace disc-22 disc-22-client --wait=false >/dev/null

log "inj_23: helm uninstall leaves the CRD and a MimicWorkload; a reinstall with the same release name keeps it"
kubectl apply -f - >/dev/null <<YAML
apiVersion: mimic.platform.io/v1alpha1
kind: MimicWorkload
metadata: {name: kept, namespace: default}
spec: {deployment: api}
YAML
helm uninstall "$RELEASE" -n "$NS" --wait --timeout 4m >/dev/null || fail "inj_23: helm uninstall failed"
kubectl -n "$NS" get secret $RELEASE-tls-ca >/dev/null 2>&1 && fail "tls_24: helm uninstall left the TLS seed Secret"
kubectl get crd mimicworkloads.mimic.platform.io >/dev/null 2>&1 || fail "inj_23: the CRD was removed by helm uninstall"
[[ $(kubectl -n default get mimicworkload kept -o jsonpath='{.spec.deployment}' 2>/dev/null) == api ]] \
  || fail "inj_23: the MimicWorkload was removed by helm uninstall"
install || fail "inj_23: reinstall with the same release name failed"
kubectl -n "$NS" rollout status deploy/$RELEASE-controller --timeout=2m
[[ $(kubectl -n default get mimicworkload kept -o jsonpath='{.spec.deployment}' 2>/dev/null) == api ]] \
  || fail "inj_23: the MimicWorkload was not kept by the reinstall"
kubectl -n default delete mimicworkload kept --ignore-not-found >/dev/null

log "e2e passed on kubernetes $server"
