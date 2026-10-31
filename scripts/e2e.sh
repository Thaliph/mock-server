#!/usr/bin/env bash
# End-to-end checks on kind (FND-08, FND-09, INJ-11..INJ-17, INJ-23, PRX-14). Idempotent: reuses the cluster
# if it exists; the scenario namespaces are recreated on each run.
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
# Application image for the scenarios: Python with a shell (pinned by digest).
PY=docker.io/library/python:3.13-alpine@sha256:7984cf56ceaddebe4fefeb75d64000b9f45d8b77b11a09b24524da6cdc3af0c8
KEEP_CLUSTER=${KEEP_CLUSTER:-$([[ -n ${CI:-} ]] && echo 0 || echo 1)}
BUILT=0
PIDS=()

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
  # mimic-init tag from the registry.
  helm upgrade --install "$RELEASE" deploy/helm/mimic -n "$NS" \
    --set image.repository="$repo" --set image.tag="$tag" --set image.pullPolicy=Never \
    --set inject.initImage.repository="$init_repo" --set inject.initImage.tag="$init_tag" \
    --wait --timeout 4m
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
curl -fs localhost:18081/metrics | grep -q '^mimic_build_info{component="controller",version="' \
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
printf '%s' "$tls_b64" | base64 -d | openssl x509 -noout -ext subjectAltName \
  | grep -q "DNS:$RELEASE-controller.$NS.svc" || fail "inj_11: serving certificate lacks the service name in its SAN"
openssl verify -CAfile <(printf '%s' "$ca_b64" | base64 -d) <(printf '%s' "$tls_b64" | base64 -d) >/dev/null \
  || fail "inj_11: serving certificate is not signed by the CA in the Secret"
[[ $(kubectl get mutatingwebhookconfiguration $RELEASE-injector -o jsonpath='{.webhooks[0].clientConfig.caBundle}') == "$ca_b64" ]] \
  || fail "inj_11: caBundle is not the CA of the Secret"
admin_forward
curl -fs localhost:18081/metrics | grep -q '^mimic_webhook_certificate_expiry_timestamp_seconds' \
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
  [[ $got == "$want" ]] || fail "inj_12: $desc (want $want, got '$got')"
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
kubectl -n inj-17 logs mimic-init | grep -q '"message":"redirection installed"' \
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

before=$(tcp_count inj-13 inbound ok) || true; before=${before:-0}
kubectl -n inj-13-up exec client -- python3 -c \
  'import urllib.request; urllib.request.urlopen("http://app.inj-13.svc:8080/ready", timeout=10).read()' \
  || fail "inj_13: request from another pod to the app Service failed"
wait_tcp_increase inj-13 inbound "$before" \
  || fail "inj_13: inbound connection not counted as mimic_tcp_connections_total{direction=\"inbound\",result=\"ok\"}"

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
#   Group 1 runs now: the flows complete through the sidecar and TLS shows the destination's own
#     certificate.
#   Group 2 (access lines and RED series) needs F2 steps 5-6 (HTTP relay, access log, RED metrics).
#     It runs only with PRX14_OBSERVABILITY=1; make 1 the default when those steps land.
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
observed=${PRX14_OBSERVABILITY:-0}
webhook=$RELEASE-controller.$NS.svc
if [[ $observed == 1 ]]; then
  # Snapshots before the requests below, so the checks of group 2 see exactly their increments.
  before_in=$(sidecar_series "$pod" mimic_http_requests_total direction=inbound protocol=http1 \
    authority=app.inj-13.svc:8080 status_class=2xx) || true
  before_out=$(sidecar_series "$pod" mimic_http_requests_total direction=outbound protocol=http1 \
    authority=echo.inj-13-up.svc:8080 status_class=2xx) || true
  before_tls=$(sidecar_series "$pod" mimic_proxy_connections_total direction=outbound protocol=tls \
    authority="$webhook") || true
fi
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
PY
) || fail "prx_14: TLS connection to the webhook Service through the sidecar failed"
[[ $got_fp == "$want_fp" ]] || fail "prx_14: TLS through the sidecar does not show the webhook's own certificate"

if [[ $observed == 1 ]]; then
  log "prx_14: access lines and RED series of the same flows (needs F2 steps 5-6)"
  wait_access "$pod" '.direction == "inbound" and .protocol == "http1" and .method == "GET" and .path == "/prx14" and .status == 200 and .result == "ok" and .authority == "app.inj-13.svc:8080"' \
    || fail "prx_14: no inbound http1 access line for GET /prx14"
  wait_access "$pod" '.direction == "outbound" and .protocol == "http1" and .method == "GET" and .path == "/prx14" and .status == 200 and .result == "ok" and .authority == "echo.inj-13-up.svc:8080"' \
    || fail "prx_14: no outbound http1 access line for GET /prx14"
  wait_access "$pod" '.direction == "outbound" and .protocol == "tls" and .sni == "mimic-controller.mimic-system.svc" and .result == "ok"' \
    || fail "prx_14: no outbound tls access line for the webhook Service"
  wait_series "$pod" "$before_in" mimic_http_requests_total direction=inbound protocol=http1 \
    authority=app.inj-13.svc:8080 status_class=2xx \
    || fail "prx_14: mimic_http_requests_total inbound 2xx did not increase"
  wait_series "$pod" "$before_out" mimic_http_requests_total direction=outbound protocol=http1 \
    authority=echo.inj-13-up.svc:8080 status_class=2xx \
    || fail "prx_14: mimic_http_requests_total outbound 2xx did not increase"
  wait_series "$pod" "$before_tls" mimic_proxy_connections_total direction=outbound protocol=tls \
    authority="$webhook" \
    || fail "prx_14: mimic_proxy_connections_total tls did not increase"
else
  log "prx_14: access lines and RED series not checked (needs F2 steps 5-6; PRX14_OBSERVABILITY=1 enables them)"
fi

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

log "inj_23: helm uninstall leaves the CRD and a MimicWorkload; a reinstall with the same release name keeps it"
kubectl apply -f - >/dev/null <<YAML
apiVersion: mimic.platform.io/v1alpha1
kind: MimicWorkload
metadata: {name: kept, namespace: default}
spec: {deployment: api}
YAML
helm uninstall "$RELEASE" -n "$NS" --wait --timeout 4m >/dev/null || fail "inj_23: helm uninstall failed"
kubectl get crd mimicworkloads.mimic.platform.io >/dev/null 2>&1 || fail "inj_23: the CRD was removed by helm uninstall"
[[ $(kubectl -n default get mimicworkload kept -o jsonpath='{.spec.deployment}' 2>/dev/null) == api ]] \
  || fail "inj_23: the MimicWorkload was removed by helm uninstall"
install || fail "inj_23: reinstall with the same release name failed"
kubectl -n "$NS" rollout status deploy/$RELEASE-controller --timeout=2m
[[ $(kubectl -n default get mimicworkload kept -o jsonpath='{.spec.deployment}' 2>/dev/null) == api ]] \
  || fail "inj_23: the MimicWorkload was not kept by the reinstall"
kubectl -n default delete mimicworkload kept --ignore-not-found >/dev/null

log "e2e passed on kubernetes $server"
