#!/usr/bin/env bash
# End-to-end checks of the load test on kind (LDT-06, LDT-17; spec §7.8, §11).
# Idempotent: reuses the cluster if it exists and upgrades the release mimic in place. It
# shares the cluster and the release with scripts/e2e.sh: never run the two at the same time.
#
#   TAG=e2e-load-<sha>-<pid>                    tag of the images built here (unique per run)
#   IMAGE=ghcr.io/thaliph/mimic:$TAG            image to test (built here unless SKIP_BUILD=1)
#   INIT_IMAGE=ghcr.io/thaliph/mimic-init:$TAG  mimic-init image (built here unless SKIP_BUILD=1)
#   CLUSTER=mimic-e2e                           kind cluster name
#   NODE_IMAGE=kindest/node:v1.33.12@sha256:…    kind node image (default: kind's own)
#   KEEP_CLUSTER=1                              keep the cluster afterwards (default: keep locally, delete in CI)
#
# The chart needs image.digest with loadTest.enabled: the digest is the one the node's containerd
# holds for IMAGE, read with crictl. The Job and NetworkPolicy checked below are the objects that
# mimic_core::load::job builds (examples/load_objects.rs), created as the controller account with a
# server-side dry run, so the chart's ValidatingAdmissionPolicy is what decides.
set -Eeuo pipefail
# A command that fails outside a condition ends the run: name it, so that no exit is silent.
trap 'e2e_status=$?; echo "FAIL line $LINENO: \"$BASH_COMMAND\" exited with status $e2e_status" >&2' ERR
cd "$(dirname "$0")/.."

TAG=${TAG:-e2e-load-$(git rev-parse --short HEAD 2>/dev/null || echo local)-$$}
IMAGE=${IMAGE:-ghcr.io/thaliph/mimic:$TAG}
INIT_IMAGE=${INIT_IMAGE:-ghcr.io/thaliph/mimic-init:$TAG}
CLUSTER=${CLUSTER:-mimic-e2e}
NS=mimic-system
RELEASE=mimic
LOAD_NS=load-e2e
PLAIN_NS=load-plain
CTL="system:serviceaccount:$NS:$RELEASE-controller"
KEEP_CLUSTER=${KEEP_CLUSTER:-$([[ -n ${CI:-} ]] && echo 0 || echo 1)}
BUILT=0
WORK=$(mktemp -d)

# found <grep args>: 0 on a match, 1 on none; an error of grep stops the script.
found() {
  local status=0
  grep "$@" >/dev/null || status=$?
  if (( status > 1 )); then
    echo "FAIL grep exited with status $status" >&2
    exit 1
  fi
  return "$status"
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
  [[ $BUILT == 1 ]] && docker rmi "$IMAGE" "$INIT_IMAGE" >/dev/null 2>&1 || true
  rm -rf "$WORK"
  [[ $KEEP_CLUSTER == 1 ]] || kind delete cluster --name "$CLUSTER"
}
trap cleanup EXIT

# --- the chart without a cluster (LDT-06) -----------------------------------------------------
log "ldt_06: the default chart installs no load test object and no load test RBAC or policy"
off=$(helm template "$RELEASE" deploy/helm/mimic -n "$NS")
for word in MimicLoadTest mimicloadtests endpointslices networkpolicies "kind: Job" "-load-job" "-load-secret"; do
  if found -F "$word" <<<"$off"; then fail "ldt_06: the default chart renders $word"; fi
done
[[ $(grep -c '^kind: ValidatingAdmissionPolicy$' <<<"$off" || true) == 1 ]] \
  || fail "ldt_06: the default chart renders a ValidatingAdmissionPolicy besides the injector's"
[[ $(awk '/name: MIMIC_LOAD_MAX_RUNS/ { getline; print }' <<<"$off") == *'"0"'* ]] \
  || fail "ldt_06: MIMIC_LOAD_MAX_RUNS is not 0 by default"

log "ldt_06: with loadTest.enabled an image given by tag only does not render, and the message names image.digest"
if helm template "$RELEASE" deploy/helm/mimic -n "$NS" --set loadTest.enabled=true --set image.tag=v1 >/dev/null 2>&1; then
  fail "ldt_06: loadTest.enabled with a tag only rendered"
fi
tag_only=$(helm template "$RELEASE" deploy/helm/mimic -n "$NS" --set loadTest.enabled=true --set image.tag=v1 2>&1 || true)
found -F "image.digest" <<<"$tag_only" || fail "ldt_06: the message does not name image.digest: $tag_only"

log "ldt_06: with loadTest.enabled and a digest the chart adds the CRD and the two policies"
on=$(helm template "$RELEASE" deploy/helm/mimic -n "$NS" --set loadTest.enabled=true \
  --set image.digest=sha256:0000000000000000000000000000000000000000000000000000000000000000)
[[ $(grep -c '^kind: ValidatingAdmissionPolicy$' <<<"$on" || true) == 3 ]] \
  || fail "ldt_06: loadTest.enabled does not render the two load test policies"
[[ $(awk '/name: MIMIC_LOAD_MAX_RUNS/ { getline; print }' <<<"$on") == *'"4"'* ]] \
  || fail "ldt_06: loadTest.enabled does not set MIMIC_LOAD_MAX_RUNS to 4"

# --- cluster, images, release -------------------------------------------------------------------
clusters=$(kind get clusters) || { echo "FAIL kind get clusters failed" >&2; exit 1; }
if ! found -x "$CLUSTER" <<<"$clusters"; then
  log "create kind cluster $CLUSTER"
  node_args=()
  [[ -z ${NODE_IMAGE:-} ]] || node_args=(--image "$NODE_IMAGE")
  kind create cluster --name "$CLUSTER" --wait 120s "${node_args[@]}"
fi
kubectl config use-context "kind-$CLUSTER" >/dev/null

if [[ ${SKIP_BUILD:-0} != 1 ]]; then
  log "build $IMAGE and $INIT_IMAGE"
  docker build -q -t "$IMAGE" .
  docker build -q -t "$INIT_IMAGE" --target init .
  BUILT=1
fi
kind load docker-image --name "$CLUSTER" "$IMAGE" "$INIT_IMAGE"

# image_digest <tag>: the manifest digest that the node's containerd holds for the tag.
image_digest() {
  docker exec "$CLUSTER-control-plane" crictl images --digests -o json \
    | jq -r --arg tag "$1" '.images[] | select(.repoTags | index($tag)) | .repoDigests[]' \
    | sed -n 's/.*@\(sha256:[0-9a-f]\{64\}\)$/\1/p' \
    | head -n 1
}
digest=$(image_digest "$IMAGE" || true)
[[ $digest =~ ^sha256:[0-9a-f]{64}$ ]] || fail "ldt_06: no digest of $IMAGE on the node (got '$digest')"
repo=${IMAGE%:*}; init_repo=${INIT_IMAGE%:*}; init_tag=${INIT_IMAGE##*:}
mimic_ref="$repo@$digest"
# The chart pulls the image by digest and pullPolicy is Never: containerd knows the image by its tag
# only, so the node gets the digest reference as an alias of the same image.
docker exec "$CLUSTER-control-plane" ctr -n k8s.io images tag --force "$IMAGE" "$mimic_ref" >/dev/null

kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl label --overwrite namespace "$NS" \
  pod-security.kubernetes.io/enforce=restricted pod-security.kubernetes.io/warn=restricted >/dev/null
log "ldt_06: helm install with loadTest.enabled"
# install [helm flags]: the release with loadTest.enabled, the images under test and the given flags.
install() {
  helm upgrade --install "$RELEASE" deploy/helm/mimic -n "$NS" \
    --set image.repository="$repo" --set image.digest="$digest" --set image.pullPolicy=Never \
    --set inject.initImage.repository="$init_repo" --set inject.initImage.tag="$init_tag" \
    --set loadTest.enabled=true "$@" --wait --timeout 4m
}
install || fail "ldt_06: helm install"
kubectl -n "$NS" rollout status deploy/$RELEASE-controller --timeout=2m

# Namespaces of the admission checks: one injected, one not.
for ns in "$LOAD_NS" "$PLAIN_NS"; do
  kubectl delete namespace "$ns" --ignore-not-found --wait=true --timeout=2m >/dev/null
  kubectl create namespace "$ns" >/dev/null
done
kubectl label namespace "$LOAD_NS" mimic.platform.io/inject=enabled >/dev/null

# --- LDT-06: the CRD and admission of MimicLoadTest --------------------------------------------
# lt_json <name> <spec JSON>: a MimicLoadTest of the load namespace.
lt_json() {
  jq -nc --arg name "$1" --arg ns "$LOAD_NS" --argjson spec "$2" \
    '{apiVersion: "mimic.platform.io/v1alpha1", kind: "MimicLoadTest", metadata: {name: $name, namespace: $ns}, spec: $spec}'
}
# refused <description> <manifest file> <kubectl args...>: the request fails with a schema, CEL or admission error.
refused() {
  local desc=$1 file=$2 out
  shift 2
  if out=$(kubectl "$@" -f "$file" 2>&1); then fail "ldt_06: $desc was accepted"; fi
  found -e 'Invalid value' -e 'Required value' -e 'denied request' -e 'Unsupported value' <<<"$out" \
    || fail "ldt_06: $desc was refused for another reason: $out"
}
accepted() {
  local desc=$1 file=$2 out
  shift 2
  out=$(kubectl "$@" -f "$file" 2>&1) || fail "ldt_06: $desc was refused: $out"
}
# spec_case <description> <spec JSON>: the server dry run of a MimicLoadTest with this spec is refused.
spec_case() {
  lt_json lt-case "$2" > "$WORK/case.json"
  refused "$1" "$WORK/case.json" create --dry-run=server
}
lt_spec_base='{"deployment": "front", "duration": "5m"}'

log "ldt_06: the CRD has the printer columns of spec §7.8 and the status subresource"
columns=$(kubectl get crd mimicloadtests.mimic.platform.io \
  -o jsonpath='{range .spec.versions[0].additionalPrinterColumns[*]}{.name}{"|"}{end}')
[[ $columns == 'Deployment|Phase|Reason|Requests|RPS|P99 µs|Age|' ]] || fail "ldt_06: printer columns are '$columns'"
[[ $(kubectl get crd mimicloadtests.mimic.platform.io -o jsonpath='{.spec.versions[0].subresources.status}') == '{}' ]] \
  || fail "ldt_06: the CRD has no status subresource"

log "ldt_06: defaults: concurrency 10, rampUp 0s, timeout 10s, stop false"
lt_json lt-defaults "$lt_spec_base" > "$WORK/defaults.json"
[[ $(kubectl create --dry-run=server -o json -f "$WORK/defaults.json" | jq -r '.spec | [.concurrency, .rampUp, .timeout, .stop] | @tsv') \
  == $'10\t0s\t10s\tfalse' ]] || fail "ldt_06: the defaults differ from spec §7.8"

log "ldt_06: a request without deployment or duration, or outside the bounds of the CRD, is refused"
spec_case "a MimicLoadTest without deployment" '{"duration": "5m"}'
spec_case "a MimicLoadTest without duration" '{"deployment": "front"}'
spec_case "a bad deployment name" '{"deployment": "Front_1", "duration": "5m"}'
spec_case "concurrency 0" '{"deployment": "front", "duration": "5m", "concurrency": 0}'
spec_case "concurrency 4097" '{"deployment": "front", "duration": "5m", "concurrency": 4097}'
spec_case "rate 0" '{"deployment": "front", "duration": "5m", "rate": 0}'
spec_case "duration 5s" '{"deployment": "front", "duration": "5s"}'
spec_case "duration 25h" '{"deployment": "front", "duration": "25h"}'
spec_case "timeout 50ms" '{"deployment": "front", "duration": "5m", "timeout": "50ms"}'
spec_case "timeout 61s" '{"deployment": "front", "duration": "5m", "timeout": "61s"}'
spec_case "65 include entries" "$(jq -nc --argjson base "$lt_spec_base" '$base + {endpoints: {include: [range(65) | "GET /p\(.)"]}}')"
spec_case "33 credential fields" "$(jq -nc --argjson base "$lt_spec_base" \
  '$base + {credentials: {secretName: "payment-load", fields: [range(33) | {field: "header/x-\(.)", key: "token"}]}}')"
spec_case "a credential field without header/, query/ or body/" \
  '{"deployment": "front", "duration": "5m", "credentials": {"secretName": "payment-load", "fields": [{"field": "authorization", "key": "token"}]}}'
spec_case "two credential fields for one field" \
  '{"deployment": "front", "duration": "5m", "credentials": {"secretName": "payment-load", "fields": [{"field": "header/authorization", "key": "a"}, {"field": "header/authorization", "key": "b"}]}}'

log "ldt_06: a valid MimicLoadTest with every field is admitted"
lt_json lt-full '{"deployment": "front", "duration": "5m", "concurrency": 8, "rate": 50, "rampUp": "10s", "timeout": "2s",
  "endpoints": {"include": ["POST /payments"], "exclude": ["DELETE /payments/{payments_id}"]},
  "credentials": {"secretName": "payment-load", "fields": [{"field": "header/authorization", "key": "token"}]}}' > "$WORK/full.json"
accepted "a valid MimicLoadTest" "$WORK/full.json" create --dry-run=server

log "ldt_06: a live MimicLoadTest: only stop may change, and only from false to true"
lt_json lt-live '{"deployment": "front", "duration": "5m", "concurrency": 8, "rate": 50, "rampUp": "10s"}' > "$WORK/live.json"
kubectl create -f "$WORK/live.json" >/dev/null || fail "ldt_06: the live MimicLoadTest was not created"
patch_refused() {
  if out=$(kubectl patch mimicloadtest lt-live -n "$LOAD_NS" --type=merge --dry-run=server -p "$1" 2>&1); then
    fail "ldt_06: $2 was accepted"
  fi
  found -e 'Invalid value' -e 'denied request' <<<"$out" || fail "ldt_06: $2 was refused for another reason: $out"
}
patch_refused '{"spec":{"duration":"6m"}}' "a change of duration"
patch_refused '{"spec":{"deployment":"back"}}' "a change of deployment"
patch_refused '{"spec":{"concurrency":9}}' "a change of concurrency"
patch_refused '{"spec":{"rate":null}}' "the removal of rate"
patch_refused '{"spec":{"rampUp":"20s"}}' "a change of rampUp"
patch_refused '{"spec":{"endpoints":{"include":["GET /a"]}}}' "an added endpoints field"
kubectl patch mimicloadtest lt-live -n "$LOAD_NS" --type=merge --dry-run=server -p '{"spec":{"stop":true}}' >/dev/null \
  || fail "ldt_06: stop from false to true was refused"
kubectl patch mimicloadtest lt-live -n "$LOAD_NS" --type=merge -p '{"spec":{"stop":true}}' >/dev/null \
  || fail "ldt_06: stop from false to true was not applied"
patch_refused '{"spec":{"stop":false}}' "stop from true to false"
patch_refused '{"spec":{"duration":"6m"}}' "a change of duration after stop"

log "ldt_06: a user who may create MimicLoadTests but not get the Secret they name is refused; with get it is admitted"
for sa in lt-author lt-reader; do
  kubectl -n "$LOAD_NS" create serviceaccount "$sa" >/dev/null 2>&1 || true
done
kubectl apply -f - >/dev/null <<YAML
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata: {name: lt-user, namespace: $LOAD_NS}
rules:
  - apiGroups: [mimic.platform.io]
    resources: [mimicloadtests]
    verbs: [create, get, update, patch]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata: {name: lt-secret-reader, namespace: $LOAD_NS}
rules:
  - apiGroups: [""]
    resources: [secrets]
    resourceNames: [payment-load]
    verbs: [get]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata: {name: lt-author, namespace: $LOAD_NS}
roleRef: {apiGroup: rbac.authorization.k8s.io, kind: Role, name: lt-user}
subjects: [{kind: ServiceAccount, name: lt-author, namespace: $LOAD_NS}]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata: {name: lt-reader, namespace: $LOAD_NS}
roleRef: {apiGroup: rbac.authorization.k8s.io, kind: Role, name: lt-user}
subjects: [{kind: ServiceAccount, name: lt-reader, namespace: $LOAD_NS}]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata: {name: lt-reader-secret, namespace: $LOAD_NS}
roleRef: {apiGroup: rbac.authorization.k8s.io, kind: Role, name: lt-secret-reader}
subjects: [{kind: ServiceAccount, name: lt-reader, namespace: $LOAD_NS}]
YAML
lt_json lt-named "$(jq -nc '{deployment: "front", duration: "5m", credentials: {secretName: "payment-load", fields: [{field: "header/authorization", key: "token"}]}}')" > "$WORK/named.json"
if out=$(kubectl --as="system:serviceaccount:$LOAD_NS:lt-author" create --dry-run=server -f "$WORK/named.json" 2>&1); then
  fail "ldt_06: a user who may not get the Secret was admitted"
fi
found -F 'payment-load' <<<"$out" || fail "ldt_06: the refusal does not name the Secret: $out"
kubectl --as="system:serviceaccount:$LOAD_NS:lt-reader" create --dry-run=server -f "$WORK/named.json" >/dev/null \
  || fail "ldt_06: a user who may get the Secret was refused"
lt_json lt-plain-author '{"deployment": "front", "duration": "5m"}' > "$WORK/plain.json"
kubectl --as="system:serviceaccount:$LOAD_NS:lt-author" create --dry-run=server -f "$WORK/plain.json" >/dev/null \
  || fail "ldt_06: a MimicLoadTest without credentials was refused for its author"

# --- LDT-17: RBAC of the controller ------------------------------------------------------------
# can <yes|no> <description> <verb and resource...>: `kubectl auth can-i` as the controller account.
can() {
  local want=$1 desc=$2 got
  shift 2
  got=$(kubectl auth can-i --as="$CTL" "$@" 2>/dev/null) || true
  [[ $got == "$want" ]] || fail "ldt_17: $desc (want $want, got '$got')"
}
log "ldt_17: the controller may create Jobs and NetworkPolicies and list EndpointSlices in an injected namespace"
can yes "create jobs in an injected namespace" create jobs.batch -n "$LOAD_NS"
can yes "list endpointslices" list endpointslices.discovery.k8s.io -n "$LOAD_NS"
can yes "watch endpointslices" watch endpointslices.discovery.k8s.io -A
can yes "create networkpolicies" create networkpolicies.networking.k8s.io -n "$LOAD_NS"
can yes "delete jobs" delete jobs.batch -n "$LOAD_NS"
can yes "delete networkpolicies" delete networkpolicies.networking.k8s.io -n "$LOAD_NS"
can yes "patch mimicloadtests/status" patch mimicloadtests.mimic.platform.io --subresource=status -n "$LOAD_NS"
can yes "list mimicloadtests" list mimicloadtests.mimic.platform.io -A
log "ldt_17: the controller may not read Secrets or pods, nor update a MimicLoadTest, nor add an ephemeral container"
can no "get secrets" get secrets -n "$LOAD_NS"
can no "get the credentials Secret" get secret/payment-load -n "$LOAD_NS"
can no "get pods" get pods -n "$LOAD_NS"
can no "update mimicloadtests" update mimicloadtests.mimic.platform.io -n "$LOAD_NS"
can no "add ephemeral containers" update pods/ephemeralcontainers -n "$LOAD_NS"

# --- LDT-17: the admission policy of the generator's objects ------------------------------------
# The objects of mimic_core::load::job for a run of concurrency 300: 3 generator pods of 128.
load_input() { # load_input <Secret name, or empty>
  jq -nc --arg image "$mimic_ref" --arg secret "$1" '{
    namespace: "load-e2e", name: "lt-live", uid: "550e8400-e29b-41d4-a716-446655440000",
    deployment: "front", concurrency: 300, durationMs: 60000, podConcurrency: 128,
    startTimeoutMs: 300000, reportIntervalMs: 5000, maxPlan: 8388608, image: $image,
    resources: {requests: {cpu: "500m", memory: "128Mi"}, limits: {cpu: "2", memory: "256Mi"}},
    controllerUrl: "https://mimic-controller.mimic-system.svc:8444", controllerCa: "unused in the dry run",
    secretName: (if $secret == "" then null else $secret end),
    ports: [8080], controllerNamespace: "mimic-system",
    controllerLabels: {"app.kubernetes.io/instance": "mimic", "app.kubernetes.io/name": "mimic-controller"},
    controllerPort: 8444, dnsNamespace: "kube-system", dnsLabels: {"k8s-app": "kube-dns"}, metricsFrom: []
  }'
}
load_input "" | cargo run -q --locked -p mimic-core --example load_objects > "$WORK/objects.json"
load_input payment-load | cargo run -q --locked -p mimic-core --example load_objects > "$WORK/objects-secret.json"
jq '.job' "$WORK/objects.json" > "$WORK/job.json"
jq '.job' "$WORK/objects-secret.json" > "$WORK/job-secret.json"
jq '.networkPolicy' "$WORK/objects.json" > "$WORK/np.json"
[[ $(jq -r '.metadata.name' "$WORK/job.json") == mimic-load-550e8400e29b ]] || fail "ldt_17: the Job name is not mimic-load-<12 hex>"

# admitted <description> <file>: the controller's server dry run is accepted.
admitted() {
  local out
  out=$(kubectl --as="$CTL" create --dry-run=server -f "$2" 2>&1) || fail "ldt_17: $1 was refused: $out"
}
# refuse <message> <base file> <jq filter>: the controller's dry run of the object, changed by the
# filter, is refused, and the refusal names the rule (its message holds <message>).
refuse() {
  local key=$1 base=$2 filter=$3 out
  jq "$filter" "$WORK/$base.json" > "$WORK/refused.json" || fail "ldt_17: the filter for '$key' does not apply: $filter"
  if out=$(kubectl --as="$CTL" create --dry-run=server -f "$WORK/refused.json" 2>&1); then
    fail "ldt_17: an object refused for '$key' was admitted"
  fi
  found -F "$key" <<<"$out" || fail "ldt_17: the refusal does not name '$key': $out"
}

log "ldt_17: the objects of the controller are admitted (the Job with and without a Secret, the NetworkPolicy)"
admitted "the Job" "$WORK/job.json"
admitted "the Job with the credentials Secret" "$WORK/job-secret.json"
admitted "the NetworkPolicy" "$WORK/np.json"

log "ldt_17: the Job policy refuses a Job with another image, args, a second container or an init container"
other_digest=sha256:1111111111111111111111111111111111111111111111111111111111111111
refuse "image must be" job '.spec.template.spec.containers[0].image = "'"$repo@$other_digest"'"'
refuse "image must be" job '.spec.template.spec.containers[0].image = "'"$repo:v1"'"'
refuse "args must be absent" job '.spec.template.spec.containers[0].args = ["--x"]'
refuse "exactly one container" job '.spec.template.spec.containers += [{"name": "side", "image": "'"$mimic_ref"'"}]'
refuse "no init or ephemeral container" job '.spec.template.spec.initContainers = [{"name": "init", "image": "busybox"}]'
refuse "command must be" job '.spec.template.spec.containers[0].command = ["/bin/sh"]'

log "ldt_17: the Job policy refuses env with valueFrom, a name outside spec 10.6, envFrom"
refuse "valueFrom" job '.spec.template.spec.containers[0].env[0] = {"name": "MIMIC_LOAD_RUN", "valueFrom": {"fieldRef": {"fieldPath": "metadata.name"}}}'
refuse "env names of spec 10.6" job '.spec.template.spec.containers[0].env += [{"name": "MIMIC_EXTRA", "value": "1"}]'
refuse "envFrom must be absent" job '.spec.template.spec.containers[0].envFrom = [{"secretRef": {"name": "payment-load"}}]'

log "ldt_17: the Job policy refuses another service account, automounted tokens and other token projections"
refuse "automountServiceAccountToken must be false" job '.spec.template.spec.automountServiceAccountToken = true'
refuse "serviceAccountName must be default" job '.spec.template.spec.serviceAccountName = "mimic-controller"'
refuse "audience must be mimic.platform.io/load" job 'del(.spec.template.spec.volumes[0].projected.sources[0].serviceAccountToken.audience)'
refuse "audience must be mimic.platform.io/load" job '.spec.template.spec.volumes[0].projected.sources[0].serviceAccountToken.audience = ""'
refuse "audience must be mimic.platform.io/load" job '.spec.template.spec.volumes[0].projected.sources[0].serviceAccountToken.audience = "https://kubernetes.default.svc"'
refuse "expirationSeconds must be at most 600" job '.spec.template.spec.volumes[0].projected.sources[0].serviceAccountToken.expirationSeconds = 3600'
refuse "one serviceAccountToken" job '.spec.template.spec.volumes[0].projected.sources += [{"downwardAPI": {"items": [{"path": "n", "fieldRef": {"fieldPath": "metadata.name"}}]}}]'

log "ldt_17: the Job policy refuses host volumes, a second Secret, a Secret not read-only at /mimic/credentials"
refuse "volumes must be" job '.spec.template.spec.volumes += [{"name": "host", "hostPath": {"path": "/"}}]'
refuse "at most one Secret volume" job-secret '.spec.template.spec.volumes += [{"name": "more", "secret": {"secretName": "payment-load"}}]'
refuse "every volume mount must be read-only" job-secret '.spec.template.spec.containers[0].volumeMounts |= map(if .name == "credentials" then .readOnly = false else . end)'
refuse "the Secret volume must be mounted at /mimic/credentials" job-secret '.spec.template.spec.containers[0].volumeMounts |= map(if .name == "credentials" then .mountPath = "/mimic/other" else . end)'

log "ldt_17: the Job policy refuses host namespaces, hostPort, nodeName, tolerations, hostAliases"
refuse "hostNetwork must be" job '.spec.template.spec.hostNetwork = true'
refuse "hostPID must be" job '.spec.template.spec.hostPID = true'
refuse "hostIPC must be" job '.spec.template.spec.hostIPC = true'
refuse "hostUsers must be" job '.spec.template.spec.hostUsers = true'
refuse "hostPort must be absent" job '.spec.template.spec.containers[0].ports = [{"containerPort": 8080, "hostPort": 8080}]'
refuse "nodeName must be absent" job '.spec.template.spec.nodeName = "mimic-e2e-control-plane"'
refuse "tolerations must be absent" job '.spec.template.spec.tolerations = [{"operator": "Exists"}]'
refuse "hostAliases must be absent" job '.spec.template.spec.hostAliases = [{"ip": "10.0.0.1", "hostnames": ["x"]}]'

log "ldt_17: the Job policy refuses the fields the generator never sets (priority, runtime class, scheduler, affinity, SELinux, AppArmor, parallelism)"
refuse "priorityClassName must be absent" job '.spec.template.spec.priorityClassName = "system-cluster-critical"'
refuse "runtimeClassName must be absent" job '.spec.template.spec.runtimeClassName = "gvisor"'
refuse "affinity must be absent" job '.spec.template.spec.affinity = {"podAntiAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": [{"topologyKey": "kubernetes.io/hostname"}]}}'
refuse "seLinuxOptions must be absent" job '.spec.template.spec.securityContext.seLinuxOptions = {"type": "spc_t"}'
refuse "seLinuxOptions must be absent" job '.spec.template.spec.containers[0].securityContext.seLinuxOptions = {"type": "spc_t"}'
refuse "appArmorProfile must be absent" job '.spec.template.spec.securityContext.appArmorProfile = {"type": "Unconfined"}'
refuse "appArmorProfile must be absent" job '.spec.template.spec.containers[0].securityContext.appArmorProfile = {"type": "Unconfined"}'
refuse "schedulerName must be default-scheduler" job '.spec.template.spec.schedulerName = "other-scheduler"'
refuse "parallelism must equal completions" job '.spec.parallelism = 2'

log "ldt_17: the Secret volume is the one the annotation names, and the annotation goes with a Secret volume"
refuse "the Secret volume must be the credentials Secret" job-secret '.spec.template.spec.volumes |= map(if .name == "credentials" then .secret.secretName = "other-secret" else . end)'
refuse "the Secret volume must be the credentials Secret" job-secret 'del(.metadata.annotations["mimic.platform.io/credentials-secret"])'
refuse "the Secret volume must be the credentials Secret" job '.metadata.annotations["mimic.platform.io/credentials-secret"] = "payment-load"'

log "ldt_17: the Job policy refuses a privileged or root container, an added capability, a weaker security context"
# allowPrivilegeEscalation must go with it: the API server refuses privileged with it set to false.
refuse "privileged must be" job '.spec.template.spec.containers[0].securityContext.privileged = true | .spec.template.spec.containers[0].securityContext.allowPrivilegeEscalation = true'
refuse "UID 65532" job '.spec.template.spec.containers[0].securityContext.runAsUser = 0'
refuse "runAsNonRoot must be true" job '.spec.template.spec.containers[0].securityContext.runAsNonRoot = false'
refuse "allowPrivilegeEscalation must be false" job '.spec.template.spec.containers[0].securityContext.allowPrivilegeEscalation = true'
refuse "readOnlyRootFilesystem must be true" job '.spec.template.spec.containers[0].securityContext.readOnlyRootFilesystem = false'
refuse "capabilities must drop ALL" job '.spec.template.spec.containers[0].securityContext.capabilities.add = ["NET_ADMIN"]'
refuse "capabilities must drop ALL" job '.spec.template.spec.containers[0].securityContext.capabilities.drop = []'
refuse "seccompProfile must be RuntimeDefault" job '.spec.template.spec.securityContext.seccompProfile.type = "Unconfined"'

log "ldt_17: the Job policy refuses Jobs without the labels of spec 7.8, and namespaces without the injection label"
refuse "metadata labels must" job 'del(.metadata.labels["mimic.platform.io/load-test"])'
refuse "pod labels must" job 'del(.spec.template.metadata.labels["mimic.platform.io/inject"])'
refuse "the namespace must have mimic.platform.io/inject" job '.metadata.namespace = "'"$PLAIN_NS"'"'

log "ldt_17: the NetworkPolicy policy refuses another shape"
refuse "podSelector must select" np '.spec.podSelector = {"matchLabels": {"app": "front"}}'
refuse "policyTypes must be" np '.spec.policyTypes = ["Egress"]'
refuse "ingress must be absent" np '.spec.ingress = [{"ports": [{"protocol": "TCP", "port": 80}]}]'
refuse "three rules" np '.spec.egress += [{"to": [{"ipBlock": {"cidr": "10.0.0.0/8"}}]}]'
refuse "egress[0] must be" np '.spec.egress[0].to = [{"ipBlock": {"cidr": "0.0.0.0/0"}}]'
refuse "egress[0] must be" np '.spec.egress[0].to = [{"namespaceSelector": {}}]'
refuse "egress[1] must be" np '.spec.egress[1].ports = [{"protocol": "TCP", "port": 22}]'
refuse "egress[2] must be" np '.spec.egress[2].to = [{"podSelector": {}}]'
refuse "the namespace must have mimic.platform.io/inject" np '.metadata.namespace = "'"$PLAIN_NS"'"'

log "ldt_17: the policy matches the controller only: the same Job from the cluster administrator is admitted"
jq '.spec.template.spec.containers[0].args = ["--x"]' "$WORK/job.json" > "$WORK/by-admin.json"
out=$(kubectl create --dry-run=server -f "$WORK/by-admin.json" 2>&1) \
  || fail "ldt_17: the policy refused a Job of another user: $out"

# The metrics source (loadTest.metricsFrom) changes the ingress rule of the policy: the release renders and
# the policy compiles with it, admits the rule of the source and refuses another port (LDT-17).
log "ldt_17: with loadTest.metricsFrom the NetworkPolicy admits the metrics source on the admin port and nothing else"
install --set 'loadTest.metricsFrom[0].podSelector.matchLabels.app=scrape' || fail "ldt_17: helm install with metricsFrom"
metrics_ingress='[{"from": [{"podSelector": {"matchLabels": {"app": "scrape"}}}], "ports": [{"protocol": "TCP", "port": 15100}]}]'
jq --argjson ingress "$metrics_ingress" '.spec.ingress = $ingress' "$WORK/np.json" > "$WORK/np-metrics.json"
# A changed policy takes effect in the API server a moment after its object is stored: retry for a minute.
for _ in $(seq 30); do
  kubectl --as="$CTL" create --dry-run=server -f "$WORK/np-metrics.json" >/dev/null 2>&1 && break
  sleep 2
done
admitted "the NetworkPolicy with the metrics source" "$WORK/np-metrics.json"
refuse "the metrics source on the admin port only" np '.spec.ingress = [{"from": [{"podSelector": {"matchLabels": {"app": "scrape"}}}], "ports": [{"protocol": "TCP", "port": 80}]}]'
refuse "the metrics source on the admin port only" np '.spec.ingress = [{"from": [{"podSelector": {"matchLabels": {"app": "other"}}}], "ports": [{"protocol": "TCP", "port": 15100}]}]'
refuse "ingress must be absent" np '.spec.ingress = [{"ports": [{"protocol": "TCP", "port": 15100}]}]'

# DELETE (spec §11): the controller may delete the objects that carry the §7.8 labels, in any namespace,
# and no other Job or NetworkPolicy. The cluster administrator's objects are created here, as admin.
log "ldt_17: the controller may delete its own Job and NetworkPolicy, also in a namespace that is not injected; not the Job or NetworkPolicy of another user"
kubectl apply -f - >/dev/null <<YAML
apiVersion: batch/v1
kind: Job
metadata: {name: plain-job, namespace: $LOAD_NS}
spec:
  suspend: true
  template:
    spec:
      restartPolicy: Never
      containers: [{name: plain, image: "$mimic_ref"}]
---
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: plain-np, namespace: $LOAD_NS}
spec: {podSelector: {matchLabels: {app: plain}}}
YAML
for object in job/plain-job networkpolicy/plain-np; do
  if out=$(kubectl --as="$CTL" delete -n "$LOAD_NS" "$object" --dry-run=server 2>&1); then
    fail "ldt_17: the controller may delete $object, which has no load test labels"
  fi
  found -F "metadata labels must be those of spec 7.8" <<<"$out" \
    || fail "ldt_17: the refusal of the delete of $object does not name the labels: $out"
done
jq '.spec.suspend = true' "$WORK/job.json" > "$WORK/job-own.json"
kubectl create -f "$WORK/job-own.json" >/dev/null || fail "ldt_17: the controller's Job was not created by the admin"
kubectl create -f "$WORK/np.json" >/dev/null || fail "ldt_17: the controller's NetworkPolicy was not created by the admin"
jq --arg ns "$PLAIN_NS" '.metadata.namespace = $ns | .metadata.name = "orphan-job"' "$WORK/job-own.json" > "$WORK/orphan.json"
kubectl create -f "$WORK/orphan.json" >/dev/null || fail "ldt_17: the orphan Job was not created by the admin"
kubectl --as="$CTL" delete -n "$LOAD_NS" job/mimic-load-550e8400e29b >/dev/null \
  || fail "ldt_17: the controller could not delete its own Job"
kubectl --as="$CTL" delete -n "$LOAD_NS" networkpolicy/mimic-load-550e8400e29b >/dev/null \
  || fail "ldt_17: the controller could not delete its own NetworkPolicy"
kubectl --as="$CTL" delete -n "$PLAIN_NS" job/orphan-job >/dev/null \
  || fail "ldt_17: the controller could not delete its own Job in a namespace without injection"
kubectl -n "$LOAD_NS" delete job/plain-job networkpolicy/plain-np --wait=true >/dev/null

install || fail "ldt_17: helm install back to the default"

log "e2e-load passed on kubernetes $(kubectl version -o json | jq -r '.serverVersion.gitVersion')"
