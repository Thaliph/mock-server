#!/usr/bin/env bash
# Baseline benchmarks of the sidecar (BEN-01 to BEN-10, docs/SPEC.md §12.19, #436). Run by hand on a quiet
# machine; not a CI job.
#
#   scripts/bench.sh               the default matrix: 12 runs of 30 s, about 8 minutes, plus the cluster
#   scripts/bench.sh --dry-run     the plan only, nothing started (scripts/test_bench.sh reads it)
#
# Environment (defaults in brackets):
#   CLUSTER=mimic-bench            kind cluster: created when absent, reused otherwise
#   SETUPS="plain discovery"       plain: no sidecar; discovery: the sidecar in discovery mode
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
SETUPS=${SETUPS-"plain discovery"}
PROTOCOLS=${PROTOCOLS-"http1 h2c"}
RATES=${RATES-"50 100 200"}
DURATION=${DURATION-30s}
CONNECTIONS=${CONNECTIONS-8}
PAUSE=${PAUSE-10}
KEEP_CLUSTER=${KEEP_CLUSTER-1}
OUT_DIR=${BENCH_OUT_DIR-docs/bench}
LOCK_DIR=${MIMIC_LOCK_DIR-${XDG_RUNTIME_DIR:-/tmp}}
NS=mimic-bench
SYSTEM_NS=mimic-system
RELEASE=mimic
RATE_CAP=200
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
  contains "$setup" plain discovery || config_error SETUPS "$setup is not plain or discovery"
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
[[ $CLUSTER =~ ^[a-z0-9-]+$ ]] || config_error CLUSTER "$CLUSTER does not match ^[a-z0-9-]+\$"
# The e2e clusters (scripts/e2e.sh, and the one an owner keeps for manual tests) are never benchmarked or deleted.
[[ $CLUSTER == mimic-e2e* ]] && config_error CLUSTER "$CLUSTER is an e2e cluster"
[[ $KEEP_CLUSTER == 0 || $KEEP_CLUSTER == 1 ]] || config_error KEEP_CLUSTER "$KEEP_CLUSTER is not 0 or 1"

# The pinned images: the kind node image and the fortio image (BEN-05, scripts/pinned-image.sh).
NODE_IMAGE=${NODE_IMAGE-$(scripts/pinned-image.sh kind-node)}
[[ $NODE_IMAGE =~ @sha256:[0-9a-f]{64}$ ]] || config_error NODE_IMAGE "$NODE_IMAGE is not pinned by digest"
FORTIO=$(scripts/pinned-image.sh fortio)
TAG=${TAG:-bench-$(git rev-parse --short HEAD 2>/dev/null || echo local)-$$}
IMAGE=${IMAGE:-ghcr.io/thaliph/mimic:$TAG}
INIT_IMAGE=${INIT_IMAGE:-ghcr.io/thaliph/mimic-init:$TAG}
SKIP_BUILD=${SKIP_BUILD-0}

# The setups run in this order whatever the order of SETUPS: plain first, then the namespace is labelled.
ORDERED_SETUPS=()
for setup in plain discovery; do
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
  echo "images $IMAGE $INIT_IMAGE (built here unless SKIP_BUILD=1)"
  echo "machine fields: cpu cores ram kernel docker kind kubernetes node_image"
  for setup in "${ORDERED_SETUPS[@]}"; do
    echo "results $OUT_DIR/$TODAY-$setup.json"
  done
  echo "rows appended to $OUT_DIR/README.md"
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
      labels: {app: $name}
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
  selector: {app: $name}
  ports:
  - {name: http, port: $port, targetPort: $port}
---
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
pod_of() {
  kubectl -n "$NS" get pods -l "app=$1" -o jsonpath='{.items[0].metadata.name}'
}

# prepare_setup <setup>: plain runs without the sidecar, discovery with it (namespace label + restart).
prepare_setup() {
  if [[ $1 == discovery ]]; then
    kubectl label --overwrite namespace "$NS" mimic.platform.io/inject=enabled
    # A deployment injected by label alone is in mode none since M0: this setup records.
    for app in fortio-server fortio-client; do
      kubectl apply -f - <<YAML
apiVersion: mimic.platform.io/v1alpha1
kind: MimicWorkload
metadata: {name: $app, namespace: $NS}
spec: {deployment: $app, mode: discovery}
YAML
    done
    sleep 5
    kubectl -n "$NS" rollout restart deploy/fortio-server deploy/fortio-client
    kubectl -n "$NS" rollout status deploy/fortio-server --timeout=180s
    kubectl -n "$NS" rollout status deploy/fortio-client --timeout=180s
  fi
  local pod containers
  for pod in "$(pod_of fortio-server)" "$(pod_of fortio-client)"; do
    containers=$(kubectl -n "$NS" get pod "$pod" -o jsonpath='{.spec.initContainers[*].name}')
    if [[ $1 == discovery ]] && ! grep -qw mimic-sidecar <<<"$containers"; then
      echo "FAIL $pod has no mimic-sidecar" >&2
      exit 1
    fi
    if [[ $1 == plain ]] && grep -qw mimic-sidecar <<<"$containers"; then
      echo "FAIL $pod has a mimic-sidecar in the plain setup" >&2
      exit 1
    fi
  done
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

# run_one <setup> <protocol> <rate> <index> <total>: one fortio run, sidecars measured before and after (BEN-03).
run_one() {
  local setup=$1 protocol=$2 rate=$3 index=$4 total=$5
  local caller callee raw cb ca cr callee_before callee_after callee_rss caller_json=null callee_json=null
  fortio_args "$protocol" "$rate"
  caller=$(pod_of fortio-client)
  callee=$(pod_of fortio-server)
  if [[ $setup == discovery ]]; then
    cb=$(scrape "$caller" process_cpu_seconds_total)
    callee_before=$(scrape "$callee" process_cpu_seconds_total)
  fi
  echo "== run $index/$total setup $setup protocol $protocol rate $rate rps"
  raw=$(kubectl -n "$NS" exec "$caller" -c fortio -- /usr/bin/fortio "${FORTIO_ARGS[@]}" 2>>"$WORK/fortio.log") \
    || { echo "FAIL fortio run $index failed, see $WORK/fortio.log" >&2; exit 1; }
  jq -e '.DurationHistogram.Count' <<<"$raw" >/dev/null || { echo "FAIL run $index printed no fortio JSON" >&2; exit 1; }
  if [[ $setup == discovery ]]; then
    ca=$(scrape "$caller" process_cpu_seconds_total)
    cr=$(scrape "$caller" process_resident_memory_bytes)
    caller_json=$(sidecar_json "$cb" "$ca" "$cr" "$DURATION_S")
    callee_after=$(scrape "$callee" process_cpu_seconds_total)
    callee_rss=$(scrape "$callee" process_resident_memory_bytes)
    callee_json=$(sidecar_json "$callee_before" "$callee_after" "$callee_rss" "$DURATION_S")
  fi
  jq -c -n --arg setup "$setup" --arg protocol "$protocol" --argjson rate "$rate" \
    --argjson connections "$CONNECTIONS" --argjson duration_s "$DURATION_S" --argjson fortio "$raw" \
    --argjson caller "$caller_json" --argjson callee "$callee_json" '
    def pct($p): [$fortio.DurationHistogram.Percentiles[] | select(.Percentile == $p) | .Value * 1000][0];
    {
      setup: $setup, protocol: $protocol, rate: $rate, duration_s: $duration_s, connections: $connections,
      requests: $fortio.DurationHistogram.Count,
      actual_qps: ($fortio.ActualQPS * 100 | round) / 100,
      errors: ($fortio.DurationHistogram.Count - ($fortio.RetCodes["200"] // 0)),
      latency_ms: {p50: pct(50), p90: pct(90), p99: pct(99), "p99.9": pct(99.9)},
      sidecar: {caller: $caller, callee: $callee},
      fortio: $fortio
    }' >>"$WORK/$setup.jsonl"
}

# write_results <setup>: the JSON file of a setup and one table row per run in the README (BEN-07).
write_results() {
  local setup=$1 file="$OUT_DIR/$TODAY-$1.json"
  jq -s --arg date "$TODAY" --arg setup "$setup" --argjson machine "$MACHINE" \
    --arg duration "$DURATION" --argjson connections "$CONNECTIONS" \
    '{date: $date, setup: $setup, machine: $machine, duration: $duration, connections: $connections, runs: .}' \
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
  for protocol in "${PROTOCOL_LIST[@]}"; do
    for rate in "${RATE_LIST[@]}"; do
      index=$((index + 1))
      run_one "$setup" "$protocol" "$rate" "$index" "$total"
      if ((index < total)); then sleep "$PAUSE"; fi
    done
  done
  write_results "$setup"
done
echo "done: $total runs"
