#!/usr/bin/env bash
# End-to-end check of the KEDA autoscaling example on kind (ADM-70; spec §10.7 "Autoscaling on the admission contract",
# §12.29). It has a cluster of its own (default name mimic-autoscaling) and shares nothing with scripts/e2e.sh: never run
# the two against the same cluster, nor two e2e scripts at once (fixed ports, one kubeconfig).
#
#   scripts/e2e-autoscaling.sh            the whole of ADM-70 (builds and installs)
#   scripts/e2e-autoscaling.sh --probe    set-up, then only the first check, "no contract": it prints what KEDA does
#                                         when its scaler query is empty (the HPA exists? its conditions? the replicas?)
#
#   TAG=e2e-autoscaling-<sha>-<pid>             tag of the images built here (unique per run)
#   IMAGE=ghcr.io/thaliph/mimic:$TAG            image to test (built here unless SKIP_BUILD=1)
#   INIT_IMAGE=ghcr.io/thaliph/mimic-init:$TAG  mimic-init image (built here unless SKIP_BUILD=1)
#   CLUSTER=mimic-autoscaling                   kind cluster name
#   NODE_IMAGE=kindest/node:v1.36.4@sha256:…    kind node image (default: the kind-node-keda stage of images/Dockerfile, Kubernetes
#                                               1.36; KEDA 2.21 supports 1.34 to 1.36, kind's own default is newer)
#   KEDA_ARCHIVE=<file>                         a local keda chart archive instead of downloading it (scripts/keda-chart.sh checks it)
#   KEEP_CLUSTER=1                              keep the cluster afterwards (default: keep locally, delete in CI)
set -Eeuo pipefail
# A command that fails outside a condition ends the run: name it, so that no exit is silent.
# shellcheck disable=SC2154 # e2e_status is set by the trap's own command
trap 'e2e_status=$?; echo "FAIL line $LINENO: \"$BASH_COMMAND\" exited with status $e2e_status" >&2' ERR
cd "$(dirname "$0")/.."
# shellcheck source=scripts/e2e/lib.sh
source scripts/e2e/lib.sh

PROBE=0
case ${1:-} in
  "") ;;
  --probe) PROBE=1 ;;
  *) echo "usage: $0 [--probe]" >&2; exit 2 ;;
esac
TAG=${TAG:-e2e-autoscaling-$(git rev-parse --short HEAD 2>/dev/null || echo local)-$$}
IMAGE=${IMAGE:-ghcr.io/thaliph/mimic:$TAG}
INIT_IMAGE=${INIT_IMAGE:-ghcr.io/thaliph/mimic-init:$TAG}
CLUSTER=${CLUSTER:-mimic-autoscaling}
NODE_IMAGE=${NODE_IMAGE:-$(scripts/pinned-image.sh kind-node-keda)}
NS=mimic-system
RELEASE=mimic
APP_NS=shop
CLIENT_NS=shop-client
PROM_PORT=19090
PY=$(scripts/pinned-image.sh e2e-app)
KEEP_CLUSTER=${KEEP_CLUSTER:-$([[ -n ${CI:-} ]] && echo 0 || echo 1)}
BUILT=0
PF=""
WORK=$(mktemp -d)

log() { printf '\n== %s\n' "$*"; }
# diag: the state that explains a failure.
diag() {
  kubectl -n "$APP_NS" get scaledobject,hpa,deploy,pods -o wide >&2 || true
  kubectl -n "$APP_NS" get scaledobject cart -o yaml >&2 || true
  kubectl -n keda logs deploy/keda-operator --tail=40 >&2 || true
  kubectl -n keda logs deploy/keda-operator-metrics-apiserver --tail=20 >&2 || true
  kubectl get events -A --sort-by=.lastTimestamp 2>/dev/null | tail -15 >&2 || true
}
fail() { echo "FAIL $*" >&2; diag; exit 1; }
cleanup() {
  [[ -z $PF ]] || kill "$PF" 2>/dev/null || true
  [[ $BUILT == 1 ]] && docker rmi "$IMAGE" "$INIT_IMAGE" >/dev/null 2>&1 || true
  rm -rf "$WORK"
  [[ $KEEP_CLUSTER == 1 ]] || kind delete cluster --name "$CLUSTER"
}
trap cleanup EXIT

# --- set-up ----------------------------------------------------------------------------------------------------
e2e_cluster_up
log "kubernetes $(kubectl version -o json | jq -r '.serverVersion.gitVersion')"
e2e_load_images

# KEDA, from the archive checked against the SHA-256 that scripts/check-autoscaling.sh pins, with keda-values.yaml unchanged.
log "install KEDA from the checked chart archive"
archive=$(scripts/keda-chart.sh "$WORK") || fail "the pinned keda chart is not available"
helm upgrade --install keda "$archive" -n keda --create-namespace -f deploy/autoscaling/keda-values.yaml --wait --timeout 5m \
  || fail "helm install keda"

# mimic, with the bundled Prometheus scraping every 5 s and admitting the KEDA operator (the queryFrom peer of §10.7).
log "install mimic with the bundled Prometheus"
kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl label --overwrite namespace "$NS" pod-security.kubernetes.io/enforce=restricted >/dev/null
cat > "$WORK/mimic-values.yaml" <<YAML
metrics:
  podMonitor:
    interval: 5s
monitoring:
  prometheus:
    enabled: true
    queryFrom:
      - namespaceSelector: {matchLabels: {kubernetes.io/metadata.name: keda}}
        podSelector: {matchLabels: {app: keda-operator}}
YAML
helm upgrade --install "$RELEASE" deploy/helm/mimic -n "$NS" \
  --set image.repository="${IMAGE%:*}" --set image.tag="${IMAGE##*:}" --set image.pullPolicy=Never \
  --set inject.initImage.repository="${INIT_IMAGE%:*}" --set inject.initImage.tag="${INIT_IMAGE##*:}" \
  -f "$WORK/mimic-values.yaml" --wait --timeout 5m || fail "helm install mimic"
kubectl -n "$NS" rollout status deploy/$RELEASE-prometheus --timeout=3m || fail "the bundled Prometheus is not Ready"

# The application: cart (1 replica, injected) and its Service in shop, and a client in a namespace mimic does not inject.
log "deploy cart in $APP_NS and the client in $CLIENT_NS"
for ns in "$APP_NS" "$CLIENT_NS"; do
  kubectl delete namespace "$ns" --ignore-not-found --wait=true --timeout=2m >/dev/null
  kubectl create namespace "$ns" >/dev/null
done
kubectl label namespace "$APP_NS" mimic.platform.io/inject=enabled pod-security.kubernetes.io/enforce=privileged >/dev/null
kubectl -n "$APP_NS" create configmap apps-code --from-file=service.py=scripts/e2e/apps/service.py >/dev/null
kubectl apply -f - >/dev/null <<YAML
apiVersion: apps/v1
kind: Deployment
metadata: {name: cart, namespace: $APP_NS}
spec:
  replicas: 1
  selector: {matchLabels: {app: cart}}
  template:
    metadata: {labels: {app: cart}}
    spec:
      terminationGracePeriodSeconds: 5
      containers:
        - name: app
          image: $PY
          command: [python3, /code/service.py]
          env: [{name: APP_NAME, value: cart}]
          ports: [{name: api, containerPort: 8080}]
          readinessProbe: {tcpSocket: {port: api}, periodSeconds: 1}
          volumeMounts: [{name: code, mountPath: /code, readOnly: true}]
      volumes:
        - {name: code, configMap: {name: apps-code}}
---
apiVersion: v1
kind: Service
metadata: {name: cart, namespace: $APP_NS}
spec:
  selector: {app: cart}
  ports: [{name: api, port: 8080, targetPort: api}]
YAML
kubectl -n "$APP_NS" rollout status deploy/cart --timeout=3m || fail "cart did not roll out"
[[ -n $(kubectl -n "$APP_NS" get pods -l app=cart -o jsonpath='{.items[0].metadata.annotations.mimic\.platform\.io/injected}') ]] \
  || fail "cart is not injected"

# The client: an open model (scripts/e2e/apps/open_client.py, 6 requests/s), a new connection each time.
kubectl -n "$CLIENT_NS" create configmap client-code --from-file=client.py=scripts/e2e/apps/open_client.py >/dev/null
kubectl apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Pod
metadata: {name: client, namespace: $CLIENT_NS}
spec:
  terminationGracePeriodSeconds: 1
  containers:
    - name: client
      image: $PY
      command: [python3, /code/client.py, "http://cart.$APP_NS.svc:8080/users/1", "6"]
      volumeMounts: [{name: code, mountPath: /code, readOnly: true}]
  volumes:
    - {name: code, configMap: {name: client-code}}
YAML
kubectl -n "$CLIENT_NS" wait --for=condition=Ready pod/client --timeout=180s >/dev/null || fail "the client pod is not Ready"

# TLS (ADM-70): the API server verifies the metrics server, with the caBundle that KEDA injected.
log "adm_70 tls: the APIService is Available with a caBundle"
api=v1beta1.external.metrics.k8s.io
apiservice_available() { [[ $(kubectl get apiservice $api -o jsonpath='{.status.conditions[?(@.type=="Available")].status}') == True ]]; }
wait_for 120 apiservice_available \
  || fail "adm_70: the APIService $api is not Available"
[[ -n $(kubectl get apiservice $api -o jsonpath='{.spec.caBundle}') ]] || fail "adm_70: the APIService $api has no caBundle"
[[ $(kubectl get apiservice $api -o jsonpath='{.spec.insecureSkipTLSVerify}') != true ]] || fail "adm_70: the APIService skips TLS verification"
echo "adm_70: $api is Available, caBundle set, TLS verified"

# The ScaledObject of the example, with maxReplicaCount 2 and no scale-down stabilisation (a false 0 would scale down at once).
yq '.spec.maxReplicaCount = 2
  | .spec.advanced.horizontalPodAutoscalerConfig.behavior.scaleDown.stabilizationWindowSeconds = 0' \
  deploy/autoscaling/scaledobject.yaml > "$WORK/scaledobject.yaml"
kubectl apply -f "$WORK/scaledobject.yaml" >/dev/null

# --- helpers ---------------------------------------------------------------------------------------------------
so_json() { kubectl -n "$APP_NS" get scaledobject cart -o json; }
# hpa_name: the HPA that the ScaledObject names in its status, empty until KEDA sets it.
hpa_name() { so_json | jq -r '.status.hpaName // ""'; }
# hpa_active: "<status> <reason>" of the ScalingActive condition of the HPA, empty when the HPA or the condition is absent.
hpa_active() {
  kubectl -n "$APP_NS" get hpa "$1" -o json 2>/dev/null \
    | jq -r '[.status.conditions[]? | select(.type == "ScalingActive")][0] | if . == null then "" else "\(.status) \(.reason)" end'
}
cart_replicas() { kubectl -n "$APP_NS" get deploy cart -o jsonpath='{.spec.replicas}'; }
# observe <label>: one line of what KEDA and the HPA say now.
observe() {
  local hpa so
  so=$(so_json)
  hpa=$(jq -r '.status.hpaName // ""' <<<"$so")
  printf '  [%s] scaledobject conditions: %s | hpaName=%s' "$1" \
    "$(jq -r '[.status.conditions[]? | "\(.type)=\(.status)(\(.reason // ""))"] | join(" ")' <<<"$so")" "${hpa:-none}"
  if [[ -n $hpa ]] && kubectl -n "$APP_NS" get hpa "$hpa" >/dev/null 2>&1; then
    printf ' | hpa: exists, ScalingActive=%s, currentReplicas=%s, cart replicas=%s\n' \
      "$(hpa_active "$hpa" | tr -d '\n')" "$(kubectl -n "$APP_NS" get hpa "$hpa" -o jsonpath='{.status.currentReplicas}')" "$(cart_replicas)"
  else
    printf ' | hpa: absent, cart replicas=%s\n' "$(cart_replicas)"
  fi
}
# --- no contract: does KEDA create the HPA while its scaler query fails? ------------------------------------------
log "adm_70 no contract: KEDA creates the HPA while the scaler query is empty; the HPA holds (60 s)"
echo "  The sidecars expose no mimic_admission_utilization without a contract, so the query returns nothing and"
echo "  ignoreNullValues=false makes it a scaler error. Observing what KEDA does with the HPA:"
has_hpa_name() { [[ -n $(hpa_name) ]]; }
wait_for 120 has_hpa_name || { observe "no hpaName"; fail "adm_70: OBSERVED: KEDA set no status.hpaName within 120 s: it creates no HPA while the scaler fails (design assumption false)"; }
hpa=$(hpa_name)
kubectl -n "$APP_NS" get hpa "$hpa" >/dev/null 2>&1 \
  || { observe "no hpa"; fail "adm_70: OBSERVED: status.hpaName=$hpa but the HPA does not exist (design assumption false)"; }
echo "  OBSERVED: KEDA created the HPA $hpa (status.hpaName set) although the scaler has no value"
kubectl -n "$APP_NS" get hpa "$hpa" -o json | jq -c '.spec.metrics[0] | {type, external: (.external | {metric: .metric.name, target})}'
seen=0
t0=$SECONDS
while (( SECONDS - t0 < 60 )); do
  observe "t+$((SECONDS - t0))s"
  [[ $(cart_replicas) == 1 ]] || fail "adm_70: with no contract cart has $(cart_replicas) replicas, not 1"
  [[ $(hpa_active "$hpa") == "False FailedGetExternalMetric" ]] && seen=1
  sleep 5
done
observe "t+60s"
if [[ $seen == 1 ]]; then
  echo "  OBSERVED: the HPA reports ScalingActive=False FailedGetExternalMetric and cart stays at 1 replica"
else
  fail "adm_70: OBSERVED: ScalingActive was '$(hpa_active "$hpa")', never 'False FailedGetExternalMetric', in 60 s"
fi
[[ $(hpa_active "$hpa") == "False FailedGetExternalMetric" ]] || fail "adm_70: the HPA does not hold FailedGetExternalMetric at the end of the 60 s"
echo "adm_70: no contract: the HPA exists, FailedGetExternalMetric, cart stays at 1 replica"
if [[ $PROBE == 1 ]]; then echo "probe: done (--probe)"; exit 0; fi

# --- scale-up ----------------------------------------------------------------------------------------------------
log "adm_70 scale-up: a contract of 50 concurrent / 5 rps under 6 requests/s gives 2 Ready pods"
kubectl apply -f - >/dev/null <<YAML
apiVersion: mimic.platform.io/v1alpha1
kind: MimicConfig
metadata: {name: cart-admission, namespace: $APP_NS}
spec:
  targetRefs: [{group: apps, kind: Deployment, name: cart}]
  admission: {concurrency: 50, rps: 5}
YAML
t_apply=$SECONDS
metric=$(so_json | jq -r '.status.externalMetricNames[0] // ""')
[[ -n $metric ]] || fail "adm_70: the ScaledObject has no status.externalMetricNames"
raw="/apis/external.metrics.k8s.io/v1beta1/namespaces/$APP_NS/$metric?labelSelector=scaledobject.keda.sh/name=cart"
metric_positive() {
  local out
  out=$(kubectl get --raw "$raw") || return 1
  jq -e '(.items | length) == 1 and (.items[0].value | . != "0" and . != "0m")' <<<"$out" >/dev/null
}
wait_for 60 metric_positive || fail "adm_70: the external metric $metric has no value above 0 within 60 s of the contract: $(kubectl get --raw "$raw" 2>&1)"
echo "  external metric: $(kubectl get --raw "$raw" | jq -c '.items[0] | {metricName, value}')"
two_ready() { [[ $(kubectl -n "$APP_NS" get deploy cart -o jsonpath='{.status.readyReplicas}') == 2 ]]; }
wait_for $((180 - (SECONDS - t_apply))) two_ready || fail "adm_70: cart has no 2 Ready pods within 180 s of the contract"
hpa_up() {
  [[ $(hpa_active "$hpa") == "True "* ]] && [[ $(kubectl -n "$APP_NS" get hpa "$hpa" -o jsonpath='{.status.currentReplicas}') == 2 ]]
}
wait_for 60 hpa_up || fail "adm_70: the HPA is not ScalingActive=True with currentReplicas 2"
observe "scaled up after $((SECONDS - t_apply)) s"
echo "adm_70: scale-up: 2 Ready pods, ScalingActive=True, currentReplicas=2"

# --- rate() variant ----------------------------------------------------------------------------------------------
log "adm_70 rate(): the two queries of the wiki variant return a value above 0 (bundled Prometheus, by port-forward)"
kubectl -n "$NS" port-forward svc/$RELEASE-prometheus "$PROM_PORT":9090 >/dev/null 2>&1 &
PF=$!
sel='job="mimic-sidecar",namespace="shop",deployment="cart"'
q_rps="sum(rate(mimic_admission_requests_total{$sel,result=~\"admitted|overloaded|queue-timeout\"}[2m])) / max(mimic_admission_limit{$sel,kind=\"rps\"})"
q_conc="sum(rate(mimic_admission_in_flight_seconds_total{$sel}[2m])) / max(mimic_admission_limit{$sel,kind=\"concurrency\"})"
prom_positive() {
  curl -fsS -G "http://127.0.0.1:$PROM_PORT/api/v1/query" --data-urlencode "query=$1" \
    | jq -e '.status == "success" and (.data.result | length) > 0 and ((.data.result[0].value[1] | tonumber) > 0)' >/dev/null
}
wait_for 30 curl -fs -o /dev/null "http://127.0.0.1:$PROM_PORT/-/ready" || fail "adm_70: Prometheus is not reachable by port-forward"
wait_for 150 prom_positive "$q_rps" || fail "adm_70: the rate() query on requests has no value above 0"
wait_for 150 prom_positive "$q_conc" || fail "adm_70: the rate() query on in-flight seconds has no value above 0"
echo "adm_70: both rate() queries return a value above 0"

# --- contract removed --------------------------------------------------------------------------------------------
log "adm_70 contract removed: cart keeps 2 replicas for 90 s, the HPA reports FailedGetExternalMetric within 60 s"
kubectl -n "$APP_NS" delete mimicconfig cart-admission >/dev/null
t_del=$SECONDS
failed_at=""
while (( SECONDS - t_del < 90 )); do
  [[ $(cart_replicas) == 2 ]] || fail "adm_70: after the contract was removed cart has $(cart_replicas) replicas, not 2 (a false 0 scaled it down)"
  if [[ -z $failed_at && $(hpa_active "$hpa") == "False FailedGetExternalMetric" ]]; then failed_at=$((SECONDS - t_del)); fi
  sleep 3
done
observe "t+90s"
[[ -n $failed_at ]] || fail "adm_70: the HPA never reported FailedGetExternalMetric after the contract was removed"
(( failed_at <= 60 )) || fail "adm_70: the HPA reported FailedGetExternalMetric only after $failed_at s, not within 60 s"
[[ $(cart_replicas) == 2 ]] || fail "adm_70: cart does not have 2 replicas after 90 s"
echo "adm_70: contract removed: 2 replicas held for 90 s, FailedGetExternalMetric after $failed_at s"

# --- delegated authentication and Secrets -----------------------------------------------------------------------
log "adm_70 delegated authentication: an anonymous call to the metrics server is refused"
code=$(kubectl -n "$CLIENT_NS" exec client -- python3 -c '
import ssl, urllib.error, urllib.request
ctx = ssl.create_default_context()
ctx.check_hostname = False
ctx.verify_mode = ssl.CERT_NONE
try:
    print(urllib.request.urlopen("https://keda-operator-metrics-apiserver.keda.svc/apis/external.metrics.k8s.io/v1beta1", timeout=10, context=ctx).status)
except urllib.error.HTTPError as e:
    print(e.code)
except Exception as e:
    print("error", type(e).__name__)
')
[[ $code == 401 || $code == 403 ]] || fail "adm_70: the anonymous call to the metrics server answered '$code', not 401 or 403"
echo "adm_70: the anonymous call answered $code"

log "adm_70 secrets: the KEDA operator cannot read Secrets in $APP_NS"
answer=$(kubectl auth can-i get secrets -n "$APP_NS" --as system:serviceaccount:keda:keda-operator || true)
[[ $answer == no ]] || fail "adm_70: can-i get secrets -n $APP_NS as keda-operator answered '$answer', not 'no'"
echo "adm_70: can-i get secrets -n $APP_NS as the KEDA operator: no"

# --- ADM-74: the admission policy binds the credential to its address (#562) ------------------------------------
log "adm_74: keda-auth-policy.yaml admits the credential only at its address (--dry-run=server)"
OTHER_NS=other
kubectl delete namespace "$OTHER_NS" --ignore-not-found --wait=true --timeout=2m >/dev/null
kubectl create namespace "$OTHER_NS" >/dev/null
kubectl apply -f - >/dev/null <<YAML
apiVersion: apps/v1
kind: Deployment
metadata: {name: cart, namespace: $OTHER_NS}
spec:
  replicas: 1
  selector: {matchLabels: {app: cart}}
  template:
    metadata: {labels: {app: cart}}
    spec:
      terminationGracePeriodSeconds: 1
      containers: [{name: app, image: $PY, command: [sleep, infinity]}]
YAML
kubectl apply -f deploy/autoscaling/keda-auth-policy.yaml >/dev/null
yq 'select(.kind == "ClusterTriggerAuthentication")' deploy/autoscaling/scaledobject-external.yaml | kubectl apply -f - >/dev/null
yq 'select(.kind == "ScaledObject") | .metadata.namespace = "other"' deploy/autoscaling/scaledobject-external.yaml > "$WORK/ext-so.yaml"
# variant <yq expression>: the external ScaledObject in other, changed by the expression.
variant() { yq -e "$1" "$WORK/ext-so.yaml"; }
# dry <file>: kubectl apply --dry-run=server of the file; sets dry_out and dry_rc (never stops the script).
dry_out=""; dry_rc=0
dry() {
  if dry_out=$(kubectl apply --dry-run=server -f "$1" 2>&1); then dry_rc=0; else dry_rc=1; fi
}
admitted() { # <description> <file>
  dry "$2"
  (( dry_rc == 0 )) || fail "adm_74: $1 was refused: $dry_out"
  echo "  admitted: $1"
}
# no_secret_name <description> <output>: a refusal holds neither the token nor the Secret's name. The policy and its binding are
# named mimic-prometheus-address, so the name of the Secret, mimic-prometheus-auth, appears in no message.
no_secret_name() {
  [[ $2 != *mimic-prometheus-auth* ]] || fail "adm_74: the refusal of $1 holds the Secret's name: $2"
  ! grep -qiE 'token|ca\.crt|secretTargetRef' <<<"$2" || fail "adm_74: the refusal of $1 mentions the token or the Secret: $2"
}
refused() { # <description> <file>
  dry "$2"
  (( dry_rc == 1 )) || fail "adm_74: $1 was admitted"
  [[ $dry_out == *"ValidatingAdmissionPolicy 'mimic-prometheus-address'"* ]] || fail "adm_74: $1 was refused, but not by the policy: $dry_out"
  [[ $dry_out == *"ClusterTriggerAuthentication mimic-prometheus "* ]] || fail "adm_74: the refusal of $1 does not name the ClusterTriggerAuthentication mimic-prometheus: $dry_out"
  no_secret_name "$1" "$dry_out"
  echo "  refused:  $1"
}
other_addr=https://collector.other.svc:443
variant ".spec.triggers[0].metadata.serverAddress = \"$other_addr\"" > "$WORK/r-address.yaml"
variant '.spec.triggers[0].metadata.unsafeSsl = "true"' > "$WORK/r-unsafe.yaml"
variant '.spec.triggers[0].metadata.serverAddressFromEnv = "PROMETHEUS_ADDRESS"' > "$WORK/r-fromenv.yaml"
variant '.spec.triggers[0].type = "loki"' > "$WORK/r-type.yaml"
variant ".spec.triggers[0].authenticationRef.kind = \"clustertriggerauthentication\" | .spec.triggers[0].metadata.serverAddress = \"$other_addr\"" > "$WORK/r-kind.yaml"
variant "del(.spec.triggers[0].authenticationRef) | del(.spec.triggers[0].metadata.authModes) | .spec.triggers[0].metadata.serverAddress = \"$other_addr\"" > "$WORK/a-noauth.yaml"
{
  printf 'apiVersion: keda.sh/v1alpha1\nkind: ScaledJob\nmetadata: {name: job, namespace: %s}\nspec:\n' "$OTHER_NS"
  printf '  jobTargetRef: {template: {spec: {restartPolicy: Never, containers: [{name: c, image: %s}]}}}\n  triggers:\n' "$PY"
  yq '.spec.triggers | del(.[0].metricType) | .[0].metadata.serverAddress = "'"$other_addr"'"' "$WORK/ext-so.yaml" | sed 's/^/  /'
} > "$WORK/r-scaledjob.yaml"
# The policy takes a moment to be enforced: wait until the first refused case is refused (30 s at most).
t0=$SECONDS
until dry "$WORK/r-address.yaml"; (( dry_rc == 1 )) || (( SECONDS - t0 >= 30 )); do sleep 2; done
admitted "the ScaledObject of scaledobject-external.yaml, in other" "$WORK/ext-so.yaml"
admitted "the same with another serverAddress and no authenticationRef" "$WORK/a-noauth.yaml"
refused "serverAddress $other_addr with the credential" "$WORK/r-address.yaml"
refused 'unsafeSsl "true"' "$WORK/r-unsafe.yaml"
refused "a serverAddressFromEnv key" "$WORK/r-fromenv.yaml"
refused "type loki with the credential" "$WORK/r-type.yaml"
# The CRD's own enum refuses a lower-case kind before the policy runs (the policy's lowerAscii is defence in depth, and
# cannot be reached on this CRD): the refusal is by the schema, not by the policy.
dry "$WORK/r-kind.yaml"
(( dry_rc == 1 )) || fail "adm_74: kind clustertriggerauthentication in lower case was admitted"
[[ $dry_out == *"authenticationRef.kind: Unsupported value"* ]] \
  || fail "adm_74: the lower-case kind was refused for another reason: $dry_out"
no_secret_name "the lower-case kind" "$dry_out"
echo "  refused:  kind clustertriggerauthentication in lower case with another serverAddress (by the CRD schema enum, not the policy)"
refused "a ScaledJob using the credential with another serverAddress" "$WORK/r-scaledjob.yaml"
# An update of the live shop/cart (ADM-70) that adds the credential and another serverAddress is refused, and the object is unchanged.
before=$(kubectl -n "$APP_NS" get scaledobject cart -o json | jq -c '.spec')
patch=$(jq -nc --arg a "$other_addr" --argjson t "$(jq -c '.triggers' <<<"$before")" \
  '{spec: {triggers: ($t | .[0].metadata.serverAddress = $a | .[0].authenticationRef = {name: "mimic-prometheus", kind: "ClusterTriggerAuthentication"})}}')
if patch_out=$(kubectl -n "$APP_NS" patch scaledobject cart --type merge -p "$patch" 2>&1); then
  fail "adm_74: the update of the live ScaledObject was admitted"
fi
[[ $patch_out == *"ValidatingAdmissionPolicy 'mimic-prometheus-address'"* && $patch_out == *"ClusterTriggerAuthentication mimic-prometheus "* ]] \
  || fail "adm_74: the update was refused, but not by the policy: $patch_out"
no_secret_name "the update of the live shop/cart" "$patch_out"
[[ $(kubectl -n "$APP_NS" get scaledobject cart -o json | jq -c '.spec') == "$before" ]] || fail "adm_74: the refused update changed the live ScaledObject"
echo "  refused:  the update of the live shop/cart, which is unchanged"
kubectl delete namespace "$OTHER_NS" --wait=false >/dev/null
echo "adm_74: 2 cases admitted, 6 refused by the policy naming the ClusterTriggerAuthentication, none holding the Secret's name, the update refused"

echo "e2e-autoscaling: ADM-70 passed"
