#!/usr/bin/env bash
# Baseline benchmarks of the sidecar (BEN-01 to BEN-10, docs/SPEC.md §12.19, #436) and of the mesh
# (BEN-11 to BEN-16, #444; mesh-refuse, RTY-23, #445). Run by hand on a quiet machine; not a CI job.
#
#   scripts/bench.sh               the default matrix: 18 runs of 30 s with 10 s pauses, about 12 minutes, plus the cluster
#   scripts/bench.sh --dry-run     the plan only, nothing started (scripts/test_bench.sh reads it)
#   SETUPS="mesh-lb mesh-churn mesh-refuse" scripts/bench.sh   the mesh setups (3 backend pods, routing asserted)
#
# Environment (defaults in brackets):
#   CLUSTER=mimic-bench            kind cluster: created when absent, reused otherwise
#   SETUPS="plain none discovery"  plain: no sidecar; none: the sidecar in mode none (proxy only, M0);
#                                  discovery: the sidecar in discovery mode; mesh-lb: discovery, the
#                                  backend has 3 pods and the mesh routing of the client is asserted
#                                  before and after the runs, then the EndpointSlice propagation and the
#                                  RSS with a synthetic view are measured (#444); mesh-churn: mesh-lb
#                                  with a rollout restart of the backend during each run; mesh-refuse:
#                                  mesh-lb with 2 injected backend pods and 1 Ready pod that is not
#                                  injected and refuses connections (RTY-23, #445)
#   PROPAGATION_SAMPLES=20         mesh-lb only: EndpointSlice changes timed, 0 to 100; 0 skips them
#   PROTOCOLS="http1 h2c"          HTTP/1.1 keep-alive; h2c (HTTP/2 over cleartext)
#   RATES="50 100 200"             requests per second, each 1 to 200
#   DURATION=30s                   one fortio run, 1 to 300 s
#   CONNECTIONS=8                  fortio connections, 1 to 64
#   PAUSE=10                       seconds between two runs, 0 to 600
#   KEEP_CLUSTER=1                 0 deletes the cluster on exit
#   TAG, IMAGE, INIT_IMAGE         the mimic images under test, as in scripts/e2e.sh; SKIP_BUILD=1 reuses them
#   NODE_IMAGE=<kind-node stage>   kind node image, pinned by digest (images/Dockerfile)
#   MIMIC_LOCK_DIR=${XDG_RUNTIME_DIR:-/tmp}   lock directories of the runs (BEN-06)
#   BENCH_OUT_DIR=docs/bench       results: <date>-<setup>.json and rows of README.md (BEN-07)
#
# The namespace mimic-bench is deleted on exit, also after a failure (BEN-08).
# Exit: 0 done; 1 a step failed; 2 usage or invalid configuration; 3 another run holds a lock.
set -Eeuo pipefail
trap 'status=$?; echo "FAIL line $LINENO: \"$BASH_COMMAND\" exited with status $status" >&2' ERR
cd "$(dirname "$0")/.."

usage="usage: scripts/bench.sh [--dry-run]"
DRY_RUN=0
case $# in
  0) ;;
  1) if [[ $1 == --dry-run ]]; then DRY_RUN=1; else echo "$usage" >&2; exit 2; fi ;;
  *) echo "$usage" >&2; exit 2 ;;
esac

CLUSTER=${CLUSTER-mimic-bench}
SETUPS=${SETUPS-"plain none discovery"}
PROTOCOLS=${PROTOCOLS-"http1 h2c"}
RATES=${RATES-"50 100 200"}
DURATION=${DURATION-30s}
CONNECTIONS=${CONNECTIONS-8}
PAUSE=${PAUSE-10}
PROPAGATION_SAMPLES=${PROPAGATION_SAMPLES-20}
KEEP_CLUSTER=${KEEP_CLUSTER-1}
OUT_DIR=${BENCH_OUT_DIR-docs/bench}
LOCK_DIR=${MIMIC_LOCK_DIR-${XDG_RUNTIME_DIR:-/tmp}}
NS=mimic-bench
SYSTEM_NS=mimic-system
RELEASE=mimic
RATE_CAP=200
MESH_PODS=3                 # the backend of the mesh setups (BEN-11)
REFUSING_PODS=1             # mesh-refuse: of the MESH_PODS backend pods, this many are not injected and refuse (RTY-23)
PROPAGATION_BUDGET_MS=2000  # EndpointSlice change to the sidecar's view applied, p99 (design §14, M1)
SYNTHETIC_SERVICES=100      # the synthetic view of the RSS measurement (BEN-15)
SYNTHETIC_ENDPOINTS=10      # endpoints of each synthetic Service
RSS_BUDGET_MIB=16           # the sidecar RSS may grow by this much over its baseline (design §14, M1)
TODAY=$(date +%F)

# config_error <variable> <message>: BEN-02, one line naming the variable; exit 2 before any cluster command.
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

read -ra SETUP_LIST <<<"$SETUPS"
read -ra PROTOCOL_LIST <<<"$PROTOCOLS"
read -ra RATE_LIST <<<"$RATES"
((${#SETUP_LIST[@]} > 0)) || config_error SETUPS "empty"
((${#PROTOCOL_LIST[@]} > 0)) || config_error PROTOCOLS "empty"
((${#RATE_LIST[@]} > 0)) || config_error RATES "empty"
for setup in "${SETUP_LIST[@]}"; do
  contains "$setup" plain none discovery mesh-lb mesh-churn mesh-refuse \
    || config_error SETUPS "$setup is not plain, none, discovery, mesh-lb, mesh-churn or mesh-refuse"
done
for protocol in "${PROTOCOL_LIST[@]}"; do
  contains "$protocol" http1 h2c || config_error PROTOCOLS "$protocol is not http1 or h2c"
done
for rate in "${RATE_LIST[@]}"; do
  # At most 3 digits before any arithmetic: a longer number would overflow bash integers past the cap.
  [[ $rate =~ ^[1-9][0-9]{0,2}$ ]] || config_error RATES "$rate is not an integer from 1 to $RATE_CAP"
  ((rate <= RATE_CAP)) || config_error RATES "$rate rps is above the cap of $RATE_CAP rps"
done
[[ $DURATION =~ ^[1-9][0-9]{0,2}s$ ]] || config_error DURATION "$DURATION is not <n>s with 1 <= n <= 300"
DURATION_S=${DURATION%s}
((DURATION_S <= 300)) || config_error DURATION "$DURATION is above 300 s"
if ! [[ $CONNECTIONS =~ ^[1-9][0-9]?$ ]] || ((CONNECTIONS > 64)); then
  config_error CONNECTIONS "$CONNECTIONS is not 1 to 64"
fi
if ! [[ $PAUSE =~ ^(0|[1-9][0-9]{0,2})$ ]] || ((PAUSE > 600)); then
  config_error PAUSE "$PAUSE is not 0 to 600"
fi
[[ $PROPAGATION_SAMPLES =~ ^(0|[1-9][0-9]?|100)$ ]] \
  || config_error PROPAGATION_SAMPLES "$PROPAGATION_SAMPLES is not 0 to 100"
[[ $CLUSTER =~ ^[a-z0-9-]+$ ]] || config_error CLUSTER "$CLUSTER does not match ^[a-z0-9-]+\$"
# The e2e clusters (scripts/e2e.sh, and the one an owner keeps for manual tests) are never benchmarked or deleted.
[[ $CLUSTER == mimic-e2e* ]] && config_error CLUSTER "$CLUSTER is an e2e cluster"
[[ $KEEP_CLUSTER == 0 || $KEEP_CLUSTER == 1 ]] || config_error KEEP_CLUSTER "$KEEP_CLUSTER is not 0 or 1"

# The pinned images: the kind node image and the fortio image (BEN-05, scripts/pinned-image.sh).
NODE_IMAGE=${NODE_IMAGE-$(scripts/pinned-image.sh kind-node)}
[[ $NODE_IMAGE =~ @sha256:[0-9a-f]{64}$ ]] || config_error NODE_IMAGE "$NODE_IMAGE is not pinned by digest"
FORTIO=$(scripts/pinned-image.sh fortio)
# The refusing pod of mesh-refuse needs a shell-less sleep: the Python image of the e2e stages (BEN-05).
REFUSING_IMAGE=$(scripts/pinned-image.sh e2e-app)
TAG=${TAG:-bench-$(git rev-parse --short HEAD 2>/dev/null || echo local)-$$}
IMAGE=${IMAGE:-ghcr.io/thaliph/mimic:$TAG}
INIT_IMAGE=${INIT_IMAGE:-ghcr.io/thaliph/mimic-init:$TAG}
SKIP_BUILD=${SKIP_BUILD-0}

# The setups run in this order whatever the order of SETUPS: plain first, then the namespace is labelled,
# the mesh setups last (they scale the backend to 3 pods, and the others need 1). mesh-refuse is the last one:
# its refusing pod stays in the namespace, and the setups before it must not see it.
ORDERED_SETUPS=()
for setup in plain none discovery mesh-lb mesh-churn mesh-refuse; do
  contains "$setup" "${SETUP_LIST[@]}" && ORDERED_SETUPS+=("$setup")
done

# The output files of a run do not exist yet (BEN-02): a result is never overwritten.
for setup in "${ORDERED_SETUPS[@]}"; do
  file="$OUT_DIR/$TODAY-$setup.json"
  [[ ! -e $file ]] || config_error BENCH_OUT_DIR "$file already exists, not overwritten"
done

# fortio_args <protocol> <rate>: the fortio load command of one run, in FORTIO_ARGS (BEN-03).
fortio_args() {
  FORTIO_ARGS=(load -qps "$2" -t "$DURATION" -c "$CONNECTIONS" -p "50,90,99,99.9" -json -)
  if [[ $1 == h2c ]]; then FORTIO_ARGS+=(-h2); fi
  FORTIO_ARGS+=("http://fortio-server:8080/")
}

# lock_state <dir>: "free", "stale" (the recorded process is gone, or no pid), or "held by pid N" (BEN-06).
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

# check_locks: the lock of scripts/e2e.sh and the lock of this script. A held lock refuses the run (exit 3).
check_locks() {
  local name state
  for name in mimic-e2e mimic-bench; do
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
    echo "cluster $CLUSTER kept afterwards"
  else
    echo "cluster $CLUSTER deleted on exit"
  fi
  echo "namespace $NS, chart release $RELEASE in $SYSTEM_NS"
  echo "kind node image $NODE_IMAGE"
  echo "fortio image $FORTIO"
  echo "refusing pod image $REFUSING_IMAGE"
  echo "images $IMAGE $INIT_IMAGE (built here unless SKIP_BUILD=1)"
  echo "machine fields: cpu cores ram kernel docker kind kubernetes node_image"
  for setup in "${ORDERED_SETUPS[@]}"; do
    echo "results $OUT_DIR/$TODAY-$setup.json"
  done
  echo "rows appended to $OUT_DIR/README.md"
  for setup in "${ORDERED_SETUPS[@]}"; do
    case $setup in
      mesh-lb) echo "mesh-lb: backend $MESH_PODS pods, mode discovery; routing asserted before and after the runs" ;;
      mesh-churn) echo "mesh-churn: backend $MESH_PODS pods, mode discovery; rollout restart of the backend in every run; routing asserted before and after the runs" ;;
      mesh-refuse) echo "mesh-refuse: backend $MESH_PODS pods, $((MESH_PODS - REFUSING_PODS)) injected and $REFUSING_PODS not injected and refusing connections, mode discovery; routing asserted before and after the runs; retries and errors recorded" ;;
    esac
  done
  if contains mesh-lb "${ORDERED_SETUPS[@]}"; then
    echo "mesh-lb propagation: $PROPAGATION_SAMPLES EndpointSlice changes, budget p99 $PROPAGATION_BUDGET_MS ms"
    echo "mesh-lb synthetic view: $SYNTHETIC_SERVICES Services, $SYNTHETIC_ENDPOINTS endpoints each, RSS budget baseline + $RSS_BUDGET_MIB MiB"
  fi
  check_locks
  total=$((${#ORDERED_SETUPS[@]} * ${#PROTOCOL_LIST[@]} * ${#RATE_LIST[@]}))
  index=0
  for setup in "${ORDERED_SETUPS[@]}"; do
    for protocol in "${PROTOCOL_LIST[@]}"; do
      for rate in "${RATE_LIST[@]}"; do
        index=$((index + 1))
        fortio_args "$protocol" "$rate"
        echo "run $index/$total setup $setup protocol $protocol rate $rate rps: fortio ${FORTIO_ARGS[*]}"
        if ((index < total)); then echo "pause ${PAUSE}s"; fi
      done
    done
  done
  echo "cleanup on exit: kubectl delete namespace $NS"
  exit 0
fi

# ---- real run ---------------------------------------------------------------------------------------

LOCKED=0
NS_CREATED=0
BUILT=0
WORK=""

# cleanup: the namespace of the run goes on exit, also after a failure (BEN-08); the lock is released.
cleanup() {
  local status=$?
  if [[ $NS_CREATED == 1 ]]; then
    kubectl delete namespace "$NS" --ignore-not-found --wait=true --timeout=120s >/dev/null 2>&1 \
      || echo "WARN namespace $NS was not deleted" >&2
  fi
  if [[ $BUILT == 1 ]]; then
    docker rmi "$IMAGE" "$INIT_IMAGE" >/dev/null 2>&1 || true
  fi
  if [[ $KEEP_CLUSTER == 0 ]]; then
    kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
  fi
  if [[ $LOCKED == 1 ]]; then
    rm -rf "$LOCK_DIR/mimic-bench.lock"
  fi
  if [[ -n $WORK ]]; then
    rm -rf "$WORK"
  fi
  exit "$status"
}
trap cleanup EXIT

check_locks >/dev/null
# Take the lock of this script: mkdir is atomic. A stale lock of a gone process is reclaimed.
bench_lock="$LOCK_DIR/mimic-bench.lock"
if ! mkdir "$bench_lock" 2>/dev/null; then
  if [[ $(lock_state "$bench_lock") != stale ]]; then
    echo "FAIL $bench_lock is held: another run is going on" >&2
    exit 3
  fi
  rm -rf "$bench_lock"
  mkdir "$bench_lock"
fi
echo $$ >"$bench_lock/pid"
LOCKED=1
WORK=$(mktemp -d)

for tool in kind kubectl helm docker jq awk; do
  command -v "$tool" >/dev/null || { echo "FAIL $tool is not on PATH" >&2; exit 1; }
done

# ensure_cluster: the kind cluster, created with the pinned node image when absent (BEN-04).
clusters=$(kind get clusters)
if ! grep -qx -- "$CLUSTER" <<<"$clusters"; then
  kind create cluster --name "$CLUSTER" --image "$NODE_IMAGE" --wait 120s
fi
kubectl config use-context "kind-$CLUSTER" >/dev/null

if [[ $SKIP_BUILD != 1 ]]; then
  docker build -q -t "$IMAGE" .
  docker build -q -t "$INIT_IMAGE" --target init .
  BUILT=1
fi
kind load docker-image --name "$CLUSTER" "$IMAGE" "$INIT_IMAGE"

# The chart under test, installed with the images above and the Pod Security restricted label, as e2e.sh.
kubectl create namespace "$SYSTEM_NS" --dry-run=client -o yaml | kubectl apply -f -
kubectl label --overwrite namespace "$SYSTEM_NS" \
  pod-security.kubernetes.io/enforce=restricted pod-security.kubernetes.io/warn=restricted
helm upgrade --install "$RELEASE" deploy/helm/mimic -n "$SYSTEM_NS" \
  --set image.repository="${IMAGE%:*}" --set image.tag="${IMAGE##*:}" --set image.pullPolicy=Never \
  --set inject.initImage.repository="${INIT_IMAGE%:*}" --set inject.initImage.tag="${INIT_IMAGE##*:}" \
  --wait --timeout 4m

# The bench namespace: privileged, as INJ-13 needs for the sidecar's NET_ADMIN init container. Labelled for
# discovery later; labelling does not restart pods, so the workloads are restarted by prepare_setup.
NS_CREATED=1
kubectl apply -f - <<EOF
apiVersion: v1
kind: Namespace
metadata:
  name: $NS
  labels:
    pod-security.kubernetes.io/enforce: privileged
EOF

# workload <name> <args-json>: a fortio Deployment of the pinned image, non-root, read-only root filesystem.
workload() {
  local name=$1 args=$2 port=$3
  cat <<EOF
apiVersion: apps/v1
kind: Deployment
metadata:
  name: $name
  namespace: $NS
  labels: {app: $name}
spec:
  replicas: 1
  selector:
    matchLabels: {app: $name}
  template:
    metadata:
      labels: {app: $name, svc: $name}
    spec:
      securityContext:
        runAsNonRoot: true
        runAsUser: 65532
        seccompProfile: {type: RuntimeDefault}
      containers:
      - name: fortio
        image: $FORTIO
        args: $args
        ports:
        - {name: http, containerPort: $port}
        readinessProbe:
          httpGet: {path: /, port: $port}
        resources:
          requests: {cpu: 500m, memory: 128Mi}
          limits: {memory: 512Mi}
        securityContext:
          allowPrivilegeEscalation: false
          readOnlyRootFilesystem: true
          capabilities: {drop: [ALL]}
---
apiVersion: v1
kind: Service
metadata:
  name: $name
  namespace: $NS
spec:
  selector: {svc: $name}
  ports:
  - {name: http, port: $port, targetPort: $port}
---
EOF
}

# refusing_workload: the backend pod of mesh-refuse that is not injected (the inject label is disabled, so no
# sidecar) and listens on no port while Ready: no probe, the port declared, a Python sleep (RTY-23). It has the
# svc label of the backend Service, not the app label of the injected backend, so the injected-pod queries
# (running_pods, backend_addrs) do not count it.
refusing_workload() {
  cat <<EOF
apiVersion: apps/v1
kind: Deployment
metadata:
  name: fortio-dead
  namespace: $NS
  labels: {app: fortio-dead}
spec:
  replicas: 1
  selector:
    matchLabels: {app: fortio-dead}
  template:
    metadata:
      labels: {app: fortio-dead, svc: fortio-server, mimic.platform.io/inject: disabled}
    spec:
      securityContext:
        runAsNonRoot: true
        runAsUser: 65532
        seccompProfile: {type: RuntimeDefault}
      containers:
      - name: app
        image: $REFUSING_IMAGE
        command: [python3, -c, "import time; time.sleep(86400)"]
        ports:
        - {name: http, containerPort: 8080}
        resources:
          requests: {cpu: 10m, memory: 16Mi}
          limits: {memory: 64Mi}
        securityContext:
          allowPrivilegeEscalation: false
          readOnlyRootFilesystem: true
          capabilities: {drop: [ALL]}
EOF
}
{
  # The server answers 1 KiB after a fixed 5 ms delay (the echo defaults of fortio's server).
  # `-redirect-port disabled`: fortio's server otherwise also listens on 8081 (an https redirector).
  workload fortio-server '["server", "-http-port", "8080", "-redirect-port", "disabled", "-ui-path", "", "-echo-server-default-params", "size=1024&delay=5ms"]' 8080
  # The client runs fortio's server only to stay up; the load is started with kubectl exec.
  workload fortio-client '["server", "-http-port", "8081", "-redirect-port", "disabled", "-ui-path", ""]' 8081
} | kubectl apply -f -
kubectl -n "$NS" rollout status deploy/fortio-server --timeout=180s
kubectl -n "$NS" rollout status deploy/fortio-client --timeout=180s

# pod_of <app>: the pod of a workload.
# pod_of <app>: the running pod of the app; a pod still terminating after a rollout restart is skipped.
pod_of() {
  kubectl -n "$NS" get pods -l "app=$1" -o json \
    | jq -er '[.items[] | select(.metadata.deletionTimestamp == null and .status.phase == "Running")][0].metadata.name'
}

# running_pods <app>: the names of the ready pods of an app, the terminating ones left out.
running_pods() {
  kubectl -n "$NS" get pods -l "app=$1" -o json \
    | jq -r '.items[] | select(.metadata.deletionTimestamp == null
        and ([.status.conditions[]? | select(.type == "Ready" and .status == "True")] | length) == 1)
        | .metadata.name'
}

# setup_mode <setup>: the mode of the sidecars of a setup. The mesh setups run in discovery, as the B0 setup,
# so that the mesh budgets compare with it (BEN-11).
setup_mode() {
  if [[ $1 == mesh-* ]]; then echo discovery; else echo "$1"; fi
}

# injected_pods <setup>: the backend pods with a sidecar. MESH_PODS in the mesh setups, less the refusing pod of
# mesh-refuse (BEN-11, RTY-23); 1 in the others.
injected_pods() {
  case $1 in
    mesh-refuse) echo $((MESH_PODS - REFUSING_PODS)) ;;
    mesh-*) echo "$MESH_PODS" ;;
    *) echo 1 ;;
  esac
}

# prepare_setup <setup>: plain runs without the sidecar, the others with it (namespace label, a MimicWorkload
# of the mode of the setup, restart). The injected backend has injected_pods pods; mesh-refuse adds its refusing pod.
prepare_setup() {
  local setup=$1 replicas pod containers
  local -a pods refusing
  replicas=$(injected_pods "$setup")
  if [[ $setup != plain ]]; then
    kubectl label --overwrite namespace "$NS" mimic.platform.io/inject=enabled
    # The mode is explicit: a deployment injected by label alone is in none since M0.
    for app in fortio-server fortio-client; do
      kubectl apply -f - <<YAML
apiVersion: mimic.platform.io/v1alpha1
kind: MimicWorkload
metadata: {name: $app, namespace: $NS}
spec: {deployment: $app, mode: $(setup_mode "$setup")}
YAML
    done
    sleep 5
    kubectl -n "$NS" rollout restart deploy/fortio-server deploy/fortio-client
  fi
  kubectl -n "$NS" scale deploy/fortio-server --replicas="$replicas" >/dev/null
  if [[ $setup == mesh-refuse ]]; then
    refusing_workload | kubectl apply -f -
    kubectl -n "$NS" rollout status deploy/fortio-dead --timeout=180s
  fi
  kubectl -n "$NS" rollout status deploy/fortio-server --timeout=180s
  kubectl -n "$NS" rollout status deploy/fortio-client --timeout=180s
  pods=("$(pod_of fortio-client)")
  while IFS= read -r pod; do pods+=("$pod"); done < <(running_pods fortio-server)
  if [[ $setup == mesh-refuse ]]; then
    mapfile -t refusing < <(running_pods fortio-dead)
    if ((${#refusing[@]} != REFUSING_PODS)); then
      echo "FAIL the refusing pod of mesh-refuse: ${#refusing[@]} ready, $REFUSING_PODS expected" >&2
      exit 1
    fi
  fi
  for pod in "${pods[@]}"; do
    containers=$(kubectl -n "$NS" get pod "$pod" -o jsonpath='{.spec.initContainers[*].name}')
    if [[ $setup != plain ]] && ! grep -qw mimic-sidecar <<<"$containers"; then
      echo "FAIL $pod has no mimic-sidecar" >&2
      exit 1
    fi
    if [[ $setup == plain ]] && grep -qw mimic-sidecar <<<"$containers"; then
      echo "FAIL $pod has a mimic-sidecar in the plain setup" >&2
      exit 1
    fi
  done
  if [[ $setup == mesh-* ]] && (( ${#pods[@]} - 1 != replicas )); then
    echo "FAIL the backend has $((${#pods[@]} - 1)) ready injected pods, $replicas expected" >&2
    exit 1
  fi
}

# scrape <pod> <metric>: a value of the sidecar's /metrics (admin port 15100), read through the API server.
scrape() {
  local metrics value
  metrics=$(kubectl get --raw "/api/v1/namespaces/$NS/pods/$1:15100/proxy/metrics")
  value=$(awk -v m="$2" '$1 == m { print $2; exit }' <<<"$metrics")
  [[ -n $value ]] || { echo "FAIL no $2 from pod $1" >&2; exit 1; }
  printf '%s\n' "$value"
}

# sidecar_json <cpu before> <cpu after> <rss after> <seconds>: CPU cores used and RSS of one sidecar in a run.
sidecar_json() {
  jq -n -c --argjson b "$1" --argjson a "$2" --argjson r "$3" --argjson d "$4" \
    '{cpu_cores: ((($a - $b) / $d) * 1000 | round) / 1000, rss_mib: (($r / 1048576) * 10 | round) / 10}'
}

# machine_json: the machine description of the results (BEN-09): no host identity.
machine_json() {
  local cpu cores ram kernel docker kind_version kube
  cpu=$(lscpu)
  cpu=$(sed -n 's/^Model name: *//p' <<<"$cpu" | head -n 1)
  cores=$(nproc)
  ram=$(awk '/^MemTotal:/ { printf "%.1f GiB", $2 / 1048576 }' /proc/meminfo)
  kernel=$(uname -r)
  docker=$(docker version --format '{{.Server.Version}}' 2>/dev/null || echo unknown)
  kind_version=$(kind version)
  kube=$(kubectl version -o json | jq -r '.serverVersion.gitVersion')
  jq -n -c --arg cpu "$cpu" --arg ram "$ram" --arg kernel "$kernel" --arg docker "$docker" \
    --arg kind "$kind_version" --arg kube "$kube" --arg node "$NODE_IMAGE" --argjson cores "$cores" \
    '{cpu: $cpu, cores: $cores, ram: $ram, kernel: $kernel, docker: $docker, kind: $kind, kubernetes: $kube, node_image: $node}'
}
MACHINE=$(machine_json)

# backend_addrs: "<pod> <ip>:8080" of each ready backend pod, the endpoint label of the mimic_mesh_* series (BEN-12).
backend_addrs() {
  kubectl -n "$NS" get pods -l app=fortio-server -o json \
    | jq -r '.items[] | select(.metadata.deletionTimestamp == null and .status.podIP != null
        and ([.status.conditions[]? | select(.type == "Ready" and .status == "True")] | length) == 1)
        | "\(.metadata.name) \(.status.podIP):8080"'
}

# warm_up: a short load from the client through the mesh, so that every backend pod has requests to count.
warm_up() {
  kubectl -n "$NS" exec "$(pod_of fortio-client)" -c fortio -- /usr/bin/fortio load -qps 50 -t 3s \
    -c "$CONNECTIONS" "http://fortio-server:8080/" >/dev/null 2>>"$WORK/fortio.log" || true
}

# mesh_routing <setup> <phase> <strict>: asserts the mesh routing of the client sidecar (BEN-12). Each ready
# injected backend pod must have had requests (mimic_mesh_endpoint_requests_total, by endpoint address), and there
# must be injected_pods of them; with strict 1 no request may have fallen back to the ClusterIP
# (mimic_mesh_fallbacks_total). In mesh-refuse the refusing pod must have been tried: a connect_failed attempt
# (mimic_mesh_attempts_total) is asserted, since a refused try is not written to the pod and has no endpoint
# request. A check that does not hold yet is retried after a warm-up, for up to 60 s, while the view reaches the
# pods; then the setup fails. The phase is written to $WORK/<setup>.routing-<phase>.json, with the retries
# (mimic_mesh_retries_total) and the connect_failed attempts of the client, cumulative since its sidecar started.
mesh_routing() {
  local setup=$1 phase=$2 strict=$3 caller metrics pod addr n fallbacks reconnects retries refused count attempt ok out want
  out="$WORK/$setup.routing-$phase.json"
  want=$(injected_pods "$setup")
  caller=$(pod_of fortio-client)
  for attempt in 1 2 3 4 5 6; do
    warm_up
    metrics=$(kubectl get --raw "/api/v1/namespaces/$NS/pods/$caller:15100/proxy/metrics")
    ok=1
    : >"$WORK/routing.rows"
    while read -r pod addr; do
      n=$(awk -v e="endpoint=\"$addr\"" \
        'index($1, "mimic_mesh_endpoint_requests_total{") == 1 && index($0, e) { sum += $NF } END { print sum + 0 }' \
        <<<"$metrics")
      jq -cn --arg pod "$pod" --arg endpoint "$addr" --argjson requests "$n" \
        '{pod: $pod, endpoint: $endpoint, requests: $requests}' >>"$WORK/routing.rows"
      if ((n == 0)); then ok=0; fi
    done < <(backend_addrs)
    count=$(jq -s length "$WORK/routing.rows")
    if ((count != want)); then ok=0; fi
    fallbacks=$(awk 'index($1, "mimic_mesh_fallbacks_total{") == 1 { sum += $NF } END { print sum + 0 }' <<<"$metrics")
    reconnects=$(awk 'index($1, "mimic_mesh_reconnects_total{") == 1 { sum += $NF } END { print sum + 0 }' <<<"$metrics")
    retries=$(awk 'index($1, "mimic_mesh_retries_total{") == 1 { sum += $NF } END { print sum + 0 }' <<<"$metrics")
    refused=$(awk 'index($1, "mimic_mesh_attempts_total{") == 1 && index($0, "result=\"connect_failed\"") { sum += $NF } END { print sum + 0 }' <<<"$metrics")
    if [[ $strict == 1 ]] && ((fallbacks != 0)); then ok=0; fi
    if [[ $setup == mesh-refuse ]] && ((refused == 0)); then ok=0; fi
    if ((ok)); then
      jq -n --arg phase "$phase" --argjson strict "$strict" --argjson fallbacks "$fallbacks" \
        --argjson reconnects "$reconnects" --argjson retries "$retries" --argjson refused "$refused" \
        --argjson pods "$(jq -s . "$WORK/routing.rows")" \
        '{phase: $phase, strict: ($strict == 1), pods: $pods, fallbacks: $fallbacks, reconnects: $reconnects,
          retries: $retries, connect_failed_attempts: $refused}' \
        >"$out"
      echo "routing $setup $phase ok: requests per pod $(jq -r '[.pods[].requests] | join(" ")' "$out"), fallbacks $fallbacks, retries $retries, connect_failed attempts $refused"
      return 0
    fi
    echo "routing $setup $phase not yet (attempt $attempt): $(jq -c -s . "$WORK/routing.rows"), fallbacks $fallbacks, retries $retries, connect_failed attempts $refused"
    sleep 10
  done
  echo "FAIL mesh routing $setup $phase: the client sidecar does not route to $want injected backend pods (and, in mesh-refuse, tries the refusing pod), see above" >&2
  exit 1
}

# ready_endpoints: the ready endpoints of the backend Service, from its EndpointSlices.
ready_endpoints() {
  kubectl -n "$NS" get endpointslices -l kubernetes.io/service-name=fortio-server -o json \
    | jq '[.items[].endpoints[]? | select(.conditions.ready == true)] | length'
}

# propagation <samples>: EndpointSlice change to the client sidecar's "mesh view applied" (BEN-13), in
# $WORK/propagation.json. The samples alternate: odd ones scale the backend from MESH_PODS to MESH_PODS + 1,
# the others back. t0 is the send time of the last poll that still saw the old ready count, so t0 is not later
# than the change, and the measure overstates the propagation by at most one poll (about 0.1 s). t1 is the
# timestamp of the first "mesh view applied" line at or after t0 with the expected number of endpoints.
propagation() {
  local samples=$1 i target expected caller base prev now ready t0 t1 since deadline
  caller=$(pod_of fortio-client)
  base=$(scrape "$caller" mimic_mesh_view_endpoints)
  : >"$WORK/propagation.rows"
  for ((i = 1; i <= samples; i++)); do
    if ((i % 2)); then target=$((MESH_PODS + 1)); else target=$MESH_PODS; fi
    expected=$((base + target - MESH_PODS))
    prev=$(date +%s.%N)
    kubectl -n "$NS" scale deploy/fortio-server --replicas="$target" >/dev/null
    deadline=$((SECONDS + 120))
    while :; do
      now=$(date +%s.%N)
      ready=$(ready_endpoints)
      if [[ $ready == "$target" ]]; then
        t0=$prev
        break
      fi
      prev=$now
      if ((SECONDS >= deadline)); then
        echo "FAIL propagation sample $i: $target ready backend endpoints not seen in 120 s" >&2
        exit 1
      fi
      sleep 0.05
    done
    since=$(date -u -d "@$((${t0%.*} - 2))" +%Y-%m-%dT%H:%M:%SZ)
    deadline=$((SECONDS + 30))
    while :; do
      t1=$(kubectl -n "$NS" logs "$caller" -c mimic-sidecar --since-time="$since" 2>>"$WORK/logs.log" \
        | jq -nR --argjson n "$expected" --argjson t0 "$t0" '
            def secs: capture("^(?<s>[0-9T:-]+)(\\.(?<f>[0-9]+))?Z$")
              | ((.s + "Z") | fromdateiso8601) + (("0." + (.f // "0")) | tonumber);
            [inputs | fromjson? | select(.message == "mesh view applied" and .endpoints == $n)
              | .timestamp | secs | select(. >= $t0)] | sort | .[0] // empty') || true
      if [[ -n $t1 ]]; then break; fi
      if ((SECONDS >= deadline)); then
        echo "FAIL propagation sample $i: no mesh view applied with $expected endpoints after the change" >&2
        exit 1
      fi
      sleep 0.2
    done
    jq -nc --argjson i "$i" --argjson target "$target" --argjson expected "$expected" \
      --argjson t0 "$t0" --argjson t1 "$t1" \
      '{sample: $i, target: $target, expected_endpoints: $expected, t0: $t0, t1: $t1, ms: (($t1 - $t0) * 1000 * 10 | round / 10)}' \
      >>"$WORK/propagation.rows"
    echo "propagation sample $i: $target backend endpoints, $(jq -r -s '.[-1].ms' "$WORK/propagation.rows") ms"
  done
  jq -s --argjson budget "$PROPAGATION_BUDGET_MS" '
    def pct($p): sort | .[((($p / 100) * length) | ceil) - 1];
    (map(.ms)) as $ms
    | {samples: length, p50_ms: ($ms | pct(50)), p99_ms: ($ms | pct(99)), max_ms: ($ms | max),
       budget_p99_ms: $budget, within_budget: (($ms | pct(99)) <= $budget), values_ms: $ms}' \
    "$WORK/propagation.rows" >"$WORK/propagation.json"
  # An odd number of samples leaves the backend at MESH_PODS + 1: back to MESH_PODS for the next measure.
  kubectl -n "$NS" scale deploy/fortio-server --replicas="$MESH_PODS" >/dev/null
  kubectl -n "$NS" rollout status deploy/fortio-server --timeout=180s >/dev/null
}

# synthetic_view: the RSS of the client sidecar with a synthetic view of SYNTHETIC_SERVICES Services, each with
# SYNTHETIC_ENDPOINTS ready endpoints, in the client's namespace (BEN-15). The baseline is the RSS read in the same
# run, before the objects exist. The result is in $WORK/synthetic.json; the objects are deleted afterwards.
synthetic_view() {
  local caller base_rss before_services before_endpoints want_services want_endpoints services endpoints rss s e name
  caller=$(pod_of fortio-client)
  base_rss=$(scrape "$caller" process_resident_memory_bytes)
  before_services=$(scrape "$caller" mimic_mesh_view_services)
  before_endpoints=$(scrape "$caller" mimic_mesh_view_endpoints)
  want_services=$((before_services + SYNTHETIC_SERVICES))
  want_endpoints=$((before_endpoints + SYNTHETIC_SERVICES * SYNTHETIC_ENDPOINTS))
  for ((s = 0; s < SYNTHETIC_SERVICES; s++)); do
    name=$(printf 'synthetic-%03d' "$s")
    cat <<EOF
apiVersion: v1
kind: Service
metadata:
  name: $name
  namespace: $NS
  labels: {app: synthetic-view}
spec:
  ports:
  - {name: http, port: 8080, targetPort: 8080}
---
apiVersion: discovery.k8s.io/v1
kind: EndpointSlice
metadata:
  name: $name
  namespace: $NS
  labels: {app: synthetic-view, kubernetes.io/service-name: $name}
addressType: IPv4
ports:
- {name: http, port: 8080, protocol: TCP}
endpoints:
EOF
    for ((e = 1; e <= SYNTHETIC_ENDPOINTS; e++)); do
      echo "- {addresses: [\"10.201.$s.$e\"], conditions: {ready: true}}"
    done
    echo "---"
  done >"$WORK/synthetic.yaml"
  kubectl apply -f "$WORK/synthetic.yaml" >/dev/null
  deadline=$((SECONDS + 120))
  while :; do
    services=$(scrape "$caller" mimic_mesh_view_services)
    endpoints=$(scrape "$caller" mimic_mesh_view_endpoints)
    if ((services >= want_services && endpoints >= want_endpoints)); then break; fi
    if ((SECONDS >= deadline)); then
      echo "FAIL synthetic view: the client has $services services and $endpoints endpoints, $want_services and $want_endpoints expected" >&2
      exit 1
    fi
    sleep 1
  done
  sleep 10
  rss=$(scrape "$caller" process_resident_memory_bytes)
  jq -nc --argjson base "$base_rss" --argjson rss "$rss" --argjson budget "$RSS_BUDGET_MIB" \
    --argjson services "$services" --argjson endpoints "$endpoints" \
    --argjson wanted_services "$SYNTHETIC_SERVICES" --argjson wanted_endpoints "$((SYNTHETIC_SERVICES * SYNTHETIC_ENDPOINTS))" '
    def mib: ((. / 1048576) * 10 | round) / 10;
    {services: $wanted_services, endpoints: $wanted_endpoints, view_services: $services, view_endpoints: $endpoints,
     rss_baseline_mib: ($base | mib), rss_mib: ($rss | mib), delta_mib: ((($rss - $base) / 1048576) * 10 | round / 10),
     budget_mib: (($base | mib) + $budget), within_budget: ($rss <= $base + $budget * 1048576)}' >"$WORK/synthetic.json"
  kubectl -n "$NS" delete service,endpointslice -l app=synthetic-view --ignore-not-found --wait=true >/dev/null
  echo "synthetic view: RSS $(jq -r '.rss_mib' "$WORK/synthetic.json") MiB, baseline $(jq -r '.rss_baseline_mib' "$WORK/synthetic.json") MiB"
}

# mesh_summary <setup>: the mesh block of the results of a mesh setup, in $WORK/<setup>.mesh.json (BEN-16).
mesh_summary() {
  local setup=$1
  jq -n --arg setup "$setup" --argjson pods "$MESH_PODS" --argjson refusing "$REFUSING_PODS" \
    --slurpfile before "$WORK/$setup.routing-before.json" --slurpfile after "$WORK/$setup.routing-after.json" \
    --slurpfile runs "$WORK/$setup.jsonl" '
    {backend_pods: $pods, routing: {before: $before[0], after: $after[0]}}
    + (if $setup == "mesh-churn" then
         {churn: {restarts: ($runs | length), errors_total: ($runs | map(.errors) | add // 0),
                  fallbacks_total: $after[0].fallbacks, reconnects_total: $after[0].reconnects}}
       else {} end)
    + (if $setup == "mesh-refuse" then
         {refusing: {injected_pods: ($pods - $refusing), not_injected_pods: $refusing,
                     errors_total: ($runs | map(.errors) | add // 0),
                     retries_total: $after[0].retries, connect_failed_attempts: $after[0].connect_failed_attempts}}
       else {} end)' >"$WORK/$setup.mesh.json"
  if [[ $setup == mesh-lb ]]; then
    jq -n --slurpfile m "$WORK/$setup.mesh.json" --slurpfile p "$WORK/propagation.json" \
      --slurpfile s "$WORK/synthetic.json" '$m[0] + {propagation: $p[0], synthetic_view: $s[0]}' \
      >"$WORK/$setup.mesh.json.tmp"
    mv "$WORK/$setup.mesh.json.tmp" "$WORK/$setup.mesh.json"
  fi
}

# run_one <setup> <protocol> <rate> <index> <total>: one fortio run, sidecars measured before and after (BEN-03).
run_one() {
  local setup=$1 protocol=$2 rate=$3 index=$4 total=$5
  local caller callee raw cb ca cr callee_before callee_after callee_rss caller_json=null callee_json=null
  local measure_callee=1 churn=0
  fortio_args "$protocol" "$rate"
  caller=$(pod_of fortio-client)
  callee=$(pod_of fortio-server)
  # No callee measure in plain (no sidecar) and in mesh-churn (the callee pods change during the run, BEN-14).
  if [[ $setup == plain || $setup == mesh-churn ]]; then measure_callee=0; fi
  if [[ $setup == mesh-churn ]]; then churn=1; fi
  if [[ $setup != plain ]]; then
    cb=$(scrape "$caller" process_cpu_seconds_total)
  fi
  if ((measure_callee)); then
    callee_before=$(scrape "$callee" process_cpu_seconds_total)
  fi
  echo "== run $index/$total setup $setup protocol $protocol rate $rate rps"
  # mesh-churn: a rolling restart of the backend starts with the load and runs through it (BEN-14).
  if ((churn)); then kubectl -n "$NS" rollout restart deploy/fortio-server >/dev/null; fi
  raw=$(kubectl -n "$NS" exec "$caller" -c fortio -- /usr/bin/fortio "${FORTIO_ARGS[@]}" 2>>"$WORK/fortio.log") \
    || { echo "FAIL fortio run $index failed, see $WORK/fortio.log" >&2; exit 1; }
  if ((churn)); then kubectl -n "$NS" rollout status deploy/fortio-server --timeout=180s >/dev/null; fi
  jq -e '.DurationHistogram.Count' <<<"$raw" >/dev/null || { echo "FAIL run $index printed no fortio JSON" >&2; exit 1; }
  if [[ $setup != plain ]]; then
    ca=$(scrape "$caller" process_cpu_seconds_total)
    cr=$(scrape "$caller" process_resident_memory_bytes)
    caller_json=$(sidecar_json "$cb" "$ca" "$cr" "$DURATION_S")
  fi
  if ((measure_callee)); then
    callee_after=$(scrape "$callee" process_cpu_seconds_total)
    callee_rss=$(scrape "$callee" process_resident_memory_bytes)
    callee_json=$(sidecar_json "$callee_before" "$callee_after" "$callee_rss" "$DURATION_S")
  fi
  jq -c -n --arg setup "$setup" --arg protocol "$protocol" --argjson rate "$rate" \
    --argjson connections "$CONNECTIONS" --argjson duration_s "$DURATION_S" --argjson fortio "$raw" \
    --argjson caller "$caller_json" --argjson callee "$callee_json" --argjson churn "$churn" '
    def pct($p): [$fortio.DurationHistogram.Percentiles[] | select(.Percentile == $p) | .Value * 1000][0];
    {
      setup: $setup, protocol: $protocol, rate: $rate, duration_s: $duration_s, connections: $connections,
      requests: $fortio.DurationHistogram.Count,
      actual_qps: ($fortio.ActualQPS * 100 | round) / 100,
      errors: ($fortio.DurationHistogram.Count - ($fortio.RetCodes["200"] // 0)),
      latency_ms: {p50: pct(50), p90: pct(90), p99: pct(99), "p99.9": pct(99.9)},
      rollout_restart: ($churn == 1),
      sidecar: {caller: $caller, callee: $callee},
      fortio: $fortio
    }' >>"$WORK/$setup.jsonl"
}

# write_results <setup>: the JSON file of a setup and one table row per run in the README (BEN-07).
write_results() {
  local setup=$1 file="$OUT_DIR/$TODAY-$1.json" mesh=null
  if [[ -f $WORK/$setup.mesh.json ]]; then mesh=$(cat "$WORK/$setup.mesh.json"); fi
  jq -s --arg date "$TODAY" --arg setup "$setup" --argjson machine "$MACHINE" \
    --arg duration "$DURATION" --argjson connections "$CONNECTIONS" --argjson mesh "$mesh" \
    '{date: $date, setup: $setup, machine: $machine, duration: $duration, connections: $connections, mesh: $mesh, runs: .}' \
    "$WORK/$setup.jsonl" >"$file"
  jq -r --arg date "$TODAY" --arg setup "$setup" '
    def v: if . == null then "-" else tostring end;
    def r2: if . == null then "-" else (. * 100 | round / 100 | tostring) end;
    "| \($date) | \($setup) | \(.protocol) | \(.rate) | \(.fortio.ActualQPS | r2) | \(.latency_ms.p50 | r2) | \(.latency_ms.p90 | r2) | \(.latency_ms.p99 | r2) | \(.latency_ms["p99.9"] | r2) | \(.errors) | \(.sidecar.caller.cpu_cores | v) | \(.sidecar.callee.cpu_cores | v) | \(.sidecar.caller.rss_mib | v) | \(.sidecar.callee.rss_mib | v) |"' \
    "$WORK/$setup.jsonl" >>"$OUT_DIR/README.md"
  echo "wrote $file"
}

total=$((${#ORDERED_SETUPS[@]} * ${#PROTOCOL_LIST[@]} * ${#RATE_LIST[@]}))
index=0
for setup in "${ORDERED_SETUPS[@]}"; do
  prepare_setup "$setup"
  if [[ $setup == mesh-* ]]; then mesh_routing "$setup" before 1; fi
  for protocol in "${PROTOCOL_LIST[@]}"; do
    for rate in "${RATE_LIST[@]}"; do
      index=$((index + 1))
      run_one "$setup" "$protocol" "$rate" "$index" "$total"
      if ((index < total)); then sleep "$PAUSE"; fi
    done
  done
  if [[ $setup == mesh-* ]]; then
    # Fallbacks are not asserted under mesh-churn: a refused connection to a restarting pod is measured (BEN-14).
    # mesh-refuse is strict: its refusing pod is retried to a sibling, so no try should need the ClusterIP.
    strict=0
    if [[ $setup == mesh-lb || $setup == mesh-refuse ]]; then strict=1; fi
    mesh_routing "$setup" after "$strict"
  fi
  if [[ $setup == mesh-lb ]]; then
    if ((PROPAGATION_SAMPLES > 0)); then
      propagation "$PROPAGATION_SAMPLES"
    else
      echo '{"samples": 0}' >"$WORK/propagation.json"
    fi
    synthetic_view
  fi
  if [[ $setup == mesh-* ]]; then mesh_summary "$setup"; fi
  write_results "$setup"
done
echo "done: $total runs"
