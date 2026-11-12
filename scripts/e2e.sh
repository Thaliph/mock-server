#!/usr/bin/env bash
# End-to-end checks on kind (MON-02, MON-21, FND-08, FND-09, INJ-11..INJ-17, INJ-23, PRX-14, TLS-24..TLS-26, DSC-20, DSC-22, CTL-19..CTL-21, UI-11, UI-12, UI-14, EGR-19, SHR-19, SHR-20, PUL-19, MCK-19, LDT-16, LDT-19 E via scripts/e2e-load.sh --suite).
# Idempotent:
# reuses the cluster if it exists; the scenario namespaces are recreated on each run.
#
#   TAG=e2e-<sha>-<pid>                         tag of the images built here (unique per run, so
#                                               another session's image on this daemon is never used)
#   IMAGE=ghcr.io/thaliph/mimic:$TAG            image to test (built here unless SKIP_BUILD=1)
#   INIT_IMAGE=ghcr.io/thaliph/mimic-init:$TAG  mimic-init image (built here unless SKIP_BUILD=1)
#   CLUSTER=mimic-e2e                           kind cluster name
#   NODE_IMAGE=kindest/node:v1.33.12@sha256:…    kind node image (default: kind's own)
#   KEEP_CLUSTER=1                              keep the cluster afterwards (default: keep locally, delete in CI)
set -Eeuo pipefail
# A command that fails outside a condition ends the run: name it, so that no exit (141 from a SIGPIPE included) is
# silent. errtrace (-E) carries the trap into functions and command substitutions.
trap 'e2e_status=$?; echo "FAIL line $LINENO: \"$BASH_COMMAND\" exited with status $e2e_status" >&2' ERR
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

# found <grep args>: 0 on a match, 1 on none; an error of grep (status 2: unreadable file, bad
# pattern) stops the script instead of reading as "no match". The arguments are not printed.
found() {
  local status=0
  grep "$@" >/dev/null || status=$?
  if (( status > 1 )); then
    echo "FAIL grep exited with status $status" >&2
    exit 1
  fi
  return "$status"
}

# kill_tree <pid>: the process and everything it started.
kill_tree() {
  local children child
  children=$(pgrep -P "$1" 2>/dev/null || true)
  kill "$1" 2>/dev/null || true
  for child in $children; do kill_tree "$child"; done
}
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
  [[ -z ${CHROME:-} ]] || docker rm -f "$CHROME" >/dev/null 2>&1 || true
  [[ -z $WORK ]] || rm -rf "$WORK"
  [[ $KEEP_CLUSTER == 1 ]] || kind delete cluster --name "$CLUSTER"
}
trap cleanup EXIT

clusters=$(kind get clusters) || { echo "FAIL kind get clusters failed" >&2; exit 1; }
if ! found -x "$CLUSTER" <<<"$clusters"; then
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
outside=$(probe default "app: probe")
if found PONG <<<"$outside"; then
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
  # The kept cluster's Valkey outlives the namespace: drop what an earlier run stored for its
  # deployments, or merged counts (ctl_20) add up across runs. No Valkey yet: nothing to drop.
  kubectl -n "$NS" exec statefulset/$RELEASE-valkey -- sh -c \
    'valkey-cli --raw --scan --pattern "mimic:v1:{$0/*" | xargs -r valkey-cli DEL' "$1" \
    >/dev/null 2>&1 || true
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
log "ctl_19: the CRD has the status subresource and the printer columns Mode, Endpoints, Pods, Last push, Mode pods"
crd_columns=$(kubectl get crd mimicworkloads.mimic.platform.io -o jsonpath='{range .spec.versions[0].additionalPrinterColumns[*]}{.name}{"|"}{end}')
[[ $crd_columns == 'Mode|Endpoints|Pods|Last push|Mode pods|' ]] || fail "ctl_19: printer columns are '$crd_columns'"
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
# The client prints the fingerprint of the certificate it saw, then that certificate as PEM (for a failed check).
prx14_seen() {
  kubectl -n inj-13 exec -i "$pod" -c app -- python3 - "$webhook" <<'PY'
import hashlib, socket, ssl, sys
host = sys.argv[1]
ctx = ssl.create_default_context()
ctx.check_hostname = False
ctx.verify_mode = ssl.CERT_NONE  # the proof is the certificate's bytes, not its chain of trust
with socket.create_connection((host, 8443), 10) as sock:
    with ctx.wrap_socket(sock, server_hostname=host) as tls:
        der = tls.getpeercert(binary_form=True)
        print(hashlib.sha256(der).hexdigest())
        print(ssl.DER_cert_to_PEM_cert(der), end="")
        # Close the TLS session (close_notify both ways) before the socket: a server that answers the
        # client's close, or sends late TLS 1.3 session tickets, writes to a socket the client already
        # closed, the client's kernel resets, and the sidecar logs client_abort (a real reset) not ok.
        tls.settimeout(5)
        try:
            tls.unwrap()
        except (OSError, ssl.SSLError):
            pass
PY
}
# The sidecar passes an unverifiable destination through, even on a first connection (TLS-18, #331): a mismatch
# here is a rotation of the webhook certificate that one controller replica does not serve yet. Both sides are read
# again until they agree; one that lasts 60 s fails with both certificates printed.
for _ in $(seq 30); do
  want_pem=$(secret_key 'tls\.crt' | base64 -d) || fail "prx_14: cannot read the webhook certificate"
  want_fp=$(openssl x509 -outform DER <<<"$want_pem" | sha256sum | cut -d' ' -f1) \
    || fail "prx_14: cannot read the webhook certificate"
  got_out=$(prx14_seen) || fail "prx_14: TLS connection to the webhook Service through the sidecar failed"
  got_fp=$(awk 'NR == 1' <<<"$got_out")
  [[ $got_fp == "$want_fp" ]] && break
  sleep 2
done
if [[ $got_fp != "$want_fp" ]]; then
  # Both certificates and what the sidecar did with the connections (#331).
  {
    echo "prx_14: expected certificate (the webhook Secret), fingerprint $want_fp:"
    openssl x509 -noout -subject -issuer -dates <<<"$want_pem" || true
    echo "prx_14: certificate seen through the sidecar, fingerprint $got_fp:"
    tail -n +2 <<<"$got_out" | openssl x509 -noout -subject -issuer -dates || true
    echo "prx_14: access lines of the sidecar for the webhook Service (interception per connection):"
    kubectl -n inj-13 logs "$pod" -c mimic-sidecar 2>/dev/null \
      | jq -R -c --arg sni "$webhook" 'fromjson? | select(.target == "access" and .protocol == "tls" and .sni == $sni)
          | {time: (.timestamp // .time), interception, result, sni, dst}' || true
  } >&2
  fail "prx_14: TLS through the sidecar does not show the webhook's own certificate"
fi

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
  local pod metrics
  pod=$(tls_pod "$1")
  for _ in $(seq 90); do
    metrics=$(sidecar_metrics tls-clients "$pod") || metrics=""
    if found -E '^mimic_tls_intermediate_expiry_timestamp_seconds [1-9]' <<<"$metrics"; then
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
if found -F "$token" "$logs"; then fail "tls_26: the projected token appears in a log line"; fi
if found -F "$pubkey" "$logs"; then fail "tls_26: the posted public key appears in a log line"; fi
if found 'PRIVATE KEY' "$logs"; then fail "tls_26: a PRIVATE KEY appears in a log line"; fi
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
    -o go-template='{{range .items}}{{if not .metadata.deletionTimestamp}}{{.metadata.name}}{{"\n"}}{{end}}{{end}}' | awk 'NR == 1')
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
  local seen=0 p app_log
  for p in $(kubectl -n disc-22 get pods -l app=users -o jsonpath='{.items[*].metadata.name}'); do
    app_log=$(kubectl -n disc-22 logs "$p" -c app) || fail "disc_22: cannot read the log of $p"
    if found -F '"path": "/users/9999"' <<<"$app_log"; then seen=$((seen + 1)); fi
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
# x-tenant-key (a name of spec.discovery.redact) is mimic-secret and whose Authorization is Bearer mimic-token (PLH-10).
users_ready() {
  endpoints "$1" | jq -e '
    .[] | select(.direction == "inbound" and .method == "GET" and .template == "/users/{users_id}")
    | select((.stats.statuses["200"] // 0) > 0
      and ((.stats.observed.response.bodies["application/json"].json.object.props // {}) | has("name"))
      and ((.stats.samples | length) > 0)
      and ([.stats.samples[].request.headers[] | select((.[0] | ascii_downcase) == "x-tenant-key") | .[1]] | any(. == "mimic-secret"))
      and ([.stats.samples[].request.headers[] | select((.[0] | ascii_downcase) == "authorization") | .[1]] | any(. == "Bearer mimic-token")))
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
# The merged report exists (ctl_20 above), so the status needs one status interval (30 s by default; a replica
# may have just missed the push, so two), the apply, and a margin for a slow runner.
status_wait=$((2 * 30 + 60))
wait_status '.endpoints.inbound == 2 and .pods == 2 and (.lastPushTime | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]{8}Z$"))' "$status_wait" \
  || { echo "ctl_20: status after ${status_wait}s: $(kubectl -n disc-22 get mimicworkload -o jsonpath='{.items[*].status}')" >&2; \
       kubectl -n "$NS" logs deploy/$RELEASE-controller --all-pods --tail=200 2>&1 | grep -i 'status' | tail -20 >&2 || true; \
       fail "ctl_20: the status of users-a and users-b is not endpoints.inbound 2, pods 2 with a lastPushTime within ${status_wait}s"; }
status_row=$(kubectl -n disc-22 get mimicworkload users-a --no-headers) || fail "ctl_19: cannot print users-a"
read -r -a row <<< "$status_row"
[[ ${row[5]:-} =~ ^[0-9]+$ && ${row[1]} == discovery && ${row[2]} == 2 && ${row[3]} == 2 && ${row[4]} =~ ^[0-9]+[smhd]([0-9]+[smhd])?$ ]] \
  || fail "ctl_20: kubectl get mimicworkload prints '$status_row', want Mode discovery, Endpoints 2, Pods 2 and a Last push age"
# A deployment without data prints 0 in the Endpoints column, not a blank (the CRD defaults endpoints.inbound).
kubectl apply -f - >/dev/null <<YAML
apiVersion: mimic.platform.io/v1alpha1
kind: MimicWorkload
metadata: {name: idle, namespace: disc-22}
spec: {deployment: idle}
YAML
end=$((SECONDS + 90)); idle_row=""
while (( SECONDS < end )); do
  idle_row=$(kubectl -n disc-22 get mimicworkload idle --no-headers 2>/dev/null) || idle_row=""
  read -r -a row <<< "$idle_row"
  [[ ${row[2]:-} == 0 ]] && break
  sleep 2
done
[[ ${row[2]:-} == 0 ]] || fail "ctl_19: a MimicWorkload without data prints '$idle_row', want 0 in the Endpoints column"
header=$(kubectl -n disc-22 get mimicworkload users-a | awk 'NR == 1')
[[ $header == *"MODE"*"ENDPOINTS"*"PODS"*"LAST PUSH"* ]] || fail "ctl_19: the printed columns are '$header'"
kubectl -n disc-22 get mimicworkload users-a -o json --show-managed-fields \
  | jq -e '[.metadata.managedFields[] | select(.manager == "mimic-controller" and .operation == "Apply" and .subresource == "status")] | length == 1' >/dev/null \
  || fail "ctl_18: the status is not managed by mimic-controller through a server-side apply on the status subresource"

# No canary in any /discovery output or pod log (DSC-22, DSC-15).
for p in $dsc_pods; do
  for c in "${canaries[@]}"; do
    if found -F "$c" "$WORK/discovery-$p.json"; then fail "dsc_22: a canary appears in the /discovery of $p"; fi
  done
done
# PLH-10: a request to /reset/<JWT-shaped value> through the injected users app. Its sidecar's access line shows
# /reset/mimic-jwt and never the JWT (the app's own log holds the raw path and is not read here).
b64u() { printf '%s' "$1" | base64 -w0 | tr '+/' '-_' | tr -d '='; }
plh_jwt="$(b64u '{"alg":"HS256","typ":"JWT"}').$(b64u "{\"sub\":\"plh10-$(openssl rand -hex 6)\"}").$(openssl rand -hex 16)"
kubectl -n disc-22-client exec client -- python3 -c '
import sys, urllib.error, urllib.request
try:
    urllib.request.urlopen("http://users.disc-22.svc:8080/reset/" + sys.argv[1], timeout=10).read()
except urllib.error.HTTPError:
    pass
' "$plh_jwt" || fail "plh_10: the request to /reset/<JWT> failed"
sleep 2
for p in $dsc_pods; do kubectl -n disc-22 logs "$p" -c mimic-sidecar --tail=-1; done > "$WORK/plh10-sidecar.log"
found -F -e '"path":"/reset/mimic-jwt"' "$WORK/plh10-sidecar.log" || fail "plh_10: no sidecar access line for /reset/mimic-jwt: not found"
if found -F -e "$plh_jwt" "$WORK/plh10-sidecar.log"; then fail "plh_10: the JWT is in a sidecar log: found"; fi
{
  for p in $dsc_pods; do kubectl -n disc-22 logs "$p" --all-containers; done
  kubectl -n disc-22-client logs deploy/front --all-containers
  kubectl -n disc-22-client logs client
} > "$WORK/dsc22.log"
for c in "${canaries[@]}"; do
  if found -F "$c" "$WORK/dsc22.log"; then fail "dsc_22: a canary appears in a pod log"; fi
done
# The merged report, every value in Valkey and the controller's logs hold no canary either (CTL-20).
valkey_dump > "$WORK/valkey.dump" || fail "ctl_20: cannot read the Valkey values"
[[ -s $WORK/valkey.dump ]] || fail "ctl_20: the Valkey dump is empty"
kubectl -n "$NS" logs -l app.kubernetes.io/name=mimic-controller --all-containers --tail=-1 > "$WORK/controller.log"
for c in "${canaries[@]}"; do
  if found -F "$c" "$WORK/merged.json"; then fail "ctl_20: a canary appears in the merged report"; fi
  if found -F "$c" "$WORK/valkey.dump"; then fail "ctl_20: a canary appears in a Valkey value"; fi
  if found -F "$c" "$WORK/controller.log"; then fail "ctl_20: a canary appears in the controller logs"; fi
done
# PLH-10: the merged inbound report holds no legacy marker (the e2e deployments are not authority scopes).
jq -e '[.. | strings | select(contains("<redacted>"))] | length == 0' "$WORK/merged.json" >/dev/null \
  || fail "plh_10: the merged report of the e2e deployment holds <redacted>"
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
# The client calls for 210 s on ids of its own (100000 and up) and counts the 200s it gets. The truth for
# the merged count is what B answered: its access lines, streamed from every pod of the rollout, those of
# the pods that are deleted included. A rollout may cut a connection after the pod answered, so the client
# can miss a 200 that B sent (and the sidecar recorded); it can never see one B did not send.
old_pods=$(kubectl -n disc-22 get pods -l app=users -o jsonpath='{.items[*].metadata.name}')
mkdir -p "$WORK/churn-logs"
(
  while true; do
    # The names go to a file, not to a command substitution: a kubectl that ends early (141, SIGPIPE, or any error
    # while the pods are changing) is then only a loop that lists nothing this second, not a failure of the script.
    kubectl -n disc-22 get pods -l app=users -o jsonpath='{.items[*].metadata.name}' >"$WORK/churn-pods" 2>/dev/null || true
    for p in $(cat "$WORK/churn-pods"); do
      [[ -e "$WORK/churn-logs/$p.seen" ]] && continue
      : > "$WORK/churn-logs/$p.seen"
      # Until a stream ends cleanly (the container exited): one that fails, while the container is still
      # starting for instance, starts again from the first line, in a file of its own (a file is never
      # truncated, so the lines of a pod that is deleted meanwhile are kept; the longest file of a pod counts).
      ( n=0; until kubectl -n disc-22 logs -f "$p" -c app > "$WORK/churn-logs/$p.$((n += 1)).log" 2>/dev/null; do sleep 1; done ) &
    done
    sleep 1
  done
) &
streams_pid=$!
PIDS+=("$streams_pid")
kubectl -n disc-22-client exec -i client -- python3 - 210 > "$WORK/churn.out" <<'PY' &
import sys, time, urllib.request
end = time.time() + int(sys.argv[1])
ok = n = 0
while time.time() < end:
    n += 1
    try:
        with urllib.request.urlopen(f"http://front.disc-22-client.svc:8080/front/users/{n % 50 + 100000}", timeout=15) as r:
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
kubectl -n "$NS" rollout status deploy/$RELEASE-controller --timeout=3m >/dev/null || fail "ctl_21: the controller did not recover"
# The traffic goes on. The pods of the rollout must have pushed (their names are not those from before), and
# then the old pods leave the status after their window: pods 2 on the old pods alone would pass too early.
new_pods=$(comm -13 <(printf '%s\n' $old_pods | sort) \
  <(kubectl -n disc-22 get pods -l app=users -o go-template='{{range .items}}{{if not .metadata.deletionTimestamp}}{{.metadata.name}}{{"\n"}}{{end}}{{end}}' | sort))
[[ $(echo "$new_pods" | wc -w) == 2 ]] || fail "ctl_21: expected 2 new users pods after the rollout, got '$new_pods' (old: $old_pods)"
ui_forward
pushed=0
for _ in $(seq 60); do
  if pods_listed=$(merged_pods 2>/dev/null) && [[ -z $(comm -23 <(echo "$new_pods" | sort) <(echo "$pods_listed" | sort)) ]]; then pushed=1; break; fi
  sleep 2
done
(( pushed )) || fail "ctl_21: the new pods $new_pods did not push: $(merged_pods | tr '\n' ' ')"
wait_status '.pods == 2 and .endpoints.inbound == 2' 120 \
  || fail "ctl_21: status.discovery.pods did not come back to 2 with the same endpoints: $(kubectl -n disc-22 get mimicworkload -o jsonpath='{.items[*].status}')"
wait "$churn_pid" || fail "ctl_21: the client of the churn failed"
churn_ok=$(cat "$WORK/churn.out")
[[ $churn_ok =~ ^[0-9]+$ ]] && (( churn_ok > 100 )) || fail "ctl_21: the churn client got '$churn_ok' 200s"
# The streams of the pods still running hold their last lines a moment after the last answer.
sleep 5
kill_tree "$streams_pid"
# Only the ids of the churn (100000 to 100049, exactly): no earlier step uses them (1 to 39 and 9999).
b_ok=0
for pod in $(ls "$WORK"/churn-logs | sed -n 's/^\(.*\)\.[0-9]*\.log$/\1/p' | sort -u); do
  best=0
  for f in "$WORK/churn-logs/$pod".*.log; do
    n=$(jq -R -s 'split("\n") | map(fromjson? | select(. != null and .method == "GET" and .status == 200 and (.path | test("^/users/1000[0-4][0-9]$")))) | length' "$f") || n=0
    (( n > best )) && best=$n
  done
  b_ok=$((b_ok + best))
done
[[ $b_ok =~ ^[0-9]+$ ]] || fail "ctl_21: cannot count the 200s B answered: '$b_ok'"
echo "ctl_21: the client got $churn_ok 200s, B answered $b_ok" >&2
(( churn_ok <= b_ok )) || fail "ctl_21: the client got $churn_ok 200s, more than the $b_ok B answered"
ui_forward
# Every 200 the client received must be counted (nothing lost at SIGTERM) and nothing counted twice, so the
# merged count is at most what B answered. A rollout that cuts a connection after B answered can also cost the
# sidecar the exchange it was relaying, and the client the answer: the gap below B's count is accepted, and
# printed, only up to the 200s the client did not receive.
base_get=$want_get
min_get=$((base_get + churn_ok))
max_get=$((base_get + b_ok))
wait_merged "$max_get" "$want_put" 90 || true
fetch_merged || fail "ctl_21: cannot read the merged report after the churn"
got_get=$(merged_count GET)
echo "ctl_21: merged GET $got_get, between $min_get (the client's 200s) and $max_get (B's)" >&2
(( got_get >= min_get && got_get <= max_get )) \
  || fail "ctl_21: merged GET $got_get is not in [$min_get, $max_get] (B answered $b_ok, the client got $churn_ok)"
[[ $(merged_count PUT) == "$want_put" ]] || fail "ctl_21: merged PUT $(merged_count PUT), expected $want_put"
[[ $(merged_templates) == "$(cat "$WORK/templates-first.txt")" ]] \
  || fail "ctl_21: the endpoints changed during the churn: $(merged_templates | tr '\n' ';')"

# --- Discovery UI (F6, spec §7.5, §12.7 UI-11, UI-12) -------------------------------------------------------------
# The same apps and the same port-forward to a controller pod. Two more PUTs record hostile bodies: a key and a value
# that are HTML, and a "$ref" that points to a server of this script. Both end up in the users document.
log "ui_11: the read API serves the users document, the endpoints and the pages through the port-forward"
ui_url=http://localhost:18082
ref_port=18090
xss='<img src=/ui-xss onerror=alert(1)>'
kubectl -n disc-22-client exec -i client -- python3 - "$xss" "http://localhost:$ref_port/ref" <<'PY' || fail "ui_11: the hostile PUTs failed"
import json, sys, urllib.request
xss, ref = sys.argv[1:3]
for n, body in ((771, {xss: xss}), (772, {"$ref": ref})):
    request = urllib.request.Request(
        f"http://front.disc-22-client.svc:8080/front/users/{n}", data=json.dumps(body).encode(), method="PUT",
        headers={"Content-Type": "application/json"})
    urllib.request.urlopen(request, timeout=10).read()
PY
# The pods ship within seconds: wait for both bodies to show in the document.
ui_doc_ready() {
  curl -fs --max-time 10 "$ui_url/v1/workloads/disc-22/users/openapi/inbound-8080" > "$WORK/ui-doc.json" 2>/dev/null \
    && jq -e --arg x "$xss" --arg r "http://localhost:$ref_port/ref" \
      '(tojson | contains($x)) and (tojson | contains($r))' \
      "$WORK/ui-doc.json" >/dev/null 2>&1
}
for _ in $(seq 60); do ui_doc_ready && break; sleep 3; done
ui_doc_ready || fail "ui_11: the hostile bodies never reached the users document"

[[ $(curl -fs "$ui_url/v1/workloads/disc-22/users/openapi" | jq -c '[.documents[] | .id]') == '["inbound-8080"]' ]] \
  || fail "ui_11: the users documents are not [inbound-8080]"
jq -e '.openapi == "3.1.1"
  and (.paths["/users/{users_id}"] | has("get") and has("put"))
  and ([.paths["/users/{users_id}"].get.responses["200"].content["application/json"].examples[]?.value] | length > 0)
  and (.paths["/users/{users_id}"].get.security == [{"http_bearer": []}])
  and (.components.securitySchemes.http_bearer == {"type": "http", "scheme": "bearer"})
  and (.servers | map(.url) | index("http://users.disc-22.svc:8080") != null)' "$WORK/ui-doc.json" >/dev/null \
  || fail "ui_11: the users document lacks get/put /users/{users_id}, a 200 example, http_bearer or the users server"
curl -fs "$ui_url/v1/workloads/disc-22/users/endpoints" > "$WORK/ui-endpoints.json" || fail "ui_11: no endpoints route"
[[ $(jq -r '[.endpoints[] | select(.direction == "inbound") | "\(.method) \(.template)"] | sort | join(",")' "$WORK/ui-endpoints.json") \
  == 'GET /users/{users_id},PUT /users/{users_id}' ]] || fail "ui_11: the endpoint list is not GET and PUT /users/{users_id}"
jq -e '[.endpoints[] | select(.method == "GET") | .exchanges] | add >= 40' "$WORK/ui-endpoints.json" >/dev/null \
  || fail "ui_11: the GET endpoint counts fewer than the 40 exchanges of the client"
for page in / /ui/ /ui/workloads/disc-22/users /ui/workloads/disc-22/users/openapi/inbound-8080; do
  curl -s "$ui_url$page" > "$WORK/ui-page.html" || fail "ui_11: cannot read $page"
  for c in "${canaries[@]}"; do
    if found -F "$c" "$WORK/ui-page.html"; then fail "ui_11: a canary appears in the page $page"; fi
  done
done
for c in "${canaries[@]}"; do
  if found -F "$c" "$WORK/ui-doc.json" "$WORK/ui-endpoints.json"; then fail "ui_11: a canary appears in the document or the endpoint list"; fi
done
[[ $(curl -s -I -o /dev/null -w '%{http_code}' "$ui_url/ui/") == 200 ]] || fail "ui_11: HEAD /ui/ is not 200"
[[ $(curl -s -o /dev/null -w '%{http_code}' -X POST "$ui_url/ui/") == 405 ]] || fail "ui_11: POST /ui/ is not 405"
[[ $(curl -s -o /dev/null -w '%{http_code}' "$ui_url/ui/workloads/disc-22/Users") == 400 ]] || fail "ui_11: an invalid name is not 400"
curl -s -D "$WORK/ui-headers.txt" -o /dev/null "$ui_url/ui/" || fail "ui_11: cannot read the headers of /ui/"
tr -d '\r' < "$WORK/ui-headers.txt" | tr 'A-Z' 'a-z' > "$WORK/ui-headers.lower"
for h in "x-content-type-options: nosniff" "referrer-policy: no-referrer" "x-frame-options: deny" "cache-control: no-store" \
  "content-security-policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"; do
  if ! found -Fx "$h" "$WORK/ui-headers.lower"; then fail "ui_11: /ui/ lacks the header '$h'"; fi
done
if found -F 'access-control-allow' "$WORK/ui-headers.lower"; then fail "ui_11: /ui/ has an Access-Control-Allow header"; fi

# UI-14 (spec §12.7): the controller container declares mimic-ui on 8080, and a port-forward to the deployment reaches
# the UI (a forward to a Service or a deployment picks one pod, and the peer is that pod's loopback, so the UI answers).
log "ui_14: the controller declares mimic-ui on 8080, and kubectl port-forward deploy/$RELEASE-controller reaches the UI"
[[ $(kubectl -n "$NS" get deploy "$RELEASE-controller" -o jsonpath='{.spec.template.spec.containers[?(@.name=="controller")].ports[?(@.name=="mimic-ui")].containerPort}') == 8080 ]] \
  || fail "ui_14: the controller container does not declare mimic-ui on 8080"
kubectl -n "$NS" port-forward "deploy/$RELEASE-controller" 18083:8080 >/dev/null 2>&1 &
deploy_pf=$!
PIDS+=("$deploy_pf")
for _ in $(seq 30); do curl -fs localhost:18083/v1/workloads >/dev/null && break; sleep 1; done
[[ $(curl -s -o /dev/null -w '%{http_code}' localhost:18083/ui/) == 200 ]] \
  || fail "ui_14: /ui/ through kubectl port-forward deploy/$RELEASE-controller is not 200"
kill "$deploy_pf" 2>/dev/null || true
wait "$deploy_pf" 2>/dev/null || true

log "ui_12: a pinned headless Chromium, with no name but localhost resolvable, loads the pages"
CHROME_IMAGE=$(scripts/pinned-image.sh e2e-chromium)
chrome_port=19223
CHROME=mimic-e2e-chromium-$$
# The container holds no capability, cannot gain one, and writes only to its tmpfs. --no-sandbox stays: Chromium's own
# sandbox needs user namespaces or CAP_SYS_ADMIN, which the dropped capabilities and the default seccomp profile refuse;
# the container is the sandbox here, and the browser only loads the pages of this script.
docker run -d --rm --name "$CHROME" --network host --cap-drop=ALL --security-opt no-new-privileges --read-only \
  --tmpfs /tmp --tmpfs /root --entrypoint /headless-shell/headless-shell "$CHROME_IMAGE" \
  --no-sandbox --user-data-dir=/tmp/chrome --disable-gpu --remote-debugging-address=127.0.0.1 --remote-debugging-port=$chrome_port \
  --host-resolver-rules='MAP * ~NOTFOUND , EXCLUDE localhost' about:blank >/dev/null || fail "ui_12: cannot start Chromium"
for _ in $(seq 30); do curl -fs "127.0.0.1:$chrome_port/json/version" >/dev/null && break; sleep 1; done
curl -fs "127.0.0.1:$chrome_port/json/version" >/dev/null || fail "ui_12: Chromium does not answer on its DevTools port"
# The server that the "$ref" points to: any request it gets is a failure.
cat > "$WORK/ref_server.py" <<'PY'
import http.server, sys
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        print(self.path, flush=True)
        self.send_response(200)
        self.end_headers()
    do_POST = do_PUT = do_HEAD = do_GET
    def log_message(self, *args):
        pass
http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY
python3 -I "$WORK/ref_server.py" "$ref_port" > "$WORK/ref_hits.log" &
PIDS+=("$!")
# users_log_count: the access lines of the users pods, without the /users/0 probes, summed. Every failure is fatal
# (the caller fails on a non-zero status): no pipe hides a failed kubectl or jq, and no pod at all is a failure.
users_log_count() {
  local pods p log n total=0
  pods=$(kubectl -n disc-22 get pods -l app=users -o jsonpath='{.items[*].metadata.name}') || return 1
  [[ -n $pods ]] || return 1
  for p in $pods; do
    log=$(kubectl -n disc-22 logs "$p" -c app) || return 1
    n=$(jq -R -s 'split("\n") | map(fromjson? | select(. != null and .path != "/users/0")) | length' <<< "$log") || return 1
    [[ $n =~ ^[0-9]+$ ]] || return 1
    total=$((total + n))
  done
  echo "$total"
}
# The client traffic is over (the churn client was waited for, the hostile PUTs returned): the count must hold still.
# users_before > 0 proves the counter sees the traffic, so an equal count afterwards is not a count of nothing.
users_before=$(users_log_count) || fail "ui_12: cannot count the access lines of the users pods"
sleep 3
users_settled=$(users_log_count) || fail "ui_12: cannot count the access lines of the users pods"
[[ $users_before == "$users_settled" ]] || fail "ui_12: the users pods still log requests before the browser run ($users_before, then $users_settled)"
(( users_before > 0 )) || fail "ui_12: the users pods' access lines count 0 after the traffic: the counter sees nothing"

# browse <name> <path> <args...>: the page as the browser saw it, in $WORK/browse-<name>.json; no violation, no failure to
# load, and no request but to the port-forward's origin (a data: image is not a request to an origin).
browse() {
  local name=$1 path=$2; shift 2
  python3 -I scripts/e2e/ui_browser.py "$chrome_port" "$ui_url$path" "$@" > "$WORK/browse-$name.json" \
    || fail "ui_12: $name: the browser did not show the page"
  [[ $(jq '.violations | length' "$WORK/browse-$name.json") == 0 ]] \
    || fail "ui_12: $name: CSP violations: $(jq -c .violations "$WORK/browse-$name.json")"
  [[ $(jq --arg o "$ui_url/" '[.requests[] | select((startswith($o) or startswith("data:")) | not)] | length' "$WORK/browse-$name.json") == 0 ]] \
    || fail "ui_12: $name: requests outside $ui_url: $(jq -c .requests "$WORK/browse-$name.json")"
  [[ $(jq '.requests | length' "$WORK/browse-$name.json") -gt 0 ]] || fail "ui_12: $name: the browser recorded no request"
  [[ $(jq '[.buttons[] | select(test("try it out|execute"; "i"))] | length' "$WORK/browse-$name.json") == 0 ]] \
    || fail "ui_12: $name: a Try it out or Execute button is shown"
  [[ $(jq .xss_imgs "$WORK/browse-$name.json") == 0 ]] || fail "ui_12: $name: the payload became an img element"
}
browse list /ui/ --wait disc-22 --wait users
browse deployment /ui/workloads/disc-22/users --wait '/users/{users_id}' --wait GET --wait PUT
jq -e '.text | test("GET\\s+/users/\\{users_id\\}") and test("PUT\\s+/users/\\{users_id\\}")' "$WORK/browse-deployment.json" >/dev/null \
  || fail "ui_12: the deployment page lacks the GET and PUT /users/{users_id} rows"
# EXP-07: the explorer tabs, the Workloads tab with the mimic resources of users, and the resource view.
browse tabs /ui/ --wait disc-22 --wait users --wait users-a --wait users-b --wait Map --wait Endpoints --wait 'Load tests'
jq -e '.links | map(.href) | contains(["/ui/map", "/ui/endpoints", "/ui/loadtests",
  "/ui/workloads/disc-22/users/resources/MimicWorkload/users-a", "/ui/workloads/disc-22/users/resources/MimicWorkload/users-b"])' \
  "$WORK/browse-tabs.json" >/dev/null \
  || fail "exp_07: the Workloads tab lacks the tab links or the links to the MimicWorkloads of users: $(jq -c '[.links[].href]' "$WORK/browse-tabs.json")"
jq -e '.rows | map(join(" ")) | any(test("disc-22 users discovery"))' "$WORK/browse-tabs.json" >/dev/null \
  || fail "exp_07: the Workloads tab has no row for disc-22/users in discovery: $(jq -c .rows "$WORK/browse-tabs.json")"
browse resource /ui/workloads/disc-22/users/resources/MimicWorkload/users-a --wait 'deployment: "users"' --wait x-tenant-key
jq -e '.text | contains("Spec") and contains("deployment: \"users\"")' "$WORK/browse-resource.json" >/dev/null \
  || fail "exp_07: the resource view does not show the spec of users-a as YAML"
browse tab-map /ui/map --wait 'not available yet'
browse tab-endpoints /ui/endpoints --wait 'Pick a deployment'
browse tab-loadtests /ui/loadtests --wait 'not available here yet'
browse document /ui/workloads/disc-22/users/openapi/inbound-8080 --wait '/users/{users_id}' --expand --interceptor
jq -e '(.methods | map(ascii_upcase) | sort) == ["GET", "PUT"]' "$WORK/browse-document.json" >/dev/null \
  || fail "ui_12: the document page lists the operations $(jq -c .methods "$WORK/browse-document.json"), not GET and PUT"
# The request interceptor refuses by throwing (a returned null falls back to the original request): a cross-origin
# URL, a same-origin URL outside /v1/workloads/ and a malformed one throw; a same-origin /v1/workloads/ URL passes.
jq -e '.interceptor | (.cross_origin.threw and .cross_origin_workloads.threw and .same_origin_outside.threw and .malformed.threw)
  and (.same_origin_workloads | .threw == false and .passed)' "$WORK/browse-document.json" >/dev/null \
  || fail "ui_12: the request interceptor does not refuse as expected: $(jq -c .interceptor "$WORK/browse-document.json")"
# The payload is shown as text, inert: no img element of that src (browse), and the text holds it.
jq -e --arg x "$xss" '.text | contains($x)' "$WORK/browse-document.json" >/dev/null \
  || fail "ui_12: the XSS payload is not shown as text in the document page"
[[ ! -s $WORK/ref_hits.log ]] || fail "ui_12: the server of the \$ref example got a request: $(cat "$WORK/ref_hits.log")"
users_after=$(users_log_count) || fail "ui_12: cannot count the access lines of the users pods after the browser run"
[[ $users_before == "$users_after" ]] || fail "ui_12: the users pods logged $((users_after - users_before)) request(s) during the browser run"
# Chromium keeps running: SHR-20 (egress discovery, below) uses it and removes it.

# --- Egress discovery (F7, F8, spec §6.1, §7.3, §12.8 EGR-19, §12.9 SHR-19, SHR-20) --------------------------------------
# A second front, injected, 2 replicas, in its own namespace with a MimicWorkload: the sidecars record A's calls to B
# (users, disc-22) and the controller aggregates both pods' outbound data. It runs after CTL-20 and CTL-21, whose
# counts of users it would change.
log "egr_19: the sidecars of an injected front with 2 replicas record its calls to users as outbound discovery, redacted; the controller holds the union"
ns_fresh egr-19 mimic.platform.io/inject=enabled pod-security.kubernetes.io/enforce=privileged
kubectl -n egr-19 create configmap apps-code --from-file=service.py=scripts/e2e/apps/service.py >/dev/null
kubectl apply -f - >/dev/null <<YAML
apiVersion: mimic.platform.io/v1alpha1
kind: MimicWorkload
metadata: {name: front, namespace: egr-19}
spec: {deployment: front}
YAML
kubectl -n egr-19 apply -f scripts/e2e/apps/front-injected.yaml >/dev/null
kubectl -n egr-19 rollout status deploy/front --timeout=3m >/dev/null \
  || { kubectl -n egr-19 describe pods -l app=front >&2 || true; fail "egr_19: deployment front did not roll out"; }
has_mimic egr-19 app=front || fail "egr_19: front was not injected"
egr_pods=$(kubectl -n egr-19 get pods -l app=front -o jsonpath='{.items[*].metadata.name}')
[[ $(wc -w <<<"$egr_pods") == 2 ]] || fail "egr_19: front has not 2 pods: '$egr_pods'"
for egr_pod in $egr_pods; do
  [[ -z $(jp egr-19 "$egr_pod" '{.spec.initContainers[?(@.name=="mimic-sidecar")].env[?(@.name=="MIMIC_DISCOVERY_REDACT_NAMES")].value}') ]] \
    || fail "egr_19: front carries MIMIC_DISCOVERY_REDACT_NAMES: the outbound allowlist must do the redaction alone"
done
# Warm-up without any HTTP exchange: a TCP connect to the Service, until kube-proxy routes it.
egr_up=0
for _ in $(seq 60); do
  if kubectl -n disc-22-client exec client -- python3 -c \
    'import socket; socket.create_connection(("front.egr-19.svc", 8080), timeout=3).close()' >/dev/null 2>&1; then egr_up=1; break; fi
  sleep 1
done
(( egr_up )) || fail "egr_19: the Service front.egr-19 does not accept connections"
# kube-proxy programs a Ready endpoint a little after the rollout reports it: warm up through the Service until each
# front pod has answered a call of its own, so that the calls below reach both. The warm-up calls are counted like
# any other (egr_warm_ok): the checks below compare the merged report with the access lines of the pods.
egr_seen() {
  local seen=0 p app_log
  for p in $egr_pods; do
    app_log=$(kubectl -n egr-19 logs "$p" -c app) || fail "egr_19: cannot read the log of $p"
    if found -F '"path": "/front/users/9999"' <<<"$app_log"; then seen=$((seen + 1)); fi
  done
  echo "$seen"
}
egr_warm_ok=0
for _ in $(seq 60); do
  if kubectl -n disc-22-client exec client -- python3 -c \
    'import urllib.request; urllib.request.urlopen("http://front.egr-19.svc:8080/front/users/9999", timeout=5).read()' \
    >/dev/null 2>&1; then egr_warm_ok=$((egr_warm_ok + 1)); fi
  [[ $(egr_seen) == 2 ]] && break
  sleep 1
done
[[ $(egr_seen) == 2 ]] || fail "egr_19: the warm-up did not reach both front pods"

egr_token="egr19-token-$(openssl rand -hex 12)"
egr_tenant="egr19-tenant-$(openssl rand -hex 12)"
egr_secret="egr19-password-$(openssl rand -hex 12)"
egr_canaries=("$egr_token" "$egr_tenant" "$egr_secret")
# 12 GETs (4 with delay_ms=200), 3 PUTs (1 delayed) and 2 calls to a missing path, each a new connection, so that the
# Service spreads them over both pods. The client checks every answer byte for byte against what B must answer: the
# calls reach B unchanged.
egr_batch=$(kubectl -n disc-22-client exec -i client -- python3 - "$egr_token" "$egr_tenant" "$egr_secret" <<'PY'
import json, sys, urllib.error, urllib.request
token, tenant, secret = sys.argv[1:4]
base = "http://front.egr-19.svc:8080/front"
auth = {"Authorization": f"Bearer {token}", "X-Tenant-Key": tenant}
ids = [str(200000 + n) for n in range(1, 13)]
gets = puts = missing = 0
for n, uid in enumerate(ids):
    query = "?delay_ms=200" if n % 3 == 0 else ""
    with urllib.request.urlopen(urllib.request.Request(f"{base}/users/{uid}{query}", headers=auth), timeout=15) as r:
        assert r.status == 200, r.status
        assert json.loads(r.read()) == {"id": uid, "name": "user " + uid, "active": True, "score": 1.5}
        gets += 1
for n, uid in enumerate(ids[:3]):
    query = "?delay_ms=200" if n == 0 else ""
    body = json.dumps({"name": "user " + uid, "password": secret}).encode()
    headers = dict(auth, **{"Content-Type": "application/json"})
    with urllib.request.urlopen(urllib.request.Request(f"{base}/users/{uid}{query}", data=body, method="PUT", headers=headers), timeout=15) as r:
        assert r.status == 200, r.status
        assert json.loads(r.read()) == {"id": uid, "updated": True, "name": "user " + uid}
        puts += 1
for _ in range(2):
    try:
        urllib.request.urlopen(urllib.request.Request(f"{base}/missing", headers=auth), timeout=15).read()
    except urllib.error.HTTPError as e:
        assert e.code == 404, e.code
        assert json.loads(e.read()) == {"error": "not found"}
        missing += 1
print(gets, puts, missing)
PY
) || fail "egr_19: the calls through front failed or an answer differs from what B sends"
[[ $egr_batch == "12 3 2" ]] || fail "egr_19: the client got '$egr_batch' answers for 12 GETs, 3 PUTs and 2 missing"
# The GETs of /users/<id> that got a 200, and all the outbound calls front made: the warm-up calls count.
egr_gets=$((12 + egr_warm_ok))
egr_total=$((egr_gets + 3 + 2))

# B got every call: its access lines, summed over both pods.
: > "$WORK/egr19-users.log"
for p in $(kubectl -n disc-22 get pods -l app=users -o jsonpath='{.items[*].metadata.name}'); do
  kubectl -n disc-22 logs "$p" -c app >> "$WORK/egr19-users.log" || fail "egr_19: cannot read the log of $p"
done
users_lines() {
  jq -R -s --arg m "$1" --arg re "$2" --argjson s "$3" \
    'split("\n") | map(fromjson? | select(. != null and .method == $m and .status == $s and (.path | test($re)))) | length' \
    "$WORK/egr19-users.log"
}
[[ $(users_lines GET '^/users/(2000[0-9]{2}|9999)(\?.*)?$' 200) == "$egr_gets" ]] || fail "egr_19: users did not answer $egr_gets GETs with a 200"
[[ $(users_lines PUT '^/users/2000[0-9]{2}(\?.*)?$' 200) == 3 ]] || fail "egr_19: users did not answer 3 PUTs with a 200"
[[ $(users_lines GET '^/missing$' 404) == 2 ]] || fail "egr_19: users did not answer 2 GETs of /missing with a 404"

# What each front sidecar says it sent (its access lines): the truth for the merged counts. Both pods sent some.
egr_authority="users.disc-22.svc:8080"
for p in $egr_pods; do
  kubectl -n egr-19 logs "$p" -c mimic-sidecar > "$WORK/egr19-sidecar-$p.log" || fail "egr_19: cannot read the sidecar log of $p"
done
# egr_sent <pod|all> <method> <path regex> <status>: outbound http1 access lines of the sidecar of <pod>, or the sum over both.
egr_sent() {
  local pods=$1 sum=0 n p
  [[ $pods == all ]] && pods=$egr_pods
  for p in $pods; do
    n=$(jq -R -s --arg m "$2" --arg re "$3" --argjson s "$4" \
      'split("\n") | map(fromjson? | select(. != null and .target == "access" and .direction == "outbound" and .protocol == "http1"
         and .method == $m and .status == $s and .authority == "users.disc-22.svc:8080" and (.path | test($re)))) | length' \
      "$WORK/egr19-sidecar-$p.log") || fail "egr_19: cannot read the access lines of $p"
    sum=$((sum + n))
  done
  echo "$sum"
}
egr_users_re='^/users/[0-9]+(\?.*)?$'
for p in $egr_pods; do
  (( $(egr_sent "$p" GET "$egr_users_re" 200) > 0 )) || fail "egr_19: the sidecar of $p logged no outbound GET /users/<id>: the calls did not reach both pods"
done
[[ $(egr_sent all GET "$egr_users_re" 200) == "$egr_gets" ]] || fail "egr_19: the sidecars logged $(egr_sent all GET "$egr_users_re" 200) outbound GETs of /users/<id>, not $egr_gets"
[[ $(egr_sent all PUT "$egr_users_re" 200) == 3 ]] || fail "egr_19: the sidecars did not log 3 outbound PUTs of /users/<id>"
[[ $(egr_sent all GET '^/missing$' 404) == 2 ]] || fail "egr_19: the sidecars did not log 2 outbound GETs of /missing with a 404"

# The recorders: polled until the sum of the pods' recorded outbound exchanges is the total (a counter: it does not
# reset when a push ships the data).
egr_recorded() {
  local sum=0 n p
  for p in $egr_pods; do
    n=$(sidecar_get egr-19 "$p" /metrics \
      | awk 'index($0, "mimic_discovery_exchanges_total{direction=\"outbound\",result=\"recorded\"} ") == 1 { printf "%d\n", $2 }' | tail -1)
    sum=$((sum + ${n:-0}))
  done
  echo "$sum"
}
egr_seen_total=0
for _ in $(seq 30); do
  egr_seen_total=$(egr_recorded)
  [[ $egr_seen_total == "$egr_total" ]] && break
  sleep 1
done
[[ $egr_seen_total == "$egr_total" ]] || fail "egr_19: the sidecars of front recorded $egr_seen_total outbound exchanges, not $egr_total"

# Canaries in a report ($1, named $2): all three are checked against its outbound part, the bearer token and the
# password against the whole report (the inbound X-Tenant-Key of an unlisted header is the accepted limit of §11.1).
egr_check_canaries() {
  jq '.scopes.outbound // {}' "$1" > "$WORK/egr19-outbound.json"
  local c
  for c in "${egr_canaries[@]}"; do
    if found -F "$c" "$WORK/egr19-outbound.json"; then fail "egr_19: a canary appears in scopes.outbound of $2"; fi
  done
  for c in "$egr_token" "$egr_secret"; do
    if found -F "$c" "$1"; then fail "egr_19: the bearer token or the password appears in $2"; fi
  done
}
# /discovery holds only what the sidecar has not shipped yet (SHR-19): it is checked for canaries only, on both pods.
egr_check_discovery() {
  local p
  for p in $egr_pods; do
    sidecar_get egr-19 "$p" /discovery > "$WORK/egr19-discovery-$p.json"
    [[ -s $WORK/egr19-discovery-$p.json ]] || fail "egr_19: cannot read /discovery of the sidecar of $p"
    egr_check_canaries "$WORK/egr19-discovery-$p.json" "/discovery of $p"
  done
}
# The outbound endpoints, counts, histograms, origin and samples of the merged report ($1).
egr_check_outbound() {
  local egr_report=$1
  jq -e '.scopes.outbound | type == "object"' "$egr_report" >/dev/null || fail "egr_19: the merged report of front has no scopes.outbound"
  jq -e --arg a "$egr_authority" '.scopes.outbound[$a] | type == "object"' "$egr_report" >/dev/null \
    || fail "egr_19: no scopes.outbound.\"$egr_authority\" in the merged report of front"
  jq -e '(.scopes.outbound | keys) | length == 1' "$egr_report" >/dev/null \
    || fail "egr_19: front has outbound scopes other than users: $(jq -c '.scopes.outbound | keys' "$egr_report")"
  [[ $(endpoints "$egr_report" | jq -r --arg a "$egr_authority" '.[] | select(.direction == "outbound") | select(.port == $a) | "\(.method) \(.template)"' | LC_ALL=C sort | tr '\n' ';') \
     == 'GET /missing;GET /users/{users_id};PUT /users/{users_id};' ]] \
    || fail "egr_19: the outbound endpoints of front are not GET /missing, GET and PUT /users/{users_id}: $(endpoints "$egr_report" | jq -c '[.[] | select(.direction == "outbound") | "\(.method) \(.template)"]')"
  out_status() { endpoints "$egr_report" | jq -r --arg m "$1" --arg t "$2" --arg s "$3" \
    '[.[] | select(.direction == "outbound" and .method == $m and .template == $t) | .stats.statuses[$s] // 0] | add // 0'; }
  # Calls whose head came in 200 ms or later (bucket 31 holds 200 ms), and all the calls of the endpoint.
  out_late() { endpoints "$egr_report" | jq -r --arg m "$1" --arg t "$2" \
    '[.[] | select(.direction == "outbound" and .method == $m and .template == $t) | .stats.latency.buckets | to_entries[] | select((.key | tonumber) >= 31) | .value] | add // 0'; }
  out_total() { endpoints "$egr_report" | jq -r --arg m "$1" --arg t "$2" \
    '[.[] | select(.direction == "outbound" and .method == $m and .template == $t) | .stats.latency.buckets | to_entries[] | .value] | add // 0'; }
  # The counts are the sums of the two pods' access lines.
  [[ $(out_status GET '/users/{users_id}' 200) == "$egr_gets" ]] || fail "egr_19: GET /users/{users_id} does not hold $egr_gets calls with a 200: $(out_status GET '/users/{users_id}' 200)"
  [[ $(out_status PUT '/users/{users_id}' 200) == 3 ]] || fail "egr_19: PUT /users/{users_id} does not hold 3 calls with a 200: $(out_status PUT '/users/{users_id}' 200)"
  [[ $(out_status GET /missing 404) == 2 ]] || fail "egr_19: GET /missing does not hold 2 calls with a 404: $(out_status GET /missing 404)"
  [[ $(out_total GET '/users/{users_id}') == "$egr_gets" && $(out_total PUT '/users/{users_id}') == 3 ]] \
    || fail "egr_19: the latency histograms do not count the calls"
  (( $(out_late GET '/users/{users_id}') >= 4 )) || fail "egr_19: fewer than 4 GETs of /users/{users_id} in a bucket of 200 ms or more: $(out_late GET '/users/{users_id}')"
  (( $(out_late PUT '/users/{users_id}') >= 1 )) || fail "egr_19: no PUT of /users/{users_id} in a bucket of 200 ms or more"
  # The origin: cleartext HTTP, and the original destination (the Service address, port 8080).
  jq -e --arg a "$egr_authority" --argjson n "$egr_total" '.scopes.outbound[$a].origin
    | .schemes == {"http": $n} and ((.dst | length) >= 1) and (.dst | all(test("^[0-9.]+:8080$"))) and ((.sni // []) | length == 0)' \
    "$egr_report" >/dev/null || fail "egr_19: the origin of $egr_authority is not $egr_total http exchanges to a :8080 destination: $(jq -c --arg a "$egr_authority" '.scopes.outbound[$a].origin' "$egr_report")"
  # The samples: the credentials are redacted by the outbound allowlist, whatever the names.
  endpoints "$egr_report" | jq -e '.[] | select(.direction == "outbound" and .method == "GET" and .template == "/users/{users_id}")
    | select((.stats.samples | length) > 0
      and ([.stats.samples[].request.headers[] | select((.[0] | ascii_downcase) == "x-tenant-key") | .[1]] | (length > 0 and all(. == "mimic-secret")))
      and ([.stats.samples[].request.headers[] | select((.[0] | ascii_downcase) == "authorization") | .[1]] | (length > 0 and all(. == "Bearer mimic-token"))))' \
    >/dev/null || fail "egr_19: the outbound GET samples do not hold Authorization: Bearer mimic-token and X-Tenant-Key: mimic-secret (PLH-10)"
  # PLH-10: no legacy marker in the report but in authority hosts (an authority is a scope key, never a value).
  jq -e '[.. | strings | select(contains("<redacted>") and (test("^<redacted>:[0-9]+$") | not))] | length == 0' "$egr_report" >/dev/null \
    || fail "egr_19: the report holds <redacted> outside an authority host (PLH-10)"
  endpoints "$egr_report" | jq -e '.[] | select(.direction == "outbound" and .method == "PUT" and .template == "/users/{users_id}")
    | select((.stats.samples | length) > 0)' >/dev/null || fail "egr_19: the outbound PUT has no sample"
}
egr_check_discovery

# Pushes carry both directions (F8, SHR-19): within 2 push intervals the controller's merged report of front holds the
# inbound GETs and every outbound exchange of both pods (their union); each sidecar resets what it shipped.
ui_forward
egr_merged="$WORK/egr19-merged.json"
egr_merged_get() {
  curl -fs --max-time 10 localhost:18082/v1/workloads/egr-19/front/report > "$egr_merged" 2>/dev/null || return 1
  endpoints "$egr_merged" | jq -e --argjson n "$egr_gets" '[.[] | select(.direction == "inbound" and .method == "GET" and (.template | startswith("/front/users/"))) | .stats.statuses["200"] // 0] | add == $n' >/dev/null \
    && jq -e --arg a "$egr_authority" --argjson n "$egr_total" '.scopes.outbound[$a].origin.schemes.http == $n' "$egr_merged" >/dev/null
}
egr_pushed=0
for _ in $(seq 60); do
  if egr_merged_get; then egr_pushed=1; break; fi
  sleep 3
done
(( egr_pushed )) || fail "egr_19: the controller holds no report of front with its $egr_gets inbound GETs and $egr_total outbound exchanges after 180 s"
jq -e '.version == 2 and ((.scopes | keys) == ["inbound", "outbound"])' "$egr_merged" >/dev/null \
  || fail "egr_19: the merged report of front is not version 2 with inbound and outbound scopes: $(jq -c '[.version, (.scopes | keys)]' "$egr_merged")"
egr_check_outbound "$egr_merged"
egr_check_canaries "$egr_merged" "the merged report of front"
egr_check_discovery
# The controller lists both pods.
curl -fs --max-time 10 localhost:18082/v1/workloads/egr-19/front > "$WORK/egr19-summary.json" || fail "egr_19: cannot read the summary of front"
[[ $(jq -r '.pods | keys | join(" ")' "$WORK/egr19-summary.json") == "$(tr ' ' '\n' <<<"$egr_pods" | LC_ALL=C sort | tr '\n' ' ' | sed 's/ $//')" ]] \
  || fail "egr_19: the controller does not list both front pods: $(jq -c '.pods | keys' "$WORK/egr19-summary.json")"

# The documents: the outbound document of users, in the list of front's documents, with the operations, the server and
# the redacted header.
egr_doc_id="outbound-$egr_authority"
curl -fs --max-time 10 localhost:18082/v1/workloads/egr-19/front/openapi > "$WORK/egr19-docs.json" || fail "egr_19: cannot read the documents of front"
jq -e --arg id "$egr_doc_id" '[.documents[].id] | index($id) != null' "$WORK/egr19-docs.json" >/dev/null \
  || fail "egr_19: the documents of front do not list $egr_doc_id: $(jq -c '[.documents[].id]' "$WORK/egr19-docs.json")"
curl -fs --max-time 10 "localhost:18082/v1/workloads/egr-19/front/openapi/$egr_doc_id" > "$WORK/egr19-doc.json" || fail "egr_19: cannot read the document $egr_doc_id"
jq -e '(.paths["/users/{users_id}"] | has("get") and has("put"))
  and (.servers | map(.url) | index("http://users.disc-22.svc:8080") != null)
  and ([.paths["/users/{users_id}"].get.parameters[]? | select(.in == "header" and (.name | ascii_downcase) == "x-tenant-key") | .["x-mimic-redacted"]] == [true])' \
  "$WORK/egr19-doc.json" >/dev/null \
  || fail "egr_19: the document $egr_doc_id lacks get and put /users/{users_id}, the users server or X-Tenant-Key as a redacted header parameter"

# The status: within MIMIC_STATUS_INTERVAL more the MimicWorkload shows the outbound count and the total.
status_wait=$((2 * 30 + 60))
egr_status_ok() {
  kubectl -n egr-19 get mimicworkload front -o json 2>/dev/null \
    | jq -e '.status.discovery | .endpoints.outbound == 3 and .pods == 2 and .totalEndpoints == (.endpoints.inbound + 3)' >/dev/null
}
end=$((SECONDS + status_wait)); egr_status_set=0
while (( SECONDS < end )); do
  if egr_status_ok; then egr_status_set=1; break; fi
  sleep 2
done
(( egr_status_set )) || { echo "egr_19: status after ${status_wait}s: $(kubectl -n egr-19 get mimicworkload front -o jsonpath='{.status}')" >&2; \
  fail "egr_19: the status of front is not endpoints.outbound 3 and pods 2 with totalEndpoints inbound + 3 within ${status_wait}s"; }
egr_total_endpoints=$(kubectl -n egr-19 get mimicworkload front -o jsonpath='{.status.discovery.totalEndpoints}') || fail "egr_19: cannot read the status of front"
read -r -a row <<< "$(kubectl -n egr-19 get mimicworkload front --no-headers)"
[[ ${row[1]} == discovery && ${row[2]} == "$egr_total_endpoints" && ${row[3]} == 2 ]] \
  || fail "egr_19: kubectl get mimicworkload prints '${row[*]}', want Endpoints $egr_total_endpoints (the total) and Pods 2"

# No canary where the outbound view or the stdout rules apply: scopes.outbound (checked above, before and after the
# push), /metrics, the stdout of front (app and sidecar) and B's logs hold none of the three. The inbound parts (the
# merged report, Valkey) hold neither the bearer token nor the password; the documents and the controller's logs hold
# none of the three.
: > "$WORK/egr19-metrics.txt"
: > "$WORK/egr19-front.log"
for p in $egr_pods; do
  sidecar_get egr-19 "$p" /metrics >> "$WORK/egr19-metrics.txt"
  kubectl -n egr-19 logs "$p" --all-containers >> "$WORK/egr19-front.log" || fail "egr_19: cannot read the logs of $p"
done
[[ -s $WORK/egr19-metrics.txt ]] || fail "egr_19: cannot read the metrics of the sidecars of front"
kubectl -n disc-22-client logs client >> "$WORK/egr19-front.log" || fail "egr_19: cannot read the logs of the client"
[[ -s $WORK/egr19-front.log ]] || fail "egr_19: the logs of front are empty"
jq -R -e 'fromjson? | select(.app == "front" and .method == "GET" and .status == 200 and (.path | startswith("/front/users/")))' \
  "$WORK/egr19-front.log" >/dev/null || fail "egr_19: no access line of front for its forwarded calls"
valkey_dump > "$WORK/egr19-valkey.dump" || fail "egr_19: cannot read the Valkey values"
[[ -s $WORK/egr19-valkey.dump ]] || fail "egr_19: the Valkey dump is empty"
kubectl -n "$NS" logs -l app.kubernetes.io/name=mimic-controller --all-containers --tail=-1 > "$WORK/egr19-controller.log" || fail "egr_19: cannot read the controller logs"
for c in "${egr_canaries[@]}"; do
  for f in "$WORK/egr19-outbound.json" "$WORK/egr19-metrics.txt" "$WORK/egr19-front.log" "$WORK/egr19-users.log" \
           "$WORK/egr19-docs.json" "$WORK/egr19-doc.json" "$WORK/egr19-controller.log"; do
    if found -F "$c" "$f"; then fail "egr_19: a canary appears in ${f##*/}"; fi
  done
done
for c in "$egr_token" "$egr_secret"; do
  for f in "$egr_merged" "$WORK/egr19-valkey.dump"; do
    if found -F "$c" "$f"; then fail "egr_19: the bearer token or the password appears in ${f##*/}"; fi
  done
done

# --- PUL-19: the injected front pods pull their deployment's egress from the controller (F9, spec §7.7) -------------------
# The traffic of SHR-19 is over. Within 2 push intervals (30 s) plus 2 pull intervals (60 s) both front pods hold the
# egress of front, and a second pull gets a 304. The pulls change no traffic: users logs nothing during the wait.
log "pul_19: the front pods pull their deployment's egress (same etag, 304 on the second pull), a users pod sees none, no canary, B's access lines unchanged"
pul_users_before=$(users_log_count) || fail "pul_19: cannot count the access lines of the users pods"
pul_want='GET /missing;GET /users/{users_id};PUT /users/{users_id};'
pul_ok() {
  local p etags=""
  for p in $egr_pods; do
    sidecar_get egr-19 "$p" /egress > "$WORK/pul19-egress-$p.json"
    [[ $(jq -r --arg a "$egr_authority" '.authorities[$a].endpoints // [] | map("\(.method) \(.template);") | sort | join("")' "$WORK/pul19-egress-$p.json" 2>/dev/null) == "$pul_want" ]] || return 1
    etags+="$(jq -r '.etag // ""' "$WORK/pul19-egress-$p.json")"$'\n'
  done
  [[ $(sort -u <<<"${etags%$'\n'}" | wc -l) == 1 && -n $(head -1 <<<"$etags") ]]
}
pul_done=0
end=$((SECONDS + 2 * 30 + 2 * 60))
while (( SECONDS < end )); do
  if pul_ok; then pul_done=1; break; fi
  sleep 3
done
(( pul_done )) || fail "pul_19: the front pods do not both list $egr_authority with $pul_want and one etag within 2 push and 2 pull intervals: $(head -c 2000 "$WORK"/pul19-egress-*.json 2>/dev/null)"
for p in $egr_pods; do
  jq -e --arg a "$egr_authority" '(.authorities | keys) == [$a] and (.pulled | type == "string")' "$WORK/pul19-egress-$p.json" >/dev/null \
    || fail "pul_19: /egress of $p does not hold exactly the authority $egr_authority and a pull time: $(head -c 1000 "$WORK/pul19-egress-$p.json")"
done
# The second pull: an empty set has an etag too, so the 304s of before the data existed do not count. The counts of
# not_modified pulls (each pod's metric, the controller's log lines) are taken now, with the data held, and must rise
# within 2 more pull intervals.
pul_nm_pod() {
  local n
  n=$(sidecar_get egr-19 "$1" /metrics \
    | awk 'index($0, "mimic_egress_pulls_total{result=\"not_modified\"} ") == 1 { printf "%d\n", $2 }' | tail -1)
  echo "${n:-0}"
}
pul_nm_log() {
  kubectl -n "$NS" logs -l app.kubernetes.io/name=mimic-controller --all-containers --tail=-1 > "$WORK/pul19-controller.log" || return 1
  jq -R -s '[split("\n")[] | fromjson? | select(.target == "sidecar_api" and .endpoint == "/v1/workloads/{deployment}/egress"
    and .namespace == "egr-19" and .deployment == "front" and .result == "not_modified")] | length' "$WORK/pul19-controller.log"
}
declare -A pul_base
for p in $egr_pods; do pul_base[$p]=$(pul_nm_pod "$p"); done
pul_log_base=$(pul_nm_log) || fail "pul_19: cannot read the controller logs"
pul_nm() {
  local p n
  for p in $egr_pods; do
    n=$(pul_nm_pod "$p")
    (( n > pul_base[$p] )) || return 1
  done
  n=$(pul_nm_log) || return 1
  (( n > pul_log_base ))
}
pul_got=0
end=$((SECONDS + 2 * 60))
while (( SECONDS < end )); do
  if pul_nm; then pul_got=1; break; fi
  sleep 3
done
(( pul_got )) || fail "pul_19: after the data was held, a front pod or the controller log did not show a new not_modified egress pull within 2 pull intervals"
# The controller's logs: sidecar_api lines for the egress route of front with ok and not_modified.
kubectl -n "$NS" logs -l app.kubernetes.io/name=mimic-controller --all-containers --tail=-1 > "$WORK/pul19-controller.log" || fail "pul_19: cannot read the controller logs"
for r in ok not_modified; do
  jq -R -e --arg r "$r" 'fromjson? | select(.target == "sidecar_api" and .endpoint == "/v1/workloads/{deployment}/egress"
    and .namespace == "egr-19" and .deployment == "front" and .result == $r)' "$WORK/pul19-controller.log" >/dev/null \
    || fail "pul_19: no sidecar_api line for the egress route of front with result $r in the controller logs"
done
# Isolation: a users pod (another deployment) holds no authority.
for p in $(kubectl -n disc-22 get pods -l app=users -o jsonpath='{.items[*].metadata.name}'); do
  sidecar_get disc-22 "$p" /egress > "$WORK/pul19-users-$p.json"
  jq -e '.authorities == {}' "$WORK/pul19-users-$p.json" >/dev/null \
    || fail "pul_19: /egress of the users pod $p holds an authority: $(head -c 1000 "$WORK/pul19-users-$p.json")"
done
# No canary in /egress, the controller logs or the sidecar logs of front.
: > "$WORK/pul19-sidecar.log"
for p in $egr_pods; do
  kubectl -n egr-19 logs "$p" --all-containers --tail=-1 >> "$WORK/pul19-sidecar.log" || fail "pul_19: cannot read the logs of $p"
done
for c in "${egr_canaries[@]}"; do
  for f in "$WORK"/pul19-egress-*.json "$WORK"/pul19-users-*.json "$WORK/pul19-controller.log" "$WORK/pul19-sidecar.log"; do
    if found -F "$c" "$f"; then fail "pul_19: a canary appears in ${f##*/}"; fi
  done
done
# F9 changes no traffic: B's access lines are those of before the pulls.
pul_users_after=$(users_log_count) || fail "pul_19: cannot count the access lines of the users pods after the pulls"
[[ $pul_users_before == "$pul_users_after" ]] || fail "pul_19: the users pods logged $((pul_users_after - pul_users_before)) request(s) during the pulls"

# --- SHR-20: the browser shows front's outbound endpoints and document -----------------------------------------------
# The Chromium of UI-12 is still running. The client traffic is over: users must log nothing during the run.
log "shr_20: the browser loads front's deployment page and its outbound document, with the Authority column and its link"
sleep 3
users_before=$(users_log_count) || fail "shr_20: cannot count the access lines of the users pods"
browse front /ui/workloads/egr-19/front --wait "$egr_authority" --wait '/users/{users_id}' --wait '/missing'
# Authority column: the header cell, and the cells of the outbound rows.
jq -e '[.rows[] | select(.[2] == "Authority")] | length == 1' "$WORK/browse-front.json" >/dev/null \
  || fail "shr_20: the endpoint table has no Authority column"
for row in 'GET|/users/{users_id}' 'PUT|/users/{users_id}' 'GET|/missing'; do
  jq -e --arg a "$egr_authority" --arg m "${row%%|*}" --arg t "${row#*|}" \
    '[.rows[] | select(.[0] == "outbound" and .[2] == $a and .[3] == $m and .[4] == $t)] | length == 1' "$WORK/browse-front.json" >/dev/null \
    || fail "shr_20: no outbound row $row with the authority $egr_authority in the deployment page: $(jq -c '[.rows[] | select(.[0] == "outbound")]' "$WORK/browse-front.json")"
done
# The inbound rows have no authority.
jq -e '[.rows[] | select(.[0] == "inbound" and .[2] != "")] | length == 0' "$WORK/browse-front.json" >/dev/null \
  || fail "shr_20: an inbound row has an authority"
# The link of the Authority cell leads to the outbound document (the document id, encoded or not).
jq -e --arg a "$egr_authority" '[.links[] | select(.text == $a and (.href | test("^/ui/workloads/egr-19/front/openapi/outbound-users\\.disc-22\\.svc(%3A|:)8080$")))] | length >= 3' \
  "$WORK/browse-front.json" >/dev/null \
  || fail "shr_20: the Authority cells do not link to the outbound document: $(jq -c '.links' "$WORK/browse-front.json")"
jq -e '[.links[] | select(.text | startswith("outbound-users.disc-22.svc"))] | length == 1' "$WORK/browse-front.json" >/dev/null \
  || fail "shr_20: the deployment page does not list the outbound document"
# Following the link: the document page of the document that the link names.
egr_href=$(jq -r --arg a "$egr_authority" '[.links[] | select(.text == $a)][0].href' "$WORK/browse-front.json")
browse outbound "$egr_href" --wait '/users/{users_id}' --expand
jq -e '(.methods | map(ascii_upcase) | unique) == ["GET", "PUT"]' "$WORK/browse-outbound.json" >/dev/null \
  || fail "shr_20: the outbound document page lists the operations $(jq -c .methods "$WORK/browse-outbound.json")"
jq -e '.text | test("GET\\s+/users/\\{users_id\\}") and test("PUT\\s+/users/\\{users_id\\}")' "$WORK/browse-outbound.json" >/dev/null \
  || fail "shr_20: the outbound document page does not show GET and PUT /users/{users_id}"
for page in front outbound; do
  for c in "${egr_canaries[@]}"; do
    if found -F "$c" "$WORK/browse-$page.json"; then fail "shr_20: a canary appears in the page $page"; fi
  done
done
users_after=$(users_log_count) || fail "shr_20: cannot count the access lines of the users pods after the browser run"
[[ $users_before == "$users_after" ]] || fail "shr_20: the users pods logged $((users_after - users_before)) request(s) during the browser run"
docker rm -f "$CHROME" >/dev/null 2>&1 || true

# --- Mock mode (F10, spec §8, §12.11 MCK-19; MCK-16 on a live connection) --------------------------------------------
# front (injected, 2 pods) in mck-19 calls users (disc-22) and an HTTPS provider (mck-19-provider, not injected) in
# discovery until the egress is pulled. spec.mode mock then answers its calls, a pod created by a scale-up starts in
# mock, and spec.mode discovery brings the calls back. A keep-alive connection of front's app to users is open across
# both switches (MCK-16 in the cluster). Each switch waits up to 2 pull intervals (60 s by default) and a status pass.
log "mck_19: front in discovery calls users and an HTTPS provider; its egress is pulled"
ns_fresh mck-19 mimic.platform.io/inject=enabled pod-security.kubernetes.io/enforce=privileged
ns_fresh mck-19-provider
issue mck-provider e2e-ca provider.mck-19-provider.svc
kubectl -n mck-19-provider create secret tls provider-cert --cert="$WORK/mck-provider.crt" --key="$WORK/mck-provider.key" >/dev/null
kubectl -n mck-19-provider create configmap provider-code --from-file=provider.py=scripts/e2e/apps/provider.py >/dev/null
kubectl apply -f - >/dev/null <<YAML
apiVersion: apps/v1
kind: Deployment
metadata: {name: provider, namespace: mck-19-provider}
spec:
  replicas: 1
  selector: {matchLabels: {app: provider}}
  template:
    metadata: {labels: {app: provider}}
    spec:
      containers:
        - name: provider
          image: $PY
          command: [python3, /code/provider.py]
          ports: [{containerPort: 8443}]
          volumeMounts:
            - {name: code, mountPath: /code, readOnly: true}
            - {name: certs, mountPath: /certs, readOnly: true}
      volumes:
        - {name: code, configMap: {name: provider-code}}
        - {name: certs, secret: {secretName: provider-cert}}
---
apiVersion: v1
kind: Service
metadata: {name: provider, namespace: mck-19-provider}
spec:
  selector: {app: provider}
  ports: [{name: https, port: 443, targetPort: 8443}]
YAML
kubectl -n mck-19-provider rollout status deploy/provider --timeout=2m >/dev/null || fail "mck_19: the provider did not roll out"

# front: the python image derived with the e2e CA (tls_25), so that its HTTPS calls to the provider trust the
# destination through mimic exactly as the TLS clients do. A MimicWorkload with no mode: discovery.
kubectl -n mck-19 create configmap apps-code --from-file=service.py=scripts/e2e/apps/service.py >/dev/null
kubectl apply -f - >/dev/null <<YAML
apiVersion: mimic.platform.io/v1alpha1
kind: MimicWorkload
metadata: {name: front, namespace: mck-19}
spec: {deployment: front}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: front, namespace: mck-19}
spec:
  replicas: 2
  selector: {matchLabels: {app: front}}
  template:
    metadata: {labels: {app: front}}
    spec:
      terminationGracePeriodSeconds: 5
      containers:
        - name: app
          image: mimic-e2e-python:$TAG
          imagePullPolicy: Never
          command: [python3, /code/service.py]
          env:
            - {name: APP_NAME, value: front}
            - {name: UPSTREAM, value: "http://users.disc-22.svc:8080"}
            - {name: PROVIDER, value: "https://provider.mck-19-provider.svc"}
          ports: [{name: api, containerPort: 8080}]
          readinessProbe: {tcpSocket: {port: api}, periodSeconds: 1}
          volumeMounts: [{name: code, mountPath: /code, readOnly: true}]
      volumes:
        - {name: code, configMap: {name: apps-code}}
---
apiVersion: v1
kind: Service
metadata: {name: front, namespace: mck-19}
spec:
  selector: {app: front}
  ports: [{name: api, port: 8080, targetPort: api}]
YAML
kubectl -n mck-19 rollout status deploy/front --timeout=3m >/dev/null \
  || { kubectl -n mck-19 describe pods -l app=front >&2 || true; fail "mck_19: deployment front did not roll out"; }
has_mimic mck-19 app=front || fail "mck_19: front was not injected"
mck_pods=$(kubectl -n mck-19 get pods -l app=front -o jsonpath='{.items[*].metadata.name}')
[[ $(wc -w <<<"$mck_pods") == 2 ]] || fail "mck_19: front has not 2 pods: '$mck_pods'"

# mck_app_count <namespace> <label> <container> <jq boolean>: the access lines of the pods' container for which the
# boolean holds, summed over the pods. A failed read is fatal (no pipe hides it).
mck_app_count() {
  local pods p log n total=0
  pods=$(kubectl -n "$1" get pods -l "$2" -o jsonpath='{.items[*].metadata.name}') || return 1
  [[ -n $pods ]] || return 1
  for p in $pods; do
    log=$(kubectl -n "$1" logs "$p" -c "$3") || return 1
    n=$(jq -R -s "split(\"\n\") | map(fromjson? | select(. != null and (.method? != null) and ($4))) | length" <<< "$log") || return 1
    total=$((total + n))
  done
  echo "$total"
}
# mck_users <boolean> / mck_provider <boolean>: the access lines of users (disc-22) and of the provider.
mck_users() { mck_app_count disc-22 app=users app "$1"; }
mck_provider() { mck_app_count mck-19-provider app=provider provider "$1"; }
# mck_sidecar <boolean>: the outbound access lines of the front sidecars for which the boolean holds (summed).
mck_sidecar() {
  local p log n total=0
  for p in $mck_pods_now; do
    log=$(kubectl -n mck-19 logs "$p" -c mimic-sidecar) || return 1
    n=$(jq -R -s "split(\"\n\") | map(fromjson? | select(. != null and .target == \"access\" and .direction == \"outbound\" and ($1))) | length" <<< "$log") || return 1
    total=$((total + n))
  done
  echo "$total"
}

# The calls of front, from the client pod: GETs of users and of the provider, each on a new connection (so that the
# Service spreads them over both pods), with a canary in the Authorization header. In mock the answers are the
# recorded ones (any id matches the endpoint); with $3 = 1 one call goes to a path no endpoint has (a miss).
mck_canary="mck19-$(openssl rand -hex 16)"
mck_batch() {
  kubectl -n disc-22-client exec -i client -- python3 - "$mck_canary" "$1" "$2" <<'PY'
import json, sys, urllib.error, urllib.request
canary, first, unrecorded = sys.argv[1], int(sys.argv[2]), sys.argv[3] == "1"
base = "http://front.mck-19.svc:8080/front"
auth = {"Authorization": f"Bearer {canary}", "X-Tenant-Key": canary}
def get(path):
    try:
        with urllib.request.urlopen(urllib.request.Request(base + path, headers=auth), timeout=20) as r:
            r.read()
            return r.status
    except urllib.error.HTTPError as e:
        e.read()
        return e.code
users = [get(f"/users/{first + n}") for n in range(12)]
provider = [get(f"/provider/rates/{n}") for n in range(4)]
unrec = get(f"/users/{first}/unrecorded/deep") if unrecorded else None
print(json.dumps({"users": users, "provider": provider, "unrecorded": unrec}))
PY
}
# mck_batch_ok <json> <expected status>: every call of the batch got that status (the 599 of the unrecorded call aside).
mck_batch_ok() {
  jq -e --argjson s "$2" '([.users[], .provider[]] | all(. == $s)) and (.unrecorded == null or .unrecorded == 599)' <<< "$1" >/dev/null
}

# The egress of a front pod (its sidecar's /egress, loopback through a port-forward) holds the users and the provider.
mck_snapshot() {
  local out
  out=$(sidecar_get mck-19 "$1" /egress) || return 1
  jq -e '(.authorities | keys | any(startswith("users.disc-22.svc:8080"))) and (.authorities | keys | any(startswith("provider.mck-19-provider.svc")))' <<< "$out" >/dev/null
}

mck_pods_now=$mck_pods
# The predicate of a mocked exchange (the access line's `mock` is exact, endpoint or fallback for an answer).
MOCKED='.mock == "exact" or .mock == "endpoint" or .mock == "fallback"'
before_users=$(mck_users 'true') || fail "mck_19: cannot count the access lines of the users pods"
before_provider=$(mck_provider 'true') || fail "mck_19: cannot count the access lines of the provider pod"
before_mock=$(mck_sidecar '.mock != null') || fail "mck_19: cannot read the sidecar access lines of front"
discovery_json=$(mck_batch 310000 0) || fail "mck_19: the calls of front in discovery failed"
mck_batch_ok "$discovery_json" 200 || fail "mck_19: front did not get 200 for every call in discovery: $discovery_json"
[[ $(mck_users 'true') == $((before_users + 12)) ]] || fail "mck_19: users did not receive the 12 calls of front in discovery"
[[ $(mck_provider 'true') == $((before_provider + 4)) ]] || fail "mck_19: the provider did not receive the 4 calls of front in discovery"
[[ $(mck_sidecar '.mock != null') == "$before_mock" ]] || fail "mck_19: a discovery call was counted as mocked"

snap=0
end=$((SECONDS + 240))
while (( SECONDS < end )); do
  all=1
  for p in $mck_pods; do
    if ! mck_snapshot "$p"; then all=0; break; fi
  done
  if (( all )); then snap=1; break; fi
  sleep 5
done
(( snap )) || fail "mck_19: the egress of front does not hold users and the provider within 4 minutes"

# The live connection: an app-level keep-alive connection of front's app to users, opened in a front pod and driven
# by a process that waits for files in the container (touch in /tmp/mck-live). Its answers show x-mimic-match.
live_pod=${mck_pods%% *}
mck_live_py=$(cat <<'PY'
import http.client, json, os, time
D = "/tmp/mck-live"
os.makedirs(D, exist_ok=True)
def emit(**kw):
    print(json.dumps(kw), flush=True)
def wait(name):
    end = time.time() + 400
    while not os.path.exists(f"{D}/{name}"):
        if time.time() > end:
            raise SystemExit(f"timeout waiting for {name}")
        time.sleep(0.2)
def get(conn, path):
    conn.request("GET", path, headers={"Authorization": "Bearer live"})
    r = conn.getresponse()
    r.read()
    return r.status, r.getheader("x-mimic-match")
def connect():
    conn = http.client.HTTPConnection("users.disc-22.svc", 8080, timeout=30)
    conn.connect()
    return conn
def port_of(conn):
    return conn.sock.getsockname()[1] if conn.sock is not None else None
conn = connect()
emit(step="ready")
status, match = get(conn, "/users/320001")
emit(step="discovery", status=status, match=match, port=port_of(conn))
wait("to-mock")
reconnected = False
try:
    status, match = get(conn, "/users/320002")
except (http.client.RemoteDisconnected, ConnectionResetError, BrokenPipeError):
    # The sidecar closes an idle keep-alive connection of the mocked path after 30 s (spec §8): a new connection, reported.
    conn.close()
    conn = connect()
    reconnected = True
    status, match = get(conn, "/users/320002")
emit(step="mock", status=status, match=match, port=port_of(conn), reconnected=reconnected)
wait("to-discovery")
try:
    status, match = get(conn, "/users/320003")
    emit(step="old-connection", status=status, match=match, port=port_of(conn))
except (http.client.HTTPException, OSError):
    emit(step="old-connection", closed=True)
conn.close()
conn = connect()
status, match = get(conn, "/users/320004")
emit(step="new-connection", status=status, match=match, port=port_of(conn))
conn.close()
emit(step="done")
PY
)
kubectl -n mck-19 exec "$live_pod" -c app -- python3 -u -c "$mck_live_py" > "$WORK/mck19-live.out" 2>&1 &
live_pid=$!
PIDS+=("$live_pid")
# live_step <step>: the JSON line of the live process with that step, or nothing.
live_step() { jq -R -s --arg s "$1" 'split("\n") | map(fromjson? | select(. != null and .step == $s)) | .[0] // empty' "$WORK/mck19-live.out"; }
# live_wait <step> <seconds>: polls until the live process printed the step.
live_wait() {
  local end=$((SECONDS + $2))
  while (( SECONDS < end )); do
    [[ -z $(live_step "$1") ]] || return 0
    sleep 1
  done
  return 1
}
live_wait ready 60 || fail "mck_19: the live connection process did not start"
live_step discovery | jq -e '.status == 200 and .match == null and (.port | type == "number")' >/dev/null || fail "mck_19: the live connection in discovery was not forwarded: $(live_step discovery)"
live_p1=$(live_step discovery | jq -r '.port')

# mode_wait <mode> <pods> <otherPods> <seconds>: the status of front converges (status.mode, MCK-07, MCK-09).
mode_wait() {
  local end=$((SECONDS + $4)) s
  while (( SECONDS < end )); do
    s=$(kubectl -n mck-19 get mimicworkload front -o jsonpath='{.status.mode}') || return 1
    if jq -e --arg m "$1" --argjson p "$2" --argjson o "$3" '.mode == $m and .pods == $p and .otherPods == $o' <<< "$s" >/dev/null 2>&1; then
      return 0
    fi
    sleep 5
  done
  return 1
}

log "mck_19: spec.mode mock makes status.mode converge to mock with 2 pods, and front's calls are answered by mock"
mock_before_switch=$(mck_sidecar "$MOCKED") || fail "mck_19: cannot read the sidecar access lines of front"
kubectl -n mck-19 patch mimicworkload front --type merge -p '{"spec":{"mode":"mock"}}' >/dev/null
mode_wait mock 2 0 150 || fail "mck_19: status.mode did not converge to mock with 2 pods within 2 pull intervals: $(kubectl -n mck-19 get mimicworkload front -o jsonpath='{.status.mode}')"
# The live connection: its next request, on the same connection, is answered by mock (MCK-16 on the cluster).
kubectl -n mck-19 exec "$live_pod" -c app -- touch /tmp/mck-live/to-mock
live_wait mock 60 || fail "mck_19: the live connection did not answer after the switch to mock"
live_step mock | jq -e --argjson p1 "$live_p1" '.status == 200 and (.match == "exact" or .match == "endpoint" or .match == "fallback")
  and (.port == $p1 or .reconnected == true)' >/dev/null \
  || fail "mck_19: the live connection in mock was not answered by mock on the discovery connection: $(live_step mock)"
# The sidecar closes a mocked connection idle for 30 s: then the mock step used a new connection, and it is said so.
if [[ $(live_step mock | jq -r '.reconnected') == true ]]; then log "mck_19: the live connection was closed while idle in mock (30 s); the mock step used a new one"; fi

users_before_mock=$(mck_users 'true') || fail "mck_19: cannot count the access lines of the users pods"
provider_before_mock=$(mck_provider 'true') || fail "mck_19: cannot count the access lines of the provider pod"
mock_json=$(mck_batch 330000 0) || fail "mck_19: the calls of front in mock failed"
mck_batch_ok "$mock_json" 200 || fail "mck_19: front did not get 200 for every call in mock: $mock_json"
[[ $(mck_users 'true') == "$users_before_mock" ]] || fail "mck_19: users logged $(( $(mck_users 'true') - users_before_mock )) request(s) in mock"
[[ $(mck_provider 'true') == "$provider_before_mock" ]] || fail "mck_19: the provider logged a request in mock"
mocked=$(mck_sidecar "$MOCKED") || fail "mck_19: cannot read the sidecar access lines of front"
(( mocked - mock_before_switch >= 17 )) || fail "mck_19: the sidecars logged $((mocked - mock_before_switch)) mocked exchanges after the switch, expected 16 (the batch) and the live call"
# A call to a path no endpoint has gets 599 from mock, and no request leaves for users.
mock_miss_json=$(mck_batch 330100 1) || fail "mck_19: the call to an unrecorded path failed"
[[ $(jq -r '.unrecorded' <<< "$mock_miss_json") == 599 ]] || fail "mck_19: a call to an unrecorded path did not get 599: $mock_miss_json"
misses=$(mck_sidecar '.mock == "miss" and .mock_miss == "endpoint"') || fail "mck_19: cannot read the sidecar access lines of front"
(( misses >= 1 )) || fail "mck_19: no endpoint miss in the sidecar access lines of front"
[[ $(mck_users 'true') == "$users_before_mock" ]] || fail "mck_19: users logged a request for the unrecorded path"

log "mck_19: a pod created by a scale-up starts in mock and its first call is answered by mock"
kubectl -n mck-19 scale deploy/front --replicas=3 >/dev/null
kubectl -n mck-19 rollout status deploy/front --timeout=3m >/dev/null || fail "mck_19: front did not scale to 3"
new_pod=""
for p in $(kubectl -n mck-19 get pods -l app=front -o jsonpath='{.items[*].metadata.name}'); do
  case " $mck_pods " in *" $p "*) ;; *) new_pod=$p ;; esac
done
[[ -n $new_pod ]] || fail "mck_19: no new front pod after the scale-up"
[[ $(jp mck-19 "$new_pod" '{.spec.initContainers[?(@.name=="mimic-sidecar")].env[?(@.name=="MIMIC_MODE")].value}') == mock ]] \
  || fail "mck_19: the scale-up pod $new_pod has no MIMIC_MODE=mock"
mck_pods_now=$(kubectl -n mck-19 get pods -l app=front -o jsonpath='{.items[*].metadata.name}')
mck_pods=$mck_pods_now
# Its sidecar started in mock from the environment, not from a pull.
kubectl -n mck-19 logs "$new_pod" -c mimic-sidecar \
  | jq -R -e -s 'split("\n") | map(fromjson? | select(. != null and .message == "mode changed" and .mode == "mock" and .source == "env")) | length > 0' >/dev/null \
  || fail "mck_19: the sidecar of $new_pod did not log mode changed to mock from the environment"
end=$((SECONDS + 120))
new_snap=0
while (( SECONDS < end )); do
  if mck_snapshot "$new_pod"; then new_snap=1; break; fi
  sleep 3
done
(( new_snap )) || fail "mck_19: the scale-up pod did not pull the egress with users and the provider"
mode_wait mock 3 0 150 || fail "mck_19: status.mode did not show 3 pods in mock: $(kubectl -n mck-19 get mimicworkload front -o jsonpath='{.status.mode}')"
new_users_before=$(mck_users 'true') || fail "mck_19: cannot count the access lines of the users pods"
new_call=$(kubectl -n mck-19 exec "$new_pod" -c app -- python3 -c '
import json, urllib.request, urllib.error
try:
    with urllib.request.urlopen("http://users.disc-22.svc:8080/users/340001", timeout=20) as r:
        body = r.read(); print(json.dumps({"status": r.status, "match": r.headers.get("x-mimic-match"), "legacy": b"<redacted>" in body, "cookie": r.headers.get("set-cookie")}))
except urllib.error.HTTPError as e:
    print(json.dumps({"status": e.code, "match": None}))
' 2>&1) || fail "mck_19: the first call of $new_pod failed"
jq -e '.status == 200 and (.match == "exact" or .match == "endpoint" or .match == "fallback")' <<< "$new_call" >/dev/null \
  || fail "mck_19: the first call of $new_pod was not answered by mock: $new_call"
jq -e '.legacy == false and .cookie == "sid=mimic-cookie"' <<< "$new_call" >/dev/null \
  || fail "mck_19: a mock answer carries <redacted>, or its set-cookie is not sid=mimic-cookie (PLH-07, PLH-10)"
[[ $(mck_users 'true') == "$new_users_before" ]] || fail "mck_19: users logged the first call of $new_pod"

log "mck_19: spec.mode discovery brings the calls back to users and the provider, and the live connection is forwarded again"
kubectl -n mck-19 patch mimicworkload front --type merge -p '{"spec":{"mode":"discovery"}}' >/dev/null
mode_wait discovery 3 0 150 || fail "mck_19: status.mode did not converge to discovery with otherPods 0 within 2 pull intervals: $(kubectl -n mck-19 get mimicworkload front -o jsonpath='{.status.mode}')"
kubectl -n mck-19 exec "$live_pod" -c app -- touch /tmp/mck-live/to-discovery
live_wait done 150 || fail "mck_19: the live connection process did not finish after the switch to discovery"
wait "$live_pid" || fail "mck_19: the live connection process failed"
[[ $(live_step old-connection | jq -r '(.closed // false) or (.match == null)') == true ]] \
  || fail "mck_19: the mocked connection answered after the switch to discovery: $(live_step old-connection)"
live_step new-connection | jq -e --argjson p1 "$live_p1" '.status == 200 and .match == null and .port != $p1' >/dev/null \
  || fail "mck_19: the new connection after the switch was not forwarded on a new TCP connection: $(live_step new-connection)"
[[ $(mck_app_count disc-22 app=users app '.path == "/users/320001"') == 1 ]] || fail "mck_19: users did not get the forwarded live call before the switch"
[[ $(mck_app_count disc-22 app=users app '.path == "/users/320002"') == 0 ]] || fail "mck_19: users got the live call that mock answered"
[[ $(mck_app_count disc-22 app=users app '.path == "/users/320004"') == 1 ]] || fail "mck_19: users did not get the live call of the new connection"

users_before_disc=$(mck_users 'true') || fail "mck_19: cannot count the access lines of the users pods"
provider_before_disc=$(mck_provider 'true') || fail "mck_19: cannot count the access lines of the provider pod"
mock_before_disc=$(mck_sidecar '.mock != null') || fail "mck_19: cannot read the sidecar access lines of front"
disc_json=$(mck_batch 350000 0) || fail "mck_19: the calls of front after the switch failed"
mck_batch_ok "$disc_json" 200 || fail "mck_19: front did not get 200 for every call after the switch: $disc_json"
[[ $(mck_users 'true') == $((users_before_disc + 12)) ]] || fail "mck_19: users did not receive the 12 calls of front after the switch"
[[ $(mck_provider 'true') == $((provider_before_disc + 4)) ]] || fail "mck_19: the provider did not receive the 4 calls of front after the switch"
[[ $(mck_sidecar '.mock != null') == "$mock_before_disc" ]] || fail "mck_19: a call after the switch to discovery was mocked"

# No canary that front sends is in a log line of the cluster: front, users, the provider, the client and the controller.
for ns in mck-19 mck-19-provider disc-22 disc-22-client; do
  for p in $(kubectl -n "$ns" get pods -o jsonpath='{.items[*].metadata.name}'); do
    kubectl -n "$ns" logs "$p" --all-containers --tail=-1 > "$WORK/mck19-$ns-$p.log" || fail "mck_19: cannot read the logs of $p"
    if found -F "$mck_canary" "$WORK/mck19-$ns-$p.log"; then fail "mck_19: a canary of front appears in the logs of $p"; fi
  done
done
kubectl -n "$NS" logs -l app.kubernetes.io/name=mimic-controller --all-containers --tail=-1 > "$WORK/mck19-controller.log" \
  || fail "mck_19: cannot read the controller logs"
if found -F "$mck_canary" "$WORK/mck19-controller.log"; then fail "mck_19: a canary of front appears in the controller logs"; fi

# LDT-16 and LDT-19 (E), spec §12.12 (F11): a load test of front on this setup, in mock, and the deployment page with its
# runs. It needs mck-19, disc-22 and mck-19-provider, so it runs before the teardown below removes them.
CLUSTER=$CLUSTER TAG=$TAG IMAGE=$IMAGE INIT_IMAGE=$INIT_IMAGE scripts/e2e-load.sh --suite \
  || fail "ldt_16: scripts/e2e-load.sh --suite failed"

kubectl delete namespace mck-19 mck-19-provider --wait=false >/dev/null

kubectl delete namespace disc-22 disc-22-client egr-19 --wait=false >/dev/null

# --- MON-02 (spec §12.13, §10.7, #378): the Valkey exporter, and a sidecar's /metrics from another pod ----------------
# The upgrade enables valkey.metrics: the Valkey pod restarts once. The scraper is a pod of the release namespace (the
# metrics namespace of the exporter's NetworkPolicy), restricted-compliant, that reads both endpoints by pod IP.
log "mon_02: the Valkey exporter is enabled by an upgrade, its container is Ready as UID 65532"
install --set valkey.metrics.enabled=true || fail "mon_02: helm upgrade with valkey.metrics.enabled"
kubectl -n "$NS" rollout status statefulset/$RELEASE-valkey --timeout=3m
valkey_pod=$RELEASE-valkey-0
[[ $(kubectl -n "$NS" get pod "$valkey_pod" -o jsonpath='{.status.containerStatuses[?(@.name=="exporter")].ready}') == true ]] \
  || fail "mon_02: the exporter container is not Ready"
[[ $(kubectl -n "$NS" get pod "$valkey_pod" -o jsonpath='{.spec.containers[?(@.name=="exporter")].securityContext.runAsUser}') == 65532 ]] \
  || fail "mon_02: the exporter container does not run as UID 65532"
# Valkey loads its users from the ACL file only: the default user must still need a password (NOAUTH without one),
# and the controller, which authenticates with it, must roll out Ready after the restart (#378). No password is printed.
kubectl -n "$NS" rollout status deploy/$RELEASE-controller --timeout=3m \
  || fail "mon_02: the controller Deployment does not roll out Ready after the Valkey restart"
noauth_out=$(kubectl -n "$NS" exec "$valkey_pod" -c valkey -- sh -c 'unset REDISCLI_AUTH; exec valkey-cli PING' 2>&1 || true)
grep -q NOAUTH <<<"$noauth_out" || fail "mon_02: an unauthenticated PING in Valkey is not refused with NOAUTH"
exporter_ip=$(kubectl -n "$NS" get pod "$valkey_pod" -o jsonpath='{.status.podIP}')
[[ -n $exporter_ip ]] || fail "mon_02: the Valkey pod has no IP"

# The scraper: the Python image, restricted, in the release namespace.
kubectl apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Pod
metadata: {name: mon02-scraper, namespace: $NS}
spec:
  automountServiceAccountToken: false
  securityContext: {runAsNonRoot: true, runAsUser: 65532, seccompProfile: {type: RuntimeDefault}}
  containers:
    - name: scraper
      image: $PY
      command: [sleep, "900"]
      securityContext:
        allowPrivilegeEscalation: false
        readOnlyRootFilesystem: true
        capabilities: {drop: [ALL]}
YAML
kubectl -n "$NS" wait --for=condition=Ready pod/mon02-scraper --timeout=2m >/dev/null || fail "mon_02: the scraper pod is not Ready"

# mon02_get <url>: the body of a GET from the scraper pod. Empty on failure.
mon02_get() {
  kubectl -n "$NS" exec mon02-scraper -- python3 -c \
    'import sys, urllib.request; sys.stdout.write(urllib.request.urlopen(sys.argv[1], timeout=5).read().decode())' "$1" 2>/dev/null || true
}
# mon02_missing <body> <names...>: prints each name with no "# TYPE" line in the Prometheus text <body> (MON-13). A counter
# is matched with or without its _total suffix, since encoders differ on whether the TYPE line keeps it.
mon02_missing() {
  local body=$1 n
  shift
  for n in "$@"; do
    grep -qE "^# TYPE ${n%_total}(_total)? " <<<"$body" || printf '%s\n' "$n"
  done
}
exporter_body=""
for _ in $(seq 30); do
  exporter_body=$(mon02_get "http://$exporter_ip:9121/metrics")
  grep -qx 'redis_up 1' <<<"$exporter_body" && break
  sleep 2
done
grep -qx 'redis_up 1' <<<"$exporter_body" || fail "mon_02: the exporter's /metrics from the release namespace does not answer redis_up 1"

# A canary key and value as the default user. Neither may reach the exporter's /metrics, and the exporter password
# neither (values are never printed).
canary_key="mon02-key-$RANDOM$RANDOM"
canary_value="mon02-$(head -c 32 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 24)"
exporter_pw=$(kubectl -n "$NS" get secret $RELEASE-valkey-exporter -o jsonpath='{.data.password}' | base64 -d)
[[ -n $exporter_pw && -n $canary_value ]] || fail "mon_02: no exporter password or canary value"
[[ $(kubectl -n "$NS" exec "$valkey_pod" -c valkey -- valkey-cli SET "$canary_key" "$canary_value") == OK ]] \
  || fail "mon_02: the canary key was not written as the default user"
exporter_body=$(mon02_get "http://$exporter_ip:9121/metrics")
grep -qx 'redis_up 1' <<<"$exporter_body" || fail "mon_02: the exporter does not see Valkey after the canary write"
if grep -qF -- "$canary_value" <<<"$exporter_body"; then fail "mon_02: the exporter's /metrics holds the canary value"; fi
if grep -qF -- "$exporter_pw" <<<"$exporter_body"; then fail "mon_02: the exporter's /metrics holds the exporter password"; fi
# MON-13: the exporter's last scrape has a TYPE line for every series of the exporter list of §9.
missing=$(mon02_missing "$exporter_body" redis_up redis_uptime_in_seconds redis_instance_info redis_memory_used_bytes \
  redis_memory_max_bytes redis_mem_fragmentation_ratio redis_evicted_keys_total redis_expired_keys_total \
  redis_commands_processed_total redis_commands_total redis_commands_duration_seconds_total redis_connected_clients \
  redis_rejected_connections_total redis_db_keys redis_aof_last_write_status | tr '\n' ' ')
[[ -z $missing ]] || fail "mon_02: the exporter's /metrics lacks the TYPE line of: $missing"

# The exporter user, with its password on stdin (not in argv): INFO answers, the canary key is refused.
exporter_cli() {
  printf '%s\n' "$exporter_pw" | kubectl -n "$NS" exec -i "$valkey_pod" -c valkey -- \
    sh -c 'read -r p; REDISCLI_AUTH="$p" valkey-cli --no-auth-warning --user mimic-exporter "$@"' sh "$@" 2>&1 || true
}
info_out=$(exporter_cli INFO server)
grep -q '^# Server' <<<"$info_out" || fail "mon_02: mimic-exporter cannot run INFO"
get_out=$(exporter_cli GET "$canary_key")
grep -q NOPERM <<<"$get_out" || fail "mon_02: mimic-exporter can GET a key"
set_out=$(exporter_cli SET "$canary_key" x)
grep -q NOPERM <<<"$set_out" || fail "mon_02: mimic-exporter can SET a key"
kubectl -n "$NS" exec "$valkey_pod" -c valkey -- valkey-cli DEL "$canary_key" >/dev/null || true

# The controller still stores and reads after the Valkey restart: it stays ready and its status passes succeed.
mon02_ns=mon-02
ns_fresh $mon02_ns mimic.platform.io/inject=enabled pod-security.kubernetes.io/enforce=privileged
admin_forward
[[ $(curl -s -o /dev/null -w '%{http_code}' localhost:18081/readyz) == 200 ]] || fail "mon_02: the controller is not ready after the Valkey restart"
kubectl apply -f - >/dev/null <<YAML
apiVersion: mimic.platform.io/v1alpha1
kind: MimicWorkload
metadata: {name: mon02, namespace: $mon02_ns}
spec: {deployment: mon02-app}
YAML
deploy_sleeper $mon02_ns mon02-app
kubectl -n $mon02_ns rollout status deploy/mon02-app --timeout=3m
# MON-13: the controller pods together have a TYPE line for each of their metrics of §9 and a mimic_limit series per
# name. Every pod is scraped, after the app's rollout: a labelled family has no TYPE line before its first series, and
# the one admission since the Valkey restart went to one replica only.
ctl_body=""
for p in $(kubectl -n "$NS" get pods -l app.kubernetes.io/name=mimic-controller -o name); do
  kubectl -n "$NS" port-forward "$p" 18091:8081 >/dev/null 2>&1 &
  pf=$!
  for _ in $(seq 50); do curl -fs --max-time 2 -o /dev/null localhost:18091/readyz && break; sleep 0.2; done
  body=$(curl -fs --max-time 10 localhost:18091/metrics || true)
  # A pod that does not answer is logged, not failed: the other replicas still count (#406 item 7).
  if [[ -z ${body//[[:space:]]/} ]]; then echo "WARN mon_02: controller pod $p does not answer its scrape"; fi
  ctl_body+=$body$'\n'
  kill "$pf" 2>/dev/null || true; wait "$pf" 2>/dev/null || true
done
[[ -n ${ctl_body//[[:space:]]/} ]] || fail "mon_02: the controller's /metrics does not answer"
missing=$(mon02_missing "$ctl_body" process_cpu_seconds_total process_resident_memory_bytes process_open_fds \
  process_max_fds process_start_time_seconds mimic_webhook_admissions_total \
  mimic_webhook_certificate_expiry_timestamp_seconds mimic_webhook_admission_duration_seconds \
  mimic_store_deployments mimic_limit | tr '\n' ' ')
[[ -z $missing ]] || fail "mon_02: the controller's /metrics lacks the TYPE line of: $missing"
missing=""
for n in webhook_max_connections sidecar_api_rate sidecar_api_max_report_bytes sidecar_api_body_budget_bytes \
  sidecar_api_max_egress_bytes sidecar_api_egress_budget_bytes store_max_deployments store_max_endpoints \
  store_max_push_endpoints store_max_literals store_max_authorities store_sample_budget_bytes load_max_runs \
  load_max_namespace_runs load_max_pending load_max_concurrency load_max_rate load_max_endpoints \
  load_pod_concurrency load_max_plan_bytes load_store_budget_bytes; do
  grep -qF "mimic_limit{name=\"$n\"}" <<<"$ctl_body" || missing+="$n "
done
[[ -z $missing ]] || fail "mon_02: the controller's /metrics has no mimic_limit series for: $missing"
# The namespace and its MimicWorkload are new, so any status was written after the Valkey restart: status.mode.pods
# comes from the pod's pull fields in Valkey. Either replica may apply it (the other then sees no change, CTL-18),
# so the check reads the object, not one replica's counter.
grown=0
for _ in $(seq 45); do
  [[ $(kubectl -n $mon02_ns get mimicworkload mon02 -o jsonpath='{.status.mode.pods}' 2>/dev/null) == 1 ]] && { grown=1; break; }
  sleep 2
done
(( grown == 1 )) || fail "mon_02: no status pass of the controller succeeded after the Valkey restart"

# The sidecar's /metrics answers from the scraper pod in the release namespace (no policy on 15100 by design).
sidecar_pod=$(kubectl -n $mon02_ns get pods -l app=mon02-app -o jsonpath='{.items[0].metadata.name}')
sidecar_ip=$(kubectl -n $mon02_ns get pod "$sidecar_pod" -o jsonpath='{.status.podIP}')
[[ -n $sidecar_ip ]] || fail "mon_02: the injected pod has no IP"
# The port by its name (MON-03): the sidecar (a native sidecar, so an init container) declares mimic-admin on 15100,
# the port a PodMonitor scrapes.
sidecar_port=$(kubectl -n $mon02_ns get pod "$sidecar_pod" -o jsonpath='{.spec.initContainers[?(@.name=="mimic-sidecar")].ports[?(@.name=="mimic-admin")].containerPort}')
[[ $sidecar_port == 15100 ]] || fail "mon_02: the sidecar container does not declare the port mimic-admin on 15100"
sidecar_body=$(mon02_get "http://$sidecar_ip:$sidecar_port/metrics")
grep -qF 'mimic_build_info{component="sidecar"' <<<"$sidecar_body" \
  || fail "mon_02: the sidecar's /metrics on 15100 does not answer from the release namespace"
# MON-13: the same scrape has a TYPE line for each sidecar metric of §9, process_open_fds included (TLS-16).
missing=$(mon02_missing "$sidecar_body" process_cpu_seconds_total process_resident_memory_bytes process_open_fds \
  process_max_fds process_start_time_seconds mimic_proxy_active_connections mimic_proxy_overhead_seconds \
  mimic_discovery_last_new_endpoint_timestamp_seconds mimic_mock_mode_switches_total mimic_mock_latency_factor \
  mimic_limit | tr '\n' ' ')
[[ -z $missing ]] || fail "mon_02: the sidecar's /metrics lacks the TYPE line of: $missing"

kubectl delete namespace $mon02_ns --wait=false >/dev/null
kubectl -n "$NS" delete pod mon02-scraper --wait=false >/dev/null

# --- MON-21 (spec §12.13, §10.7, #379): the bundled Prometheus and Grafana, enabled by an upgrade -------------------
# The upgrade enables monitoring.prometheus, monitoring.grafana and valkey.metrics (the exporter target). The checks run
# in pods of the release namespace through kubectl exec, so no host port is used. The Prometheus API is called from the
# pod the Prometheus NetworkPolicy admits (mon21-prom), and an unlabelled pod is refused (mon21-client). Grafana is
# queried with the admin password, which is read from its Secret into a variable and sent on stdin: it is never
# printed and never in argv.
# A successful login proves the admin password file is mounted; the provisioned datasource and the dashboard both prove
# the provisioning and the /tmp/dashboards mounts on the read-only root. Ready pods prove that the images start with no
# command override (the Deployment sets none).
log "mon_21: the bundled Prometheus and Grafana are enabled by an upgrade"
install --set valkey.metrics.enabled=true --set monitoring.prometheus.enabled=true --set monitoring.grafana.enabled=true \
  || fail "mon_21: helm upgrade with the bundled monitoring"
kubectl -n "$NS" rollout status deploy/$RELEASE-prometheus --timeout=3m || fail "mon_21: Prometheus is not Ready"
kubectl -n "$NS" rollout status deploy/$RELEASE-grafana --timeout=3m || fail "mon_21: Grafana is not Ready"
[[ $(kubectl -n "$NS" get deploy $RELEASE-grafana -o jsonpath='{.spec.template.spec.containers[?(@.name=="grafana")].command}') == "" ]] \
  || fail "mon_21: the grafana container overrides the image command"
[[ $(kubectl -n "$NS" get pod -l app.kubernetes.io/name=mimic-grafana -o jsonpath='{.items[0].status.containerStatuses[?(@.name=="grafana")].restartCount}') == 0 ]] \
  || fail "mon_21: the grafana container restarted"

grafana_pw=$(kubectl -n "$NS" get secret $RELEASE-grafana-admin -o jsonpath='{.data.admin-password}' | base64 -d)
[[ ${#grafana_pw} == 32 ]] || fail "mon_21: the admin password Secret does not hold 32 characters"

# The clients: the Python image, restricted, in the release namespace (MON-21). mon21-client has no labels, so the
# Prometheus NetworkPolicy refuses it; it queries Grafana (admitted from the release namespace) and proves the refusal.
# mon21-prom carries exactly the labels of the Prometheus policy's ingress selector, read from the live NetworkPolicy
# (never hard-coded): the policy admits it, and every Prometheus API call of this section goes through it.
# client_pod <name> <labels JSON>: create one client pod.
client_pod() {
  jq -n --arg name "$1" --arg ns "$NS" --arg image "$PY" --argjson labels "$2" '{
    apiVersion: "v1", kind: "Pod",
    metadata: {name: $name, namespace: $ns, labels: $labels},
    spec: {
      automountServiceAccountToken: false,
      securityContext: {runAsNonRoot: true, runAsUser: 65532, seccompProfile: {type: "RuntimeDefault"}},
      containers: [{
        name: "client", image: $image, command: ["sleep", "900"],
        securityContext: {allowPrivilegeEscalation: false, readOnlyRootFilesystem: true, capabilities: {drop: ["ALL"]}}
      }]
    }
  }' | kubectl apply -f - >/dev/null
}
prom_labels=$(kubectl -n "$NS" get networkpolicy $RELEASE-prometheus -o jsonpath='{.spec.ingress[0].from[0].podSelector.matchLabels}')
[[ -n $prom_labels && $prom_labels != null ]] || fail "mon_21: the Prometheus NetworkPolicy admits no labelled pod"
client_pod mon21-client '{}'
client_pod mon21-prom "$prom_labels"
kubectl -n "$NS" wait --for=condition=Ready pod/mon21-client --timeout=2m >/dev/null || fail "mon_21: the client pod is not Ready"
kubectl -n "$NS" wait --for=condition=Ready pod/mon21-prom --timeout=2m >/dev/null || fail "mon_21: the Prometheus client pod is not Ready"

# mon21_py: GET argv[1] and print its status on the first line, then the body. With a second argument the admin
# password is read from stdin and sent as Basic auth. Connection failures print status 0.
mon21_py=$(cat <<'PY'
import base64, sys, urllib.error, urllib.request
req = urllib.request.Request(sys.argv[1])
if len(sys.argv) > 2:
    pw = sys.stdin.read()
    req.add_header("Authorization", "Basic " + base64.b64encode(("admin:" + pw).encode()).decode())
try:
    r = urllib.request.urlopen(req, timeout=10)
    code, body = r.status, r.read()
except urllib.error.HTTPError as e:
    code, body = e.code, e.read()
except OSError:
    code, body = 0, b""
sys.stdout.write("%d\n%s" % (code, body.decode("utf-8", "replace")))
PY
)
# mon21_fetch <pod> <url> [auth]: the response of a GET from the client pod <pod> (status line, then body). Empty when
# the exec fails.
mon21_fetch() {
  local pod=$1 url=$2
  if [[ ${3:-} == auth ]]; then
    printf '%s' "$grafana_pw" | kubectl -n "$NS" exec -i "$pod" -- python3 -I -c "$mon21_py" "$url" auth 2>/dev/null || true
  else
    kubectl -n "$NS" exec "$pod" -- python3 -I -c "$mon21_py" "$url" 2>/dev/null || true
  fi
}
# mon21_get <url> [auth]: the same from mon21-client (the Grafana checks).
mon21_get() { mon21_fetch mon21-client "$@"; }
mon21_status() { head -n 1 <<<"$1"; }
mon21_body() { tail -n +2 <<<"$1"; }

# Grafana: health, no anonymous access, and the admin login (the password file is mounted).
health=$(mon21_get http://$RELEASE-grafana:3000/api/health)
[[ $(mon21_status "$health") == 200 ]] && grep -q '"database": "ok"' <<<"$(mon21_body "$health")" \
  || fail "mon_21: Grafana /api/health is not ok"
[[ $(mon21_status "$(mon21_get http://$RELEASE-grafana:3000/api/search)") == 401 ]] \
  || fail "mon_21: Grafana answers without credentials other than 401"
# The default login is refused: the admin password comes from the Secret, never Grafana's built-in "admin".
[[ $(mon21_status "$(grafana_pw=admin mon21_get http://$RELEASE-grafana:3000/api/org auth)") == 401 ]] \
  || fail "mon_21: Grafana accepts the default admin:admin login"
ds_health=""
for _ in $(seq 60); do
  ds_health=$(mon21_get http://$RELEASE-grafana:3000/api/datasources/uid/mimic-prometheus/health auth)
  [[ $(mon21_status "$ds_health") == 200 ]] && jq -e '.status == "OK"' <<<"$(mon21_body "$ds_health")" >/dev/null && break
  sleep 2
done
[[ $(mon21_status "$ds_health") == 200 ]] && jq -e '.status == "OK"' <<<"$(mon21_body "$ds_health")" >/dev/null \
  || fail "mon_21: the provisioned Prometheus datasource is not healthy through Grafana"

# The Prometheus targets, through the Grafana proxy of the datasource: the controller, a sidecar of an injected pod in
# its own namespace (with its namespace, pod and deployment labels), and the Valkey exporter, all up.
mon21_ns=mon-21
ns_fresh $mon21_ns mimic.platform.io/inject=enabled pod-security.kubernetes.io/enforce=privileged
kubectl apply -f - >/dev/null <<YAML
apiVersion: mimic.platform.io/v1alpha1
kind: MimicWorkload
metadata: {name: mon21, namespace: $mon21_ns}
spec: {deployment: mon21-app}
YAML
deploy_sleeper $mon21_ns mon21-app
kubectl -n $mon21_ns rollout status deploy/mon21-app --timeout=3m
# The target of the requests that MON-24 sends through mon21-app: a plain HTTP server in the same namespace.
kubectl apply -f - >/dev/null <<YAML
apiVersion: apps/v1
kind: Deployment
metadata: {name: mon21-echo, namespace: $mon21_ns}
spec:
  replicas: 1
  selector: {matchLabels: {app: mon21-echo}}
  template:
    metadata: {labels: {app: mon21-echo}}
    spec:
      containers:
        - name: echo
          image: $PY
          command: [sh, -c, "mkdir -p /www && touch /www/ok && exec python3 -m http.server 8080 -d /www"]
          ports: [{containerPort: 8080}]
---
apiVersion: v1
kind: Service
metadata: {name: mon21-echo, namespace: $mon21_ns}
spec:
  selector: {app: mon21-echo}
  ports: [{port: 8080}]
YAML
kubectl -n $mon21_ns rollout status deploy/mon21-echo --timeout=3m

# targets_up <targets JSON>: the three jobs have an up target, and the sidecar target carries the labels of its pod.
targets_up() {
  jq -e --arg ns "$mon21_ns" '
    [.data.activeTargets[] | select(.health == "up")] as $up
    | ($up | map(.labels.job)) as $jobs
    | ($jobs | index("mimic-controller")) and ($jobs | index("mimic-valkey")) and ($jobs | index("mimic-sidecar"))
      and any($up[]; .labels.job == "mimic-sidecar" and .labels.namespace == $ns and .labels.deployment == "mon21-app")
  ' >/dev/null 2>&1 <<<"$1"
}
ready=$(mon21_fetch mon21-prom http://$RELEASE-prometheus:9090/-/ready)
[[ $(mon21_status "$ready") == 200 ]] || fail "mon_21: Prometheus /-/ready does not answer 200 from the admitted pod"
# The negative check (MON-21): the unlabelled pod gets no HTTP answer from 9090, only a timeout or a connection failure
# (status 0). It runs after the admitted pod got 200, so the name resolves and the refusal is the policy's.
refused=$(mon21_fetch mon21-client http://$RELEASE-prometheus:9090/-/ready)
[[ $(mon21_status "$refused") == 0 ]] || fail "mon_21: the unlabelled pod got an HTTP answer from the Prometheus API on 9090"
# Within 120 s (MON-21): the deadline is on the clock, since each request takes time of its own.
targets=""
deadline=$((SECONDS + 120))
while (( SECONDS < deadline )); do
  targets=$(mon21_get http://$RELEASE-grafana:3000/api/datasources/proxy/uid/mimic-prometheus/api/v1/targets auth)
  if [[ $(mon21_status "$targets") == 200 ]] && targets_up "$(mon21_body "$targets")"; then break; fi
  sleep 2
done
[[ $(mon21_status "$targets") == 200 ]] && targets_up "$(mon21_body "$targets")" \
  || fail "mon_21: the controller, sidecar and Valkey exporter targets are not all up in Prometheus"

# The dashboards: a ConfigMap labelled grafana_dashboard "1" is listed by Grafana within 60 s, one labelled "0" is not.
kubectl apply -f - >/dev/null <<YAML
apiVersion: v1
kind: ConfigMap
metadata: {name: mon21-dashboard, namespace: $NS, labels: {grafana_dashboard: "1"}}
data:
  mon21.json: '{"uid": "mimic-e2e", "title": "mimic e2e", "schemaVersion": 39, "panels": []}'
---
apiVersion: v1
kind: ConfigMap
metadata: {name: mon21-dashboard-off, namespace: $NS, labels: {grafana_dashboard: "0"}}
data:
  mon21-off.json: '{"uid": "mimic-e2e-off", "title": "mimic off", "schemaVersion": 39, "panels": []}'
YAML
found_dash=0
deadline=$((SECONDS + 60))
while (( SECONDS < deadline )); do
  search=$(mon21_get "http://$RELEASE-grafana:3000/api/search?query=mimic" auth)
  if [[ $(mon21_status "$search") == 200 ]] && jq -e 'any(.[]; .uid == "mimic-e2e")' >/dev/null <<<"$(mon21_body "$search")"; then
    found_dash=1
    break
  fi
  sleep 2
done
(( found_dash == 1 )) || fail "mon_21: the dashboard of the labelled ConfigMap is not listed by Grafana"
jq -e 'any(.[]; .uid == "mimic-e2e-off") | not' >/dev/null <<<"$(mon21_body "$search")" \
  || fail "mon_21: the dashboard of the ConfigMap labelled 0 is listed by Grafana"

# MON-24 (#380): the chart's dashboards. Grafana lists exactly the five tagged mimic, with their titles, within 60 s.
dash_dir=deploy/helm/mimic/dashboards
want_dashboards=$(for f in "$dash_dir"/*.json; do jq -c '{uid, title}' "$f"; done | jq -s -c 'sort_by(.uid)')
found_dash=0
deadline=$((SECONDS + 60))
while (( SECONDS < deadline )); do
  search=$(mon21_get "http://$RELEASE-grafana:3000/api/search?tag=mimic" auth)
  if [[ $(mon21_status "$search") == 200 ]] \
    && [[ $(jq -c '[.[] | {uid, title}] | sort_by(.uid)' <<<"$(mon21_body "$search")" 2>/dev/null) == "$want_dashboards" ]]; then
    found_dash=1
    break
  fi
  sleep 2
done
(( found_dash == 1 )) || fail "mon_21: Grafana /api/search?tag=mimic does not list exactly the dashboards of the chart with their titles"

# The controller job of the variable query is the release's.
controller_match=$(jq -rn '"mimic_build_info{component=\"controller\"}" | @uri')
found_job=0
deadline=$((SECONDS + 120))
while (( SECONDS < deadline )); do
  jobs=$(mon21_fetch mon21-prom "http://$RELEASE-prometheus:9090/api/v1/label/job/values?match%5B%5D=$controller_match")
  if [[ $(mon21_status "$jobs") == 200 ]] && jq -e --arg j "$RELEASE-controller" '.data | index($j)' >/dev/null 2>&1 <<<"$(mon21_body "$jobs")"; then
    found_job=1
    break
  fi
  sleep 2
done
(( found_job == 1 )) || fail "mon_21: the label values of job for mimic_build_info{component=\"controller\"} lack $RELEASE-controller"

# Every expression of every dashboard parses on the real Prometheus (status success; an empty result is allowed), with
# the selectors set to the release, the injected deployment of mon_02 and an empty load test.
dash_exprs=$(for f in "$dash_dir"/*.json; do
  jq -c --arg d "$(basename "$f" .json)" --arg c "$RELEASE-controller" --arg ns "$mon21_ns" --arg rel "$NS" --arg dep mon21-app '
    [.. | objects | select((.expr? | type) == "string") | .expr]
    | map(gsub("\\$__rate_interval"; "2m") | gsub("\\$__range"; "1h") | gsub("\\$controller"; $c)
        | gsub("\\$namespace"; (if $d == "mimic-valkey" then $rel else $ns end)) | gsub("\\$deployment"; $dep)
        | gsub("\\$load_test"; ""))
    | map({d: $d, e: .})[]' "$f"
done | jq -s -c .)
mon21_expr_py=$(cat <<'PY'
import json, sys, urllib.error, urllib.parse, urllib.request
bad = 0
items = json.load(sys.stdin)
for it in items:
    url = sys.argv[1] + "/api/v1/query?" + urllib.parse.urlencode({"query": it["e"]})
    try:
        body = json.load(urllib.request.urlopen(url, timeout=30))
        ok, err = body.get("status") == "success", body.get("error", "")
    except urllib.error.HTTPError as e:
        ok, err = False, e.read().decode("utf-8", "replace")[:300]
    except OSError as e:
        ok, err = False, str(e)
    if not ok:
        bad += 1
        print("FAIL", it["d"], it["e"], err)
print("checked", len(items), "bad", bad)
PY
)
expr_out=$(printf '%s' "$dash_exprs" | kubectl -n "$NS" exec -i mon21-prom -- python3 -I -c "$mon21_expr_py" "http://$RELEASE-prometheus:9090" 2>&1 || true)
[[ $(tail -n 1 <<<"$expr_out") =~ ^checked\ [1-9][0-9]*\ bad\ 0$ ]] \
  || fail "mon_21: a dashboard expression is not accepted by Prometheus: $(grep '^FAIL' <<<"$expr_out" | head -n 3)"

# One panel query of mimic-sidecar through Grafana, with the datasource uid of this test, after requests through the
# injected deployment: at least one frame holds a value above 0 within 120 s. The password goes on stdin with the body.
mon21_ds_py=$(cat <<'PY'
import base64, sys, urllib.error, urllib.request
pw, _, body = sys.stdin.read().partition("\n")
auth = "Basic " + base64.b64encode(("admin:" + pw).encode()).decode()
req = urllib.request.Request(sys.argv[1], data=body.encode(), method="POST",
                             headers={"Content-Type": "application/json", "Authorization": auth})
try:
    r = urllib.request.urlopen(req, timeout=30)
    code, out = r.status, r.read()
except urllib.error.HTTPError as e:
    code, out = e.code, e.read()
except OSError:
    code, out = 0, b""
sys.stdout.write("%d\n%s" % (code, out.decode("utf-8", "replace")))
PY
)
panel_expr=$(jq -r --arg ns "$mon21_ns" --arg dep mon21-app '
  .panels[] | select(.title == "Requests by direction and authority") | .targets[0].expr
  | gsub("\\$__rate_interval"; "2m") | gsub("\\$namespace"; $ns) | gsub("\\$deployment"; $dep)' "$dash_dir/mimic-sidecar.json")
[[ -n $panel_expr ]] || fail "mon_21: the mimic-sidecar panel Requests by direction and authority has no query"
ds_body=$(jq -n -c --arg e "$panel_expr" '{queries: [{refId: "A", datasource: {type: "prometheus", uid: "mimic-prometheus"},
  expr: $e, range: true, instant: false, intervalMs: 15000, maxDataPoints: 100}], from: "now-10m", to: "now"}')
traffic='import urllib.request
for _ in range(5):
    urllib.request.urlopen("http://mon21-echo:8080/ok", timeout=10).read()'
has_value=0
deadline=$((SECONDS + 120))
while (( SECONDS < deadline )); do
  kubectl -n "$mon21_ns" exec deploy/mon21-app -- python3 -c "$traffic" >/dev/null 2>&1 || true
  ds=$(printf '%s\n%s' "$grafana_pw" "$ds_body" | kubectl -n "$NS" exec -i mon21-client -- python3 -I -c "$mon21_ds_py" \
    "http://$RELEASE-grafana:3000/api/ds/query" 2>/dev/null || true)
  if [[ $(mon21_status "$ds") == 200 ]] \
    && jq -e '[.results.A.frames[]?.data.values[1]?[]? | select(. != null and . > 0)] | length > 0' >/dev/null 2>&1 <<<"$(mon21_body "$ds")"; then
    has_value=1
    break
  fi
  sleep 5
done
(( has_value == 1 )) || fail "mon_21: the mimic-sidecar panel query through Grafana returns no value above 0"

# The lifecycle and admin APIs are off (MON-16): Prometheus refuses a reload and a TSDB snapshot with a 4xx or 5xx.
mon21_post='import sys, urllib.error, urllib.request
req = urllib.request.Request(sys.argv[1], data=b"", method="POST")
try:
    code = urllib.request.urlopen(req, timeout=10).status
except urllib.error.HTTPError as e:
    code = e.code
except OSError:
    code = 0
print(code)'
for path in /-/reload /api/v1/admin/tsdb/snapshot; do
  code=$(kubectl -n "$NS" exec mon21-prom -- python3 -I -c "$mon21_post" "http://$RELEASE-prometheus:9090$path" 2>/dev/null || echo 0)
  (( code >= 400 && code < 600 )) || fail "mon_21: POST $path answered $code, not a refusal"
done

# A second upgrade with the same values keeps the admin Secret: the hash of its password value is unchanged (the value
# is never printed).
secret_hash() { kubectl -n "$NS" get secret $RELEASE-grafana-admin -o jsonpath='{.data.admin-password}' | sha256sum; }
before=$(secret_hash)
install --set valkey.metrics.enabled=true --set monitoring.prometheus.enabled=true --set monitoring.grafana.enabled=true \
  || fail "mon_21: the second helm upgrade with the same values failed"
[[ $(secret_hash) == "$before" ]] || fail "mon_21: the second helm upgrade changed the Grafana admin Secret"

# The admin password is in no log of the release namespace (never printed: the check is grep -qF on a variable).
logs=$(kubectl -n "$NS" logs --all-containers --selector "app.kubernetes.io/instance=$RELEASE" --tail=-1 2>/dev/null || true)
if grep -qF -- "$grafana_pw" <<<"$logs"; then fail "mon_21: the admin password appears in a log of the release namespace"; fi

# The Prometheus and Grafana ServiceAccounts cannot read Secrets, in the release namespace or the injected one.
for sa in $RELEASE-prometheus $RELEASE-grafana; do
  for ns in $NS $mon21_ns; do
    [[ $(kubectl auth can-i get secrets -n "$ns" --as=system:serviceaccount:$NS:$sa || true) == no ]] \
      || fail "mon_21: $sa may get secrets in $ns"
  done
done
echo "mon_21: Prometheus targets up, Grafana healthy and provisioned, the dashboard listed, no password in a log"

# Back to the defaults: the later sections see the chart without the bundled monitoring.
kubectl delete namespace $mon21_ns --wait=false >/dev/null
kubectl -n "$NS" delete pod mon21-client mon21-prom --wait=false >/dev/null
kubectl -n "$NS" delete configmap mon21-dashboard mon21-dashboard-off --ignore-not-found >/dev/null
install || fail "mon_21: helm upgrade back to the defaults"
kubectl -n "$NS" rollout status deploy/$RELEASE-controller --timeout=2m
if kubectl -n "$NS" get deploy $RELEASE-grafana >/dev/null 2>&1; then fail "mon_21: Grafana is still rendered with the defaults"; fi

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
