#!/usr/bin/env bash
# Controller measurements of the mesh (M1, docs/design/mimic-mesh.md §14, docs/SPEC.md §7.9, #441). Run by hand on a
# quiet machine; not a CI job. Synthetic objects only, on one kind cluster:
#
#   scale    1000 Services with 10 ready endpoints each (10 namespaces of 100), one injected client per namespace:
#            each view holds the 100 Services of its namespace (1000 endpoints). Once the views are held, the RSS
#            of every controller replica (and the view cache gauge) is sampled for 60 s. Budget: RSS <= 256 MiB.
#   rollout  a namespace of 200 injected sidecars in 4 deployments of 50. The callee deployment (its Service is in
#            every view of the namespace) is restarted. Per sidecar, the views received (pulls with result ok) per
#            second, max and p99, for the sidecars present during the whole rollout (kept) and for the ones started during it
#            (new), and over both; the controller CPU, summed over the replicas. Budgets: <= 1 view/s per kept
#            sidecar (a new pod's first pulls over a few seconds of life are not a sustained rate), <= 0.5 core.
#
#   scripts/bench-controller.sh               both scenarios: the cluster, the images, the chart, then the runs (about 20 minutes, an estimate)
#   scripts/bench-controller.sh --dry-run     the plan only, nothing started
#
# Environment (defaults in brackets):
#   CLUSTER=mimic-ctl-bench        kind cluster: created when absent (with kubelet maxPods 300), reused otherwise
#   SCENARIOS="scale rollout"      the scenarios to run; each is run in this order
#   KEEP_CLUSTER=0                 0 deletes the cluster on exit; 1 keeps it and deletes the namespaces and the release
#   TAG, IMAGE, INIT_IMAGE         the mimic images under test, as in scripts/bench.sh; SKIP_BUILD=1 reuses them
#   NODE_IMAGE=<kind-node stage>   kind node image, pinned by digest (images/Dockerfile)
#   MIMIC_LOCK_DIR=${XDG_RUNTIME_DIR:-/tmp}   lock directories of the runs
#   BENCH_OUT_DIR=docs/bench       result: <date>-controller.json (never overwritten)
#
# Fixed here: kubelet maxPods 300 (kind's default is 110; a node with a pod CIDR of /24 holds about 250 pods, and
# the scenarios stay below that); the sidecars request 5m CPU and 16Mi memory (the scheduler reserves those; the
# measured CPU and RSS are not affected); the workloads are fortio servers with 1m CPU and 16Mi memory requests.
# The namespaces and the cluster are removed on exit, also after a failure.
# Exit: 0 done and every budget met; 1 a step failed, or a budget of design §14 is not met (the JSON is written
# in both cases); 2 usage or invalid configuration; 3 another run holds a lock.
set -Eeuo pipefail
trap 'status=$?; echo "FAIL line $LINENO: \"$BASH_COMMAND\" exited with status $status" >&2' ERR
cd "$(dirname "$0")/.."

usage="usage: scripts/bench-controller.sh [--dry-run]"
DRY_RUN=0
case $# in
  0) ;;
  1) if [[ $1 == --dry-run ]]; then DRY_RUN=1; else echo "$usage" >&2; exit 2; fi ;;
  *) echo "$usage" >&2; exit 2 ;;
esac

CLUSTER=${CLUSTER-mimic-ctl-bench}
SCENARIOS=${SCENARIOS-"scale rollout"}
KEEP_CLUSTER=${KEEP_CLUSTER-0}
SKIP_BUILD=${SKIP_BUILD-0}
OUT_DIR=${BENCH_OUT_DIR-docs/bench}
LOCK_DIR=${MIMIC_LOCK_DIR-${XDG_RUNTIME_DIR:-/tmp}}
TODAY=$(date +%F)
OUT_FILE="$OUT_DIR/$TODAY-controller.json"
SYSTEM_NS=mimic-system
RELEASE=mimic
MAX_PODS=300
SCALE_NS_PREFIX=mimic-ctl-scale
SCALE_NAMESPACES=10
SCALE_SERVICES=100
SCALE_ENDPOINTS=10
SCALE_WINDOW=60
SAMPLE_EVERY=5
ROLLOUT_NS=mimic-ctl-rollout
ROLLOUT_REPLICAS=50
ROLLOUT_CALLERS=(caller-a caller-b caller-c)
RSS_BUDGET=$((256 * 1048576))
VIEW_RATE_BUDGET=1
CPU_BUDGET=0.5
PULL_OK='mimic_mesh_view_pulls_total{result="ok"}'

# config_error <variable> <message>: one line naming the variable; exit 2 before any cluster command.
config_error() {
  echo "FAIL $1: $2" >&2
  exit 2
}

# contains <word> <words...>: 0 when the word is one of the words.
contains() {
  local word=$1 w
  shift
  for w in "$@"; do [[ $w == "$word" ]] && return 0; done
  return 1
}

read -ra SCENARIO_LIST <<<"$SCENARIOS"
((${#SCENARIO_LIST[@]} > 0)) || config_error SCENARIOS "empty"
for scenario in "${SCENARIO_LIST[@]}"; do
  contains "$scenario" scale rollout || config_error SCENARIOS "$scenario is not scale or rollout"
done
# The scenarios run in the order of the list, scale before rollout whatever the order given.
ORDERED_SCENARIOS=()
for scenario in scale rollout; do
  contains "$scenario" "${SCENARIO_LIST[@]}" && ORDERED_SCENARIOS+=("$scenario")
done
[[ $CLUSTER =~ ^[a-z0-9-]+$ ]] || config_error CLUSTER "$CLUSTER does not match ^[a-z0-9-]+\$"
# The e2e clusters (scripts/e2e.sh, and the one an owner keeps for manual tests) are never measured or deleted.
[[ $CLUSTER == mimic-e2e* ]] && config_error CLUSTER "$CLUSTER is an e2e cluster"
[[ $KEEP_CLUSTER == 0 || $KEEP_CLUSTER == 1 ]] || config_error KEEP_CLUSTER "$KEEP_CLUSTER is not 0 or 1"
[[ $SKIP_BUILD == 0 || $SKIP_BUILD == 1 ]] || config_error SKIP_BUILD "$SKIP_BUILD is not 0 or 1"

SCALE_NS_LIST=()
for ((i = 0; i < SCALE_NAMESPACES; i++)); do
  SCALE_NS_LIST+=("$(printf '%s-%02d' "$SCALE_NS_PREFIX" "$i")")
done

# The pinned images: the kind node image and the fortio image, the app of the workloads (scripts/pinned-image.sh).
NODE_IMAGE=${NODE_IMAGE-$(scripts/pinned-image.sh kind-node)}
[[ $NODE_IMAGE =~ @sha256:[0-9a-f]{64}$ ]] || config_error NODE_IMAGE "$NODE_IMAGE is not pinned by digest"
FORTIO=$(scripts/pinned-image.sh fortio)
TAG=${TAG:-ctl-bench-$(git rev-parse --short HEAD 2>/dev/null || echo local)-$$}
IMAGE=${IMAGE:-ghcr.io/thaliph/mimic:$TAG}
INIT_IMAGE=${INIT_IMAGE:-ghcr.io/thaliph/mimic-init:$TAG}

# The result file does not exist yet: a result is never overwritten.
[[ ! -e $OUT_FILE ]] || config_error BENCH_OUT_DIR "$OUT_FILE already exists, not overwritten"

# check_locks: the locks of scripts/e2e.sh, scripts/bench.sh and this script. A held lock refuses the run (exit 3).
lock_state() {
  local dir=$1 pid
  if [[ ! -d $dir ]]; then
    echo free
    return
  fi
  pid=$(cat "$dir/pid" 2>/dev/null || true)
  if [[ $pid =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null; then
    echo "held by pid $pid"
  else
    echo stale
  fi
}
check_locks() {
  local name state
  for name in mimic-e2e mimic-bench mimic-controller-bench; do
    state=$(lock_state "$LOCK_DIR/$name.lock")
    case $state in
      free) echo "lock $LOCK_DIR/$name.lock: free" ;;
      stale) echo "lock $LOCK_DIR/$name.lock: stale, reclaimed by a real run" ;;
      *)
        echo "FAIL $LOCK_DIR/$name.lock is $state: another run is going on, retry when it has finished" >&2
        exit 3
        ;;
    esac
  done
}

if ((DRY_RUN)); then
  echo "plan: dry run, nothing started"
  if [[ $KEEP_CLUSTER == 1 ]]; then
    echo "cluster $CLUSTER kept afterwards (namespaces and release $RELEASE deleted)"
  else
    echo "cluster $CLUSTER deleted on exit"
  fi
  echo "kind node image $NODE_IMAGE, kubelet maxPods $MAX_PODS (checked on the node)"
  echo "sidecars request 5m CPU and 16Mi memory; app: fortio image $FORTIO, 1m CPU and 16Mi memory requests"
  echo "images $IMAGE $INIT_IMAGE (built here unless SKIP_BUILD=1)"
  echo "chart release $RELEASE in $SYSTEM_NS"
  echo "machine fields: cpu cores ram kernel docker kind kubernetes node_image kubelet_max_pods"
  for scenario in "${ORDERED_SCENARIOS[@]}"; do
    if [[ $scenario == scale ]]; then
      echo "scenario scale: ${SCALE_NAMESPACES} namespaces ${SCALE_NS_LIST[0]} .. ${SCALE_NS_LIST[-1]}, each with" \
        "$SCALE_SERVICES Services and $SCALE_ENDPOINTS ready endpoints each, one injected client; the views held," \
        "then the RSS and view cache of each controller replica sampled every ${SAMPLE_EVERY}s for ${SCALE_WINDOW}s"
      echo "  budget: controller RSS <= $((RSS_BUDGET / 1048576)) MiB"
    else
      echo "scenario rollout: namespace $ROLLOUT_NS, 200 injected sidecars in 4 deployments of $ROLLOUT_REPLICAS" \
        "(callee, ${ROLLOUT_CALLERS[*]}); kubectl rollout restart deploy/callee; views per second per sidecar" \
        "and controller CPU during the rollout"
      echo "  budgets: views <= $VIEW_RATE_BUDGET/s per sidecar present during the whole rollout (max), controller CPU <= $CPU_BUDGET cores (sum over replicas)"
    fi
  done
  echo "result $OUT_FILE"
  check_locks
  echo "cleanup on exit: the namespaces ${SCALE_NS_LIST[0]} .. ${SCALE_NS_LIST[-1]} and $ROLLOUT_NS, release $RELEASE (KEEP_CLUSTER=1), the cluster (KEEP_CLUSTER=0)"
  exit 0
fi

# ---- real run ---------------------------------------------------------------------------------------

LOCKED=0
TOUCHED=0
RELEASE_INSTALLED=0
BUILT=0
WORK=""

# cleanup: the namespaces and the release go on exit (KEEP_CLUSTER=1; with 0 the cluster goes with them), the
# images and the lock too, also after a failure.
cleanup() {
  local status=$?
  if [[ $TOUCHED == 1 && $KEEP_CLUSTER == 1 ]]; then
    for ns in "${SCALE_NS_LIST[@]}" "$ROLLOUT_NS"; do
      kubectl delete namespace "$ns" --ignore-not-found --wait=true --timeout=180s >/dev/null 2>&1 \
        || echo "WARN namespace $ns was not deleted" >&2
    done
    if [[ $RELEASE_INSTALLED == 1 ]]; then
      helm uninstall "$RELEASE" -n "$SYSTEM_NS" --wait >/dev/null 2>&1 || echo "WARN release $RELEASE was not uninstalled" >&2
      kubectl delete namespace "$SYSTEM_NS" --ignore-not-found --wait=true --timeout=180s >/dev/null 2>&1 \
        || echo "WARN namespace $SYSTEM_NS was not deleted" >&2
    fi
  fi
  if [[ $BUILT == 1 ]]; then
    docker rmi "$IMAGE" "$INIT_IMAGE" >/dev/null 2>&1 || true
  fi
  if [[ $KEEP_CLUSTER == 0 && $TOUCHED == 1 ]]; then
    kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || echo "WARN cluster $CLUSTER was not deleted" >&2
  fi
  if [[ $LOCKED == 1 ]]; then
    rm -rf "$LOCK_DIR/mimic-controller-bench.lock"
  fi
  if [[ -n $WORK ]]; then
    rm -rf "$WORK"
  fi
  exit "$status"
}
trap cleanup EXIT

check_locks >/dev/null
# Take the lock of this script: mkdir is atomic. A stale lock of a gone process is reclaimed.
ctl_lock="$LOCK_DIR/mimic-controller-bench.lock"
if ! mkdir "$ctl_lock" 2>/dev/null; then
  if [[ $(lock_state "$ctl_lock") != stale ]]; then
    echo "FAIL $ctl_lock is held: another run is going on" >&2
    exit 3
  fi
  rm -rf "$ctl_lock"
  mkdir "$ctl_lock"
fi
echo $$ >"$ctl_lock/pid"
LOCKED=1
WORK=$(mktemp -d)

for tool in kind kubectl helm docker jq awk xargs; do
  command -v "$tool" >/dev/null || { echo "FAIL $tool is not on PATH" >&2; exit 1; }
done

# ---- helpers ----------------------------------------------------------------------------------------

# namespace_manifest <namespace>: privileged (the sidecar's init container needs NET_ADMIN, as in bench.sh), injected.
namespace_manifest() {
  cat <<EOF
apiVersion: v1
kind: Namespace
metadata:
  name: $1
  labels:
    pod-security.kubernetes.io/enforce: privileged
    mimic.platform.io/inject: enabled
EOF
}

# mimicworkload_manifest <namespace> <deployment>: the sidecar of the deployment in mode none.
mimicworkload_manifest() {
  cat <<EOF
---
apiVersion: mimic.platform.io/v1alpha1
kind: MimicWorkload
metadata: {name: $2, namespace: $1}
spec: {deployment: $2, mode: none}
EOF
}

# app_deployment <namespace> <name> <replicas> [service]: a fortio server as the tiny app, with a Service when asked.
app_deployment() {
  local ns=$1 name=$2 replicas=$3
  cat <<EOF
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: $name
  namespace: $ns
  labels: {app: $name}
spec:
  replicas: $replicas
  selector:
    matchLabels: {app: $name}
  template:
    metadata:
      labels: {app: $name}
    spec:
      securityContext:
        runAsNonRoot: true
        runAsUser: 65532
        seccompProfile: {type: RuntimeDefault}
      containers:
      - name: fortio
        image: $FORTIO
        args: ["server", "-http-port", "8080", "-redirect-port", "disabled", "-ui-path", ""]
        ports:
        - {name: http, containerPort: 8080}
        readinessProbe:
          httpGet: {path: /, port: 8080}
        resources:
          requests: {cpu: 1m, memory: 16Mi}
          limits: {memory: 64Mi}
        securityContext:
          allowPrivilegeEscalation: false
          readOnlyRootFilesystem: true
          capabilities: {drop: [ALL]}
EOF
  if [[ ${4-} == service ]]; then
    cat <<EOF
---
apiVersion: v1
kind: Service
metadata:
  name: $name
  namespace: $ns
spec:
  selector: {app: $name}
  ports:
  - {name: http, port: 8080, targetPort: 8080}
EOF
  fi
}

# scale_objects <namespace> <index>: SCALE_SERVICES Services without selector, each with an EndpointSlice of
# SCALE_ENDPOINTS ready addresses (10.200.0.0/16, the addresses are unique across the namespaces).
scale_objects() {
  local ns=$1 index=$2 s e name k
  for ((s = 0; s < SCALE_SERVICES; s++)); do
    name=$(printf 'svc-%03d' "$s")
    printf -- '---\napiVersion: v1\nkind: Service\nmetadata: {name: %s, namespace: %s}\nspec:\n  ports: [{name: http, port: 8080, targetPort: 8080, protocol: TCP}]\n' \
      "$name" "$ns"
    printf -- '---\napiVersion: discovery.k8s.io/v1\nkind: EndpointSlice\nmetadata: {name: %s-1, namespace: %s, labels: {kubernetes.io/service-name: %s}}\naddressType: IPv4\nports: [{name: http, port: 8080, protocol: TCP}]\nendpoints:\n' \
      "$name" "$ns" "$name"
    for ((e = 0; e < SCALE_ENDPOINTS; e++)); do
      k=$((index * SCALE_SERVICES * SCALE_ENDPOINTS + s * SCALE_ENDPOINTS + e + 1))
      printf -- '- addresses: ["10.200.%d.%d"]\n  conditions: {ready: true}\n' $((k / 256)) $((k % 256))
    done
  done
}

# sidecar_pods <namespace>: the running pods of a namespace, not terminating (one name per line).
sidecar_pods() {
  kubectl -n "$1" get pods -o json \
    | jq -r '.items[] | select(.metadata.deletionTimestamp == null and .status.phase == "Running") | .metadata.name'
}

# controller_pods: the running replicas of the controller (one name per line).
controller_pods() {
  kubectl -n "$SYSTEM_NS" get pods -l "app.kubernetes.io/name=mimic-controller,app.kubernetes.io/instance=$RELEASE" -o json \
    | jq -r '.items[] | select(.metadata.deletionTimestamp == null and .status.phase == "Running") | .metadata.name'
}

# pod_value <namespace> <pod> <port> <series>: "<pod> <epoch seconds> <value>" of one series of a pod's /metrics,
# read through the API server (as scripts/bench.sh). Fails when the pod does not answer or has no such series.
pod_value() {
  local ns=$1 pod=$2 port=$3 series=$4 t metrics value
  t=$(date +%s.%N)
  metrics=$(kubectl get --raw "/api/v1/namespaces/$ns/pods/$pod:$port/proxy/metrics" 2>/dev/null) || return 1
  value=$(awk -v m="$series" '$1 == m { print $2; exit }' <<<"$metrics")
  [[ -n $value ]] || return 1
  printf '%s %s %s\n' "$pod" "$t" "$value"
}
export -f pod_value

# sample_all <namespace> <port> <series>: pod_value of each pod named on stdin, 8 at a time.
sample_all() {
  xargs -r -P 8 -I{} bash -c 'pod_value "$@"' _ "$1" {} "$2" "$3"
}

# wait_until <namespace> <count> <series> <min>: until <count> pods of the namespace report <series> >= <min>
# (polled every 3 s, 300 s at most); exit 1 with a FAIL line otherwise.
wait_until() {
  local ns=$1 want=$2 series=$3 min=$4 deadline=$((SECONDS + 300)) got
  while :; do
    got=$(sidecar_pods "$ns" | sample_all "$ns" 15100 "$series" 2>/dev/null | awk -v m="$min" '$3 >= m' | wc -l) || true
    if ((got >= want)); then return 0; fi
    if ((SECONDS >= deadline)); then
      echo "FAIL $got of $want pods of $ns report $series >= $min after 300 s" >&2
      exit 1
    fi
    sleep 3
  done
}

# machine_json: the machine description of the result (no host identity), as scripts/bench.sh.
machine_json() {
  local cpu cores ram kernel docker kind_version kube max
  cpu=$(lscpu)
  cpu=$(sed -n 's/^Model name: *//p' <<<"$cpu" | head -n 1)
  cores=$(nproc)
  ram=$(awk '/^MemTotal:/ { printf "%.1f GiB", $2 / 1048576 }' /proc/meminfo)
  kernel=$(uname -r)
  docker=$(docker version --format '{{.Server.Version}}' 2>/dev/null || echo unknown)
  kind_version=$(kind version)
  kube=$(kubectl version -o json | jq -r '.serverVersion.gitVersion')
  max=$(kubectl get nodes -o jsonpath='{.items[0].status.capacity.pods}')
  jq -n -c --arg cpu "$cpu" --arg ram "$ram" --arg kernel "$kernel" --arg docker "$docker" \
    --arg kind "$kind_version" --arg kube "$kube" --arg node "$NODE_IMAGE" --argjson cores "$cores" \
    --argjson max "$max" \
    '{cpu: $cpu, cores: $cores, ram: $ram, kernel: $kernel, docker: $docker, kind: $kind, kubernetes: $kube, node_image: $node, kubelet_max_pods: $max}'
}

# ---- the cluster, the images and the chart ----------------------------------------------------------

# From here on the run owns the cluster (it is deleted on exit with KEEP_CLUSTER=0, and the namespaces are deleted with 1).
TOUCHED=1
clusters=$(kind get clusters)
if ! grep -qx -- "$CLUSTER" <<<"$clusters"; then
  # kubelet maxPods, raised from kind's 110 for this script only.
  cat >"$WORK/kind.yaml" <<EOF
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
- role: control-plane
  kubeadmConfigPatches:
  - |
    kind: KubeletConfiguration
    maxPods: $MAX_PODS
EOF
  kind create cluster --name "$CLUSTER" --image "$NODE_IMAGE" --config "$WORK/kind.yaml" --wait 120s
fi
kubectl config use-context "kind-$CLUSTER" >/dev/null

node_pods=$(kubectl get nodes -o jsonpath='{.items[0].status.capacity.pods}')
if ((node_pods < MAX_PODS)); then
  echo "FAIL node $CLUSTER allows $node_pods pods, the run needs $MAX_PODS: delete the cluster or use another CLUSTER" >&2
  exit 1
fi

if [[ $SKIP_BUILD != 1 ]]; then
  docker build -q -t "$IMAGE" .
  docker build -q -t "$INIT_IMAGE" --target init .
  BUILT=1
fi
kind load docker-image --name "$CLUSTER" "$IMAGE" "$INIT_IMAGE"

# The chart under test, with the images above. The sidecars' requests are small (the scheduler reserves them for
# 200 sidecars on one node); the limits stay as the chart sets them.
kubectl create namespace "$SYSTEM_NS" --dry-run=client -o yaml | kubectl apply -f -
kubectl label --overwrite namespace "$SYSTEM_NS" \
  pod-security.kubernetes.io/enforce=restricted pod-security.kubernetes.io/warn=restricted
helm upgrade --install "$RELEASE" deploy/helm/mimic -n "$SYSTEM_NS" \
  --set image.repository="${IMAGE%:*}" --set image.tag="${IMAGE##*:}" --set image.pullPolicy=Never \
  --set inject.initImage.repository="${INIT_IMAGE%:*}" --set inject.initImage.tag="${INIT_IMAGE##*:}" \
  --set inject.sidecarResources.requests.cpu=5m --set inject.sidecarResources.requests.memory=16Mi \
  --wait --timeout 4m
RELEASE_INSTALLED=1

MACHINE=$(machine_json)

# ---- scale --------------------------------------------------------------------------------------------

# scale_phase: the objects, one client per namespace, the views held; the RSS and view cache sampled after.
scale_phase() {
  local i ns pod svc ep samples
  echo "== scale: $SCALE_NAMESPACES namespaces of $SCALE_SERVICES Services, $SCALE_ENDPOINTS endpoints each"
  for ((i = 0; i < SCALE_NAMESPACES; i++)); do
    ns=${SCALE_NS_LIST[i]}
    { namespace_manifest "$ns"; mimicworkload_manifest "$ns" client; } | kubectl apply -f - >/dev/null
    scale_objects "$ns" "$i" | kubectl apply -f - >/dev/null
  done
  for ns in "${SCALE_NS_LIST[@]}"; do
    app_deployment "$ns" client 1 | kubectl apply -f - >/dev/null
  done
  for ns in "${SCALE_NS_LIST[@]}"; do
    kubectl -n "$ns" rollout status deploy/client --timeout=180s >/dev/null
  done
  # Each client holds its namespace's Services: wait until the view has all the endpoints.
  for ns in "${SCALE_NS_LIST[@]}"; do
    wait_until "$ns" 1 'mimic_mesh_view_endpoints' $((SCALE_SERVICES * SCALE_ENDPOINTS))
  done
  : >"$WORK/scale-views.txt"
  for ns in "${SCALE_NS_LIST[@]}"; do
    pod=$(sidecar_pods "$ns" | head -n 1)
    svc=$(pod_value "$ns" "$pod" 15100 'mimic_mesh_view_services' | awk '{ print $3 }')
    ep=$(pod_value "$ns" "$pod" 15100 'mimic_mesh_view_endpoints' | awk '{ print $3 }')
    printf '%s %s %s\n' "$ns" "$svc" "$ep" >>"$WORK/scale-views.txt"
  done
  # The RSS and the view cache of every replica, every SAMPLE_EVERY seconds over SCALE_WINDOW seconds.
  : >"$WORK/scale-rss.txt"
  : >"$WORK/scale-cache.txt"
  samples=$((SCALE_WINDOW / SAMPLE_EVERY))
  for ((i = 0; i <= samples; i++)); do
    controller_pods | sample_all "$SYSTEM_NS" 8081 process_resident_memory_bytes >>"$WORK/scale-rss.txt"
    controller_pods | sample_all "$SYSTEM_NS" 8081 mimic_mesh_view_cache_bytes >>"$WORK/scale-cache.txt"
    if ((i < samples)); then sleep "$SAMPLE_EVERY"; fi
  done

  jq -n -c \
    --rawfile rss "$WORK/scale-rss.txt" --rawfile cache "$WORK/scale-cache.txt" --rawfile views "$WORK/scale-views.txt" \
    --argjson budget "$RSS_BUDGET" --argjson namespaces "$SCALE_NAMESPACES" --argjson services "$SCALE_SERVICES" \
    --argjson endpoints "$SCALE_ENDPOINTS" --argjson window "$SCALE_WINDOW" --argjson every "$SAMPLE_EVERY" '
    def rows: split("\n") | map(select(length > 0) | split(" ") | {pod: .[0], t: (.[1] | tonumber), v: (.[2] | tonumber)});
    def mib: (. / 1048576 * 10 | round) / 10;
    ($rss | rows) as $r
    | ($cache | rows) as $c
    | ($views | split("\n") | map(select(length > 0) | split(" ") | {namespace: .[0], services: (.[1] | tonumber), endpoints: (.[2] | tonumber)})) as $v
    | ($r | map(.v) | max) as $max_rss
    | {
        namespaces: $namespaces, services: ($namespaces * $services), endpoints: ($namespaces * $services * $endpoints),
        views: {clients: ($v | length), services_min: ($v | map(.services) | min), endpoints_min: ($v | map(.endpoints) | min)},
        window_s: $window, sample_every_s: $every,
        replicas: ([$r[].pod] | unique | map(. as $p | {
          pod: $p,
          rss_max_mib: ([$r[] | select(.pod == $p) | .v] | max | mib),
          view_cache_max_mib: ([$c[] | select(.pod == $p) | .v] | max | mib)
        })),
        rss_max_mib: ($max_rss | mib),
        view_cache_max_mib: ($c | map(.v) | max | mib),
        budgets: [{name: "controller RSS, 1000 Services and 10000 endpoints, view cache included",
                   value: ($max_rss / 1048576 * 10 | round / 10), unit: "MiB",
                   limit: ($budget / 1048576), met: ($max_rss <= $budget)}]
      }' >"$WORK/scale.json"
}

# ---- rollout ------------------------------------------------------------------------------------------

# rollout_phase: 200 sidecars in 4 deployments; the callee restarted; per sidecar the views received, per controller
# replica the CPU, over the rollout.
rollout_phase() {
  local ns=$ROLLOUT_NS d total=$((ROLLOUT_REPLICAS * (1 + ${#ROLLOUT_CALLERS[@]})))
  echo "== rollout: namespace $ns, $total sidecars"
  {
    namespace_manifest "$ns"
    mimicworkload_manifest "$ns" callee
    for d in "${ROLLOUT_CALLERS[@]}"; do mimicworkload_manifest "$ns" "$d"; done
  } | kubectl apply -f - >/dev/null
  app_deployment "$ns" callee "$ROLLOUT_REPLICAS" service | kubectl apply -f - >/dev/null
  for d in "${ROLLOUT_CALLERS[@]}"; do
    app_deployment "$ns" "$d" "$ROLLOUT_REPLICAS" | kubectl apply -f - >/dev/null
  done
  for d in callee "${ROLLOUT_CALLERS[@]}"; do
    kubectl -n "$ns" rollout status "deploy/$d" --timeout=600s >/dev/null
  done
  local injected
  injected=$(kubectl -n "$ns" get pods -o json \
    | jq '[.items[] | select(.metadata.deletionTimestamp == null and any(.spec.initContainers[]?; .name == "mimic-sidecar"))] | length')
  ((injected == total)) || { echo "FAIL $injected of $total pods of $ns have a mimic-sidecar" >&2; exit 1; }
  # Steady state: every sidecar holds its view before the measure starts.
  wait_until "$ns" "$total" "$PULL_OK" 1

  sidecar_pods "$ns" | sample_all "$ns" 15100 "$PULL_OK" >"$WORK/rollout-s0.txt"
  controller_pods | sample_all "$SYSTEM_NS" 8081 process_cpu_seconds_total >"$WORK/rollout-cpu0.txt"
  local t_restart t_end
  t_restart=$(date +%s.%N)
  kubectl -n "$ns" rollout restart deploy/callee >/dev/null
  kubectl -n "$ns" rollout status deploy/callee --timeout=600s >/dev/null
  t_end=$(date +%s.%N)
  controller_pods | sample_all "$SYSTEM_NS" 8081 process_cpu_seconds_total >"$WORK/rollout-cpu1.txt"
  sidecar_pods "$ns" | sample_all "$ns" 15100 "$PULL_OK" >"$WORK/rollout-s1.txt"
  kubectl -n "$ns" get pods -o json >"$WORK/rollout-pods.json"

  # Per sidecar: a pod of the first sample is measured over its two reads; a pod started during the rollout
  # counts its pulls since its sidecar started (its counter starts at 0). Restarted sidecars (a counter that went
  # down) are counted and left out.
  jq -n -c \
    --rawfile s0 "$WORK/rollout-s0.txt" --rawfile s1 "$WORK/rollout-s1.txt" \
    --rawfile c0 "$WORK/rollout-cpu0.txt" --rawfile c1 "$WORK/rollout-cpu1.txt" \
    --slurpfile pods "$WORK/rollout-pods.json" \
    --arg ns "$ns" --argjson replicas "$ROLLOUT_REPLICAS" --argjson total "$total" \
    --argjson rollout_s "$(awk -v a="$t_restart" -v b="$t_end" 'BEGIN { printf "%.3f", b - a }')" \
    --argjson view_budget "$VIEW_RATE_BUDGET" --argjson cpu_budget "$CPU_BUDGET" '
    def rows: split("\n") | map(select(length > 0) | split(" ") | {pod: .[0], t: (.[1] | tonumber), v: (.[2] | tonumber)});
    def epoch: sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601;
    def starts: [$pods[0].items[] | select(.metadata.deletionTimestamp == null)
      | {key: .metadata.name,
         value: ((((.status.initContainerStatuses // []) | map(select(.name == "mimic-sidecar") | .state.running.startedAt) | first)
                  // .metadata.creationTimestamp) | epoch)}] | from_entries;
    ($s0 | rows | map({key: .pod, value: .}) | from_entries) as $before
    | starts as $start
    | [($s1 | rows)[] as $e
        | if $before[$e.pod] then
            {pod: $e.pod, kind: "kept", views: ($e.v - $before[$e.pod].v), window_s: ($e.t - $before[$e.pod].t)}
          else
            {pod: $e.pod, kind: "new", views: $e.v, window_s: ($e.t - $start[$e.pod])}
          end] as $all
    | def rates(k): [$all[] | select(.views >= 0 and .window_s > 0 and (k == "all" or .kind == k)) | .views / .window_s] | sort;
      def stats(k): rates(k) as $r | ($r | length) as $n
        | if $n == 0 then {sidecars: 0, max: 0, p99: 0, mean: 0}
          else {sidecars: $n, max: ($r[-1] * 1000 | round / 1000), p99: ($r[(($n * 0.99 | ceil) - 1)] * 1000 | round / 1000),
                mean: (($r | add) / $n * 1000 | round / 1000)} end;
      rates("all") as $rates
    | rates("kept") as $kept
    | ($rates | length) as $n
    | ($c0 | rows | map({key: .pod, value: .}) | from_entries) as $cb
    | [($c1 | rows)[] as $e
        | ($cb[$e.pod]) as $b
        | {pod: $e.pod, cores: (($e.v - $b.v) / ($e.t - $b.t)), reset: ($e.v < $b.v)}] as $cpu
    | ($cpu | map(.cores) | add) as $cores
    | {
        namespace: $ns, deployment: "callee", replicas: $replicas, sidecars: $total,
        rollout_s: ($rollout_s * 1000 | round / 1000),
        pulls: {sidecars_measured: $n, kept: ($all | map(select(.kind == "kept")) | length),
                new: ($all | map(select(.kind == "new")) | length),
                restarted: ($all | map(select(.views < 0)) | length)},
        views_per_s: (stats("all") + {kept: stats("kept"), new: stats("new")}),
        controller: {replicas: ($cpu | length), cpu_cores: ($cores * 1000 | round / 1000),
                     resets: ($cpu | map(select(.reset)) | length),
                     per_replica: ($cpu | map({pod, cores: (.cores * 1000 | round / 1000)}))},
        budgets: [
          {name: "views per second per sidecar present during the whole rollout (max)",
           value: (stats("kept").max), unit: "views/s", limit: $view_budget, met: (stats("kept").max <= $view_budget)},
          {name: "controller CPU, sum over replicas", value: ($cores * 1000 | round / 1000), unit: "cores",
           limit: $cpu_budget, met: ($cores <= $cpu_budget)}]
      }' >"$WORK/rollout.json"
  if [[ $(jq -r '.controller.resets' "$WORK/rollout.json") != 0 ]]; then
    echo "FAIL a controller replica restarted during the rollout: the CPU of the run is not valid" >&2
    exit 1
  fi
}

for scenario in "${ORDERED_SCENARIOS[@]}"; do
  if [[ $scenario == scale ]]; then scale_phase; else rollout_phase; fi
done

# ---- result -----------------------------------------------------------------------------------------

# A scenario not run is null in the file.
[[ -e $WORK/scale.json ]] || echo null >"$WORK/scale.json"
[[ -e $WORK/rollout.json ]] || echo null >"$WORK/rollout.json"
jq -n --arg date "$TODAY" --argjson machine "$MACHINE" \
  --slurpfile scale "$WORK/scale.json" --slurpfile rollout "$WORK/rollout.json" \
  '{date: $date, machine: $machine, scale: $scale[0], rollout: $rollout[0]}' >"$OUT_FILE"
echo "wrote $OUT_FILE"

jq -r '
  (.scale // empty | "scale: controller RSS max \(.rss_max_mib) MiB (view cache max \(.view_cache_max_mib) MiB), views held \(.views.clients) clients, \(.views.endpoints_min)+ endpoints each"),
  (.rollout // empty | "rollout: \(.sidecars) sidecars, \(.rollout_s) s; views per second per sidecar max \(.views_per_s.max) p99 \(.views_per_s.p99) (kept: max \(.views_per_s.kept.max) p99 \(.views_per_s.kept.p99), new: max \(.views_per_s.new.max)); controller CPU \(.controller.cpu_cores) cores")
' "$OUT_FILE"
if ! jq -e '[.. | objects | select(has("met")) | .met] | all' "$OUT_FILE" >/dev/null; then
  jq -r '.. | objects | select(has("met") and .met == false)
    | "FAIL budget \(.name): \(.value) \(.unit), limit \(.limit) \(.unit)"' "$OUT_FILE" >&2
  exit 1
fi
echo "done: every budget met"
