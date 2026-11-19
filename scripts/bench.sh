#!/usr/bin/env bash
# Baseline benchmarks of the sidecar (BEN-01 to BEN-10, docs/SPEC.md §12.19, #436) and of the mesh
# (BEN-11 to BEN-16, #444; mesh-refuse, RTY-23, #445; mesh-eject and mesh-breaker, CBR-34, #451; mesh-admission, ADM-32,
# #453). Run by hand on a quiet machine; not a CI job.
#
#   scripts/bench.sh               the default matrix: 18 runs of 30 s with 10 s pauses, about 12 minutes, plus the cluster
#   scripts/bench.sh --dry-run     the plan only, nothing started (scripts/test_bench.sh reads it)
#   SETUPS="mesh-lb mesh-churn mesh-refuse" scripts/bench.sh   the mesh setups (3 backend pods, routing asserted)
#   SETUPS=mesh-mirror scripts/bench.sh   every request mirrored to a second Service (RTE-94, #450)
#   SETUPS="mesh-eject mesh-breaker" RATES=200 scripts/bench.sh   outlier ejection and circuit breaker (CBR-34, #451)
#   SETUPS=mesh-admission scripts/bench.sh   the admission contract of the backend pods (ADM-32, #453)
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
#                                  injected and refuses connections (RTY-23, #445); mesh-mirror: mesh-lb with
#                                  an HTTPRoute mirroring every request to a second fortio Service; each
#                                  rate and protocol runs without the route, then with it (RTE-94, #450);
#                                  mesh-eject: mesh-refuse with the third pod resetting every connection after
#                                  its request instead of refusing; mesh-breaker: mesh-lb with a MimicConfig
#                                  circuitBreaker.maxRequests 8 and a 50 ms backend (CBR-34, #451);
#                                  mesh-admission: the topology of mesh-breaker without its MimicConfig; each
#                                  rate and protocol runs without an admission contract, then under one that
#                                  never binds (64/1000 per pod), then each protocol runs once at twice the
#                                  rps of a contract 64/30 per pod (ADM-32, #453)
#   PROPAGATION_SAMPLES=20         mesh-lb only: EndpointSlice changes timed, 0 to 100; 0 skips them
#   PROTOCOLS="http1 h2c"          HTTP/1.1 keep-alive; h2c (HTTP/2 over cleartext)
#   RATES="50 100 200"             requests per second, each 1 to 200
#   DURATION=30s                   one fortio run, 1 to 300 s
#   CONNECTIONS=8                  fortio connections, 1 to 64
#   MIRROR_DELAY_MS=5              mesh-mirror: the response delay of the mirror Service, 0 to 1000 (the primary's is 5)
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
MIRROR_DELAY_MS=${MIRROR_DELAY_MS-5}
PROPAGATION_SAMPLES=${PROPAGATION_SAMPLES-20}
KEEP_CLUSTER=${KEEP_CLUSTER-1}
OUT_DIR=${BENCH_OUT_DIR-docs/bench}
LOCK_DIR=${MIMIC_LOCK_DIR-${XDG_RUNTIME_DIR:-/tmp}}
NS=mimic-bench
SYSTEM_NS=mimic-system
RELEASE=mimic
RATE_CAP=200
MIRROR_MISMATCH=0
FAILED_SETUPS=()
MIRROR_P50_BUDGET_MS=0.2    # mesh-mirror: the primary's p50 with the mirror over its p50 without (design §14, M3)
MESH_PODS=3                 # the backend of the mesh setups (BEN-11)
REFUSING_PODS=1             # mesh-refuse: of the MESH_PODS backend pods, this many are not injected and refuse (RTY-23);
                            # mesh-eject: this many are not injected and reset every connection (CBR-34)
BREAKER_MAX_REQUESTS=8      # mesh-breaker: circuitBreaker.maxRequests of the backend Service (CBR-34)
BREAKER_DELAY_MS=50         # mesh-breaker: the response delay of the backend
BREAKER_OPEN_BUDGET_MS=1    # mesh-breaker: the p99 of the access duration_ms of the breaker-open lines, at most (CBR-34)
EJECT_PER_EJECTION=5        # mesh-eject: consecutiveFailures (default); the bound per ejection adds the connections in flight - 1
BREAKER_CONNECTIONS=32      # mesh-breaker: fortio connections, more than maxRequests so that the breaker opens (BEN-03: at most 64)
BREAKER_OPEN_MIN_PERCENT=10 # mesh-breaker: breaker-open answers, at least this share of the requests, proves it opened (CBR-34)
ADMISSION_UNDER_CONCURRENCY=64 ADMISSION_UNDER_RPS=1000   # mesh-admission: never binding, 200 rps over 3 pods is about 67 per pod
ADMISSION_OVER_CONCURRENCY=64                             # a queue of 7 places
ADMISSION_RPS=30                                          # per pod, the "over" runs
ADMISSION_OVER_RATE=$((2 * MESH_PODS * ADMISSION_RPS))    # 180 rps, under the 200 cap
ADMISSION_OVER_CONNECTIONS=64                             # refusals wait up to 150 ms (BEN-03 cap)
ADMISSION_P99_BUDGET_MS=0.5 ADMISSION_REFUSAL_BUDGET_MS=1 ADMISSION_QUEUE_BUDGET_MS=160 ADMISSION_RATE_PERCENT=5
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

# variants_of <setup>: the variants of each rate and protocol, in order. mesh-mirror runs without any HTTPRoute
# (none), with the HTTPRoute and no mirror filter (route, the baseline of RTE-94), then with the mirror (on);
# mesh-admission without any admission contract (adm-off), then under one that never binds (adm-under), and runs_of
# adds its adm-over run to each protocol; the others have one run (-). run_total counts them.
variants_of() {
  case $1 in
    mesh-mirror) echo "none route on" ;;
    mesh-admission) echo "adm-off adm-under" ;;
    *) echo "-" ;;
  esac
}
# variant_label <variant>: the plan words of a variant.
variant_label() {
  case $1 in
    none) echo " (no route)" ;;
    route) echo " (route, no mirror)" ;;
    on) echo " (route with mirror)" ;;
    adm-off) echo " (no admission)" ;;
    adm-under) echo " (admission $ADMISSION_UNDER_CONCURRENCY/$ADMISSION_UNDER_RPS)" ;;
    adm-over) echo " (admission $ADMISSION_OVER_CONCURRENCY/$ADMISSION_RPS, 2x)" ;;
  esac
}

read -ra SETUP_LIST <<<"$SETUPS"
read -ra PROTOCOL_LIST <<<"$PROTOCOLS"
read -ra RATE_LIST <<<"$RATES"
((${#SETUP_LIST[@]} > 0)) || config_error SETUPS "empty"
((${#PROTOCOL_LIST[@]} > 0)) || config_error PROTOCOLS "empty"
((${#RATE_LIST[@]} > 0)) || config_error RATES "empty"
for setup in "${SETUP_LIST[@]}"; do
  contains "$setup" plain none discovery mesh-lb mesh-churn mesh-mirror mesh-refuse mesh-eject mesh-breaker mesh-admission \
    || config_error SETUPS "$setup is not plain, none, discovery, mesh-lb, mesh-churn, mesh-mirror, mesh-refuse, mesh-eject, mesh-breaker or mesh-admission"
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
if ! [[ $MIRROR_DELAY_MS =~ ^(0|[1-9][0-9]{0,2}|1000)$ ]]; then
  config_error MIRROR_DELAY_MS "$MIRROR_DELAY_MS is not 0 to 1000"
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
# the mesh setups last (they scale the backend to 3 pods, and the others need 1). mesh-refuse comes after the
# others: its refusing pod stays in the namespace, and the setups before it must not see it. mesh-eject and
# mesh-breaker come after it and remove the refusing pod first (prepare_setup); mesh-admission runs last.
ORDERED_SETUPS=()
for setup in plain none discovery mesh-lb mesh-churn mesh-mirror mesh-refuse mesh-eject mesh-breaker mesh-admission; do
  contains "$setup" "${SETUP_LIST[@]}" && ORDERED_SETUPS+=("$setup")
done

# The output files of a run do not exist yet (BEN-02): a result is never overwritten.
for setup in "${ORDERED_SETUPS[@]}"; do
  file="$OUT_DIR/$TODAY-$setup.json"
  [[ ! -e $file ]] || config_error BENCH_OUT_DIR "$file already exists, not overwritten"
done

# setup_connections <setup> [variant]: the fortio connections of a run: CONNECTIONS, BREAKER_CONNECTIONS for
# mesh-breaker and mesh-admission, ADMISSION_OVER_CONNECTIONS for the adm-over run of mesh-admission.
setup_connections() {
  case $1/${2:--} in
    mesh-admission/adm-over) echo "$ADMISSION_OVER_CONNECTIONS" ;;
    mesh-breaker/* | mesh-admission/*) echo "$BREAKER_CONNECTIONS" ;;
    *) echo "$CONNECTIONS" ;;
  esac
}

# runs_of <setup>: one line "<protocol> <rate> <variant>" per run of the setup, in order. mesh-admission ends each
# protocol with one adm-over run at ADMISSION_OVER_RATE, after the pairs of every rate.
runs_of() {
  local protocol rate variant
  for protocol in "${PROTOCOL_LIST[@]}"; do
    for rate in "${RATE_LIST[@]}"; do
      for variant in $(variants_of "$1"); do echo "$protocol $rate $variant"; done
    done
    if [[ $1 == mesh-admission ]]; then echo "$protocol $ADMISSION_OVER_RATE adm-over"; fi
  done
}

# run_total: the runs of every setup.
run_total() {
  local setup n=0
  for setup in "${ORDERED_SETUPS[@]}"; do n=$((n + $(runs_of "$setup" | wc -l))); done
  echo "$n"
}

# eject_bound: requests the resetting pod may receive per ejection (CBR-34): the consecutive failures that eject it,
# plus the requests already in flight on the other connections, which an ejection cannot recall.
eject_bound() { echo $((EJECT_PER_EJECTION + CONNECTIONS - 1)); }

# fortio_args <protocol> <rate> [setup] [variant]: the fortio load command of one run, in FORTIO_ARGS (BEN-03).
# mesh-breaker and mesh-admission ask the backend for its own delay in the URL (the echo parameters of fortio's
# server), the others use the default.
fortio_args() {
  local url="http://fortio-server:8080/?size=1024&delay=${BREAKER_DELAY_MS}ms"
  FORTIO_ARGS=(load -qps "$2" -t "$DURATION" -c "$(setup_connections "${3:-}" "${4:-}")" -p "50,90,99,99.9" -json -)
  if [[ $1 == h2c ]]; then FORTIO_ARGS+=(-h2); fi
  case ${3:-}/${4:--} in
    # More connections than maxRequests: the warm-up requests (one per connection) are already answered breaker-open.
    mesh-breaker/*) FORTIO_ARGS+=(-allow-initial-errors "$url") ;;
    # Twice the contract: the warm-up requests (one per connection) may be refused already.
    mesh-admission/adm-over) FORTIO_ARGS+=(-allow-initial-errors "$url") ;;
    mesh-admission/*) FORTIO_ARGS+=("$url") ;;
    *) FORTIO_ARGS+=("http://fortio-server:8080/") ;;
  esac
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
      mesh-mirror) echo "mesh-mirror: backend $MESH_PODS pods, mode discovery; every rate and protocol runs without any HTTPRoute, with the HTTPRoute and no mirror, then with it mirroring every request to a second fortio Service; routing asserted before and after the runs; the primary p50 budget is +$MIRROR_P50_BUDGET_MS ms and the mirror must receive as many requests as the primary" ;;
      mesh-refuse) echo "mesh-refuse: backend $MESH_PODS pods, $((MESH_PODS - REFUSING_PODS)) injected and $REFUSING_PODS not injected and refusing connections, mode discovery; routing asserted before and after the runs; retries and errors recorded" ;;
      mesh-eject) echo "mesh-eject: backend $MESH_PODS pods, $((MESH_PODS - REFUSING_PODS)) injected and $REFUSING_PODS not injected and resetting every connection, mode discovery; routing and an ejection asserted before and after the runs; the error rate must be 0 % and the resetting pod receives at most $(eject_bound) requests per ejection of the caller sidecar ($EJECT_PER_EJECTION consecutive failures + $CONNECTIONS connections - 1)" ;;
      mesh-breaker) echo "mesh-breaker: backend $MESH_PODS pods answering after $BREAKER_DELAY_MS ms, MimicConfig circuitBreaker.maxRequests $BREAKER_MAX_REQUESTS, $BREAKER_CONNECTIONS connections, mode discovery; routing asserted before and after the runs; at least $BREAKER_OPEN_MIN_PERCENT % of the requests must be answered breaker-open and the p99 of the access duration_ms of the breaker-open lines must be at most $BREAKER_OPEN_BUDGET_MS ms" ;;
      mesh-admission) echo "mesh-admission: backend $MESH_PODS pods answering after $BREAKER_DELAY_MS ms, mode discovery; routing asserted before and after the runs; each rate runs without an admission contract, then under MimicConfig admission $ADMISSION_UNDER_CONCURRENCY/$ADMISSION_UNDER_RPS per pod, $BREAKER_CONNECTIONS connections: the p99 under it must be at most +$ADMISSION_P99_BUDGET_MS ms over the run without it, with no error; then each protocol runs once at $ADMISSION_OVER_RATE rps under admission $ADMISSION_OVER_CONCURRENCY/$ADMISSION_RPS per pod (2x), $ADMISSION_OVER_CONNECTIONS connections: at least 95 % of $ADMISSION_OVER_RATE rps sent, the admitted rate within $ADMISSION_RATE_PERCENT % of $ADMISSION_RPS rps per pod, the p99 of the access duration_ms of the overloaded lines at most $ADMISSION_REFUSAL_BUDGET_MS ms and the longest queue-timeout at most $ADMISSION_QUEUE_BUDGET_MS ms" ;;
    esac
  done
  if contains mesh-lb "${ORDERED_SETUPS[@]}"; then
    echo "mesh-lb propagation: $PROPAGATION_SAMPLES EndpointSlice changes, budget p99 $PROPAGATION_BUDGET_MS ms"
    echo "mesh-lb synthetic view: $SYNTHETIC_SERVICES Services, $SYNTHETIC_ENDPOINTS endpoints each, RSS budget baseline + $RSS_BUDGET_MIB MiB"
  fi
  check_locks
  total=$(run_total)
  index=0
  for setup in "${ORDERED_SETUPS[@]}"; do
    mapfile -t runs < <(runs_of "$setup")
    for run in "${runs[@]}"; do
      read -r protocol rate variant <<<"$run"
      index=$((index + 1))
      fortio_args "$protocol" "$rate" "$setup" "$variant"
      echo "run $index/$total setup $setup protocol $protocol rate $rate rps$(variant_label "$variant"): fortio ${FORTIO_ARGS[*]}"
      if ((index < total)); then echo "pause ${PAUSE}s"; fi
    done
  done
  echo "cleanup on exit: kubectl delete namespace $NS"
  exit 0
fi

# ---- real run ---------------------------------------------------------------------------------------

LOCKED=0
GW_CRDS=0
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
  # Only the Gateway API CRDs this run installed: a cluster that had them keeps them.
  if [[ $GW_CRDS == 1 ]]; then
    kubectl delete -f "$WORK/gateway-standard.yaml" --ignore-not-found --wait=false >/dev/null 2>&1 || true
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
# resetting_workload: the backend pod of mesh-eject that is not injected and resets every connection after reading
# its request (CBR-34): port 8080 reads the request, counts it and closes with a reset; port 8081 answers 200 to the
# readiness probe and, as the body, the number of requests counted. Same labels as the refusing pod.
resetting_workload() {
  cat <<EOF
apiVersion: apps/v1
kind: Deployment
metadata:
  name: fortio-reset
  namespace: $NS
  labels: {app: fortio-reset}
spec:
  replicas: 1
  selector:
    matchLabels: {app: fortio-reset}
  template:
    metadata:
      labels: {app: fortio-reset, svc: fortio-server, mimic.platform.io/inject: disabled}
    spec:
      securityContext:
        runAsNonRoot: true
        runAsUser: 65532
        seccompProfile: {type: RuntimeDefault}
      containers:
      - name: app
        image: $REFUSING_IMAGE
        command: [python3, -c]
        args:
        - |
          import http.server, socket, struct, threading
          count = [0]
          def reset(c):
              try:
                  if c.recv(65536):
                      count[0] += 1
                  c.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
              except OSError:
                  pass
              finally:
                  c.close()
          def serve():
              s = socket.socket()
              s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
              s.bind(("", 8080))
              s.listen(128)
              while True:
                  c, _ = s.accept()
                  threading.Thread(target=reset, args=(c,), daemon=True).start()
          class H(http.server.BaseHTTPRequestHandler):
              def do_GET(self):
                  body = str(count[0]).encode()
                  self.send_response(200)
                  self.send_header("content-length", str(len(body)))
                  self.end_headers()
                  self.wfile.write(body)
              def log_message(self, *args):
                  pass
          threading.Thread(target=serve, daemon=True).start()
          http.server.ThreadingHTTPServer(("", 8081), H).serve_forever()
        ports:
        - {name: http, containerPort: 8080}
        - {name: probe, containerPort: 8081}
        readinessProbe:
          httpGet: {path: /, port: 8081}
          periodSeconds: 1
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
# mesh-refuse and the resetting pod of mesh-eject (BEN-11, RTY-23, CBR-34); 1 in the others.
injected_pods() {
  case $1 in
    mesh-refuse | mesh-eject) echo $((MESH_PODS - REFUSING_PODS)) ;;
    mesh-*) echo "$MESH_PODS" ;;
    *) echo 1 ;;
  esac
}

# ensure_gateway_crds: the Gateway API standard CRDs, when the cluster has none: the manifest pinned by
# scripts/e2e.sh (version and SHA-256 are read from there, one pin), removed on exit by this run only (RTE-94).
ensure_gateway_crds() {
  local version sha256
  if kubectl get crd httproutes.gateway.networking.k8s.io -o name >/dev/null 2>&1; then return 0; fi
  version=$(sed -n 's/^gw_version=//p' scripts/e2e.sh)
  sha256=$(sed -n 's/^gw_sha256=//p' scripts/e2e.sh)
  [[ -n $version && $sha256 =~ ^[0-9a-f]{64}$ ]] || { echo "FAIL the Gateway API pin of scripts/e2e.sh is not readable" >&2; exit 1; }
  curl -fsSL --max-time 120 -o "$WORK/gateway-standard.yaml" \
    "https://github.com/kubernetes-sigs/gateway-api/releases/download/$version/standard-install.yaml"
  [[ $(sha256sum "$WORK/gateway-standard.yaml" | cut -d' ' -f1) == "$sha256" ]] \
    || { echo "FAIL the Gateway API $version standard CRDs do not match the pinned digest" >&2; exit 1; }
  GW_CRDS=1
  kubectl apply --server-side -f "$WORK/gateway-standard.yaml" >/dev/null
  kubectl wait --for=condition=Established crd/httproutes.gateway.networking.k8s.io \
    crd/referencegrants.gateway.networking.k8s.io --timeout=60s >/dev/null
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
  if [[ $setup == mesh-mirror ]]; then
    ensure_gateway_crds
    # The mirror backend: a second fortio server, one pod, injected like the others (the namespace label).
    workload fortio-mirror '["server", "-http-port", "8080", "-redirect-port", "disabled", "-ui-path", "", "-echo-server-default-params", "size=1024&delay='"$MIRROR_DELAY_MS"'ms"]' 8080 | kubectl apply -f -
    kubectl -n "$NS" rollout status deploy/fortio-mirror --timeout=180s
  fi
  if [[ $setup == mesh-* ]]; then
    # Nothing of an earlier setup stays: a refusing or resetting pod, a circuit breaker or an admission contract would
    # change the numbers.
    kubectl -n "$NS" delete mimicconfig fortio-breaker fortio-admission --ignore-not-found >/dev/null
    ADMISSION_STATE=adm-off
    if [[ $setup != mesh-refuse ]]; then
      kubectl -n "$NS" delete deploy/fortio-dead --ignore-not-found --wait=true >/dev/null
    fi
    if [[ $setup != mesh-eject ]]; then
      kubectl -n "$NS" delete deploy/fortio-reset --ignore-not-found --wait=true >/dev/null
    fi
  fi
  if [[ $setup == mesh-refuse ]]; then
    refusing_workload | kubectl apply -f -
    kubectl -n "$NS" rollout status deploy/fortio-dead --timeout=180s
  fi
  if [[ $setup == mesh-eject ]]; then
    resetting_workload | kubectl apply -f -
    kubectl -n "$NS" rollout status deploy/fortio-reset --timeout=180s
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
  if [[ $setup == mesh-eject ]]; then
    mapfile -t refusing < <(running_pods fortio-reset)
    if ((${#refusing[@]} != REFUSING_PODS)); then
      echo "FAIL the resetting pod of mesh-eject: ${#refusing[@]} ready, $REFUSING_PODS expected" >&2
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
  if [[ $setup == mesh-breaker ]]; then breaker_config; fi
}

ADMISSION_STATE=adm-off
# admission_config <variant>: the MimicConfig fortio-admission of mesh-admission on the backend Deployment: deleted for
# adm-off, the contract of the variant otherwise. Nothing is done when it is in force already. Otherwise it waits, up
# to 60 s, until the config is Accepted (when applied) and every backend pod's sidecar, which enforces the contract,
# has pulled a view after it, and a little more for the apply.
admission_config() {
  local variant=$1 attempt pod grown concurrency rps accepted i
  local -a pods before
  [[ $variant != "$ADMISSION_STATE" ]] || return 0
  mapfile -t pods < <(running_pods fortio-server)
  before=()
  for pod in "${pods[@]}"; do before+=("$(pulls_ok "$pod")"); done
  case $variant in
    adm-off) kubectl -n "$NS" delete mimicconfig fortio-admission --ignore-not-found >/dev/null ;;
    *)
      if [[ $variant == adm-over ]]; then
        concurrency=$ADMISSION_OVER_CONCURRENCY rps=$ADMISSION_RPS
      else
        concurrency=$ADMISSION_UNDER_CONCURRENCY rps=$ADMISSION_UNDER_RPS
      fi
      kubectl apply -f - >/dev/null <<YAML
apiVersion: mimic.platform.io/v1alpha1
kind: MimicConfig
metadata: {name: fortio-admission, namespace: $NS}
spec:
  targetRefs: [{group: apps, kind: Deployment, name: fortio-server}]
  admission: {concurrency: $concurrency, rps: $rps}
YAML
      ;;
  esac
  for attempt in $(seq 60); do
    grown=1
    for i in "${!pods[@]}"; do
      (($(pulls_ok "${pods[$i]}") > before[i])) || grown=0
    done
    accepted=True
    if [[ $variant != adm-off ]]; then
      accepted=$(kubectl -n "$NS" get mimicconfig fortio-admission \
        -o jsonpath='{.status.conditions[?(@.type=="Accepted")].status}')
    fi
    if ((grown)) && [[ $accepted == True ]]; then
      ADMISSION_STATE=$variant
      sleep 2
      return 0
    fi
    sleep 1
  done
  echo "FAIL mesh-admission: the MimicConfig is not Accepted or a backend sidecar pulled no view after the change to $variant in 60 s (attempt $attempt)" >&2
  exit 1
}

# admission_contract <variant>: the contract of a variant as JSON, null for adm-off.
admission_contract() {
  case $1 in
    adm-off) echo null ;;
    adm-under) echo "{\"concurrency\": $ADMISSION_UNDER_CONCURRENCY, \"rps\": $ADMISSION_UNDER_RPS}" ;;
    adm-over) echo "{\"concurrency\": $ADMISSION_OVER_CONCURRENCY, \"rps\": $ADMISSION_RPS}" ;;
  esac
}

# inbound_requests <pod> [class]: the inbound requests the sidecar of the pod counted (mimic_http_requests_total), of one
# status_class when given.
inbound_requests() {
  kubectl get --raw "/api/v1/namespaces/$NS/pods/$1:15100/proxy/metrics" \
    | awk -v c="${2:-}" 'index($1, "mimic_http_requests_total{") == 1 && index($0, "direction=\"inbound\"") \
        && (c == "" || index($0, "status_class=\"" c "\"")) { sum += $NF } END { print sum + 0 }'
}

# admission_refusals <pod> <since>: the duration_ms of the refusal lines of the pod's sidecar since the RFC 3339 time,
# grouped by reason: {"overloaded": [...], "queue-timeout": [...]}. Read per run: kubelet rotates the log at about 10 MiB.
admission_refusals() {
  kubectl -n "$NS" logs "$1" -c mimic-sidecar --since-time="$2" 2>>"$WORK/logs.log" \
    | jq -R -c 'fromjson? | select(.target == "access" and .direction == "inbound" and .reason != null)
        | {reason, ms: .duration_ms}' \
    | jq -s -c '{overloaded: map(select(.reason == "overloaded") | .ms),
                 "queue-timeout": map(select(.reason == "queue-timeout") | .ms)}'
}

# breaker_config: the MimicConfig of mesh-breaker (circuitBreaker.maxRequests on the backend Service), then waits, up
# to 60 s, until it is Accepted and the client sidecar has pulled a view after it, and a little more for the apply.
breaker_config() {
  local before now attempt caller
  caller=$(pod_of fortio-client)
  before=$(pulls_ok "$caller")
  kubectl apply -f - >/dev/null <<YAML
apiVersion: mimic.platform.io/v1alpha1
kind: MimicConfig
metadata: {name: fortio-breaker, namespace: $NS}
spec:
  targetRefs: [{group: "", kind: Service, name: fortio-server}]
  circuitBreaker: {maxRequests: $BREAKER_MAX_REQUESTS}
YAML
  for attempt in $(seq 60); do
    now=$(pulls_ok "$caller")
    if ((now > before)) && [[ $(kubectl -n "$NS" get mimicconfig fortio-breaker \
      -o jsonpath='{.status.conditions[?(@.type=="Accepted")].status}') == True ]]; then
      sleep 5
      return 0
    fi
    sleep 1
  done
  echo "FAIL mesh-breaker: the MimicConfig is not Accepted or the client sidecar pulled no view after it in 60 s" >&2
  exit 1
}

# pulls_ok <pod>: the views the sidecar of the pod pulled and applied (mimic_mesh_view_pulls_total{result="ok"}).
pulls_ok() {
  kubectl get --raw "/api/v1/namespaces/$NS/pods/$1:15100/proxy/metrics" \
    | awk 'index($1, "mimic_mesh_view_pulls_total{") == 1 && index($0, "result=\"ok\"") { sum += $NF } END { print sum + 0 }'
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
  local setup=$1 phase=$2 strict=$3 caller metrics pod addr n fallbacks reconnects retries refused ejected count attempt ok out want
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
    ejected=$(awk 'index($1, "mimic_mesh_ejections_total{") == 1 { sum += $NF } END { print sum + 0 }' <<<"$metrics")
    if [[ $strict == 1 ]] && ((fallbacks != 0)); then ok=0; fi
    if [[ $setup == mesh-refuse ]] && ((refused == 0)); then ok=0; fi
    # In mesh-eject the resetting pod must have been ejected: its resets are counted by the outlier detection.
    if [[ $setup == mesh-eject ]] && ((ejected == 0)); then ok=0; fi
    if ((ok)); then
      jq -n --arg phase "$phase" --argjson strict "$strict" --argjson fallbacks "$fallbacks" \
        --argjson reconnects "$reconnects" --argjson retries "$retries" --argjson refused "$refused" \
        --argjson ejected "$ejected" \
        --argjson pods "$(jq -s . "$WORK/routing.rows")" \
        '{phase: $phase, strict: ($strict == 1), pods: $pods, fallbacks: $fallbacks, reconnects: $reconnects,
          retries: $retries, connect_failed_attempts: $refused, ejections: $ejected}' \
        >"$out"
      echo "routing $setup $phase ok: requests per pod $(jq -r '[.pods[].requests] | join(" ")' "$out"), fallbacks $fallbacks, retries $retries, connect_failed attempts $refused, ejections $ejected"
      return 0
    fi
    echo "routing $setup $phase not yet (attempt $attempt): $(jq -c -s . "$WORK/routing.rows"), fallbacks $fallbacks, retries $retries, connect_failed attempts $refused, ejections $ejected"
    sleep 10
  done
  echo "FAIL mesh routing $setup $phase: the client sidecar does not route to $want injected backend pods (and, in mesh-refuse, tries the refusing pod, in mesh-eject ejects the resetting pod), see above" >&2
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

# primary_requests: the requests the client sidecar routed to fortio-server, all of them, fortio's warm-up
# requests (one per connection, not in its count) included.
primary_requests() {
  kubectl get --raw "/api/v1/namespaces/$NS/pods/$(pod_of fortio-client):15100/proxy/metrics" \
    | awk 'index($1, "mimic_mesh_requests_total{") == 1 && index($0, "fortio-server") { sum += $NF } END { print sum + 0 }'
}

# route_requests: the requests the client sidecar routed through a route rule (the series whose route label is not none).
route_requests() {
  kubectl get --raw "/api/v1/namespaces/$NS/pods/$(pod_of fortio-client):15100/proxy/metrics" \
    | awk 'index($1, "mimic_mesh_requests_total{") == 1 && index($0, "route=\"") && !index($0, "route=\"none\"") { sum += $NF } END { print sum + 0 }'
}

# mirrored_sent: the copies the client sidecar counted sent, summed over the mirror Services (mesh-mirror).
mirrored_sent() {
  kubectl get --raw "/api/v1/namespaces/$NS/pods/$(pod_of fortio-client):15100/proxy/metrics" \
    | awk 'index($1, "mimic_mesh_mirrored_total{") == 1 && index($0, "result=\"sent\"") { sum += $NF } END { print sum + 0 }'
}

# mirror_route <none|route|on>: the HTTPRoute of mesh-mirror: deleted (none), every request of fortio-server sent
# to its backend (route), or also mirrored to fortio-mirror (on); then waits, up to 120 s, until the client
# sidecar's view agrees, read from its counters after warm-ups: none, no request through a route rule and no copy
# in two warm-ups in a row; route, requests through the rule and no copy; on, copies.
mirror_route() {
  local state=$1 sent_before sent_after routed_before routed_after sent_again routed_again attempt filters=""
  if [[ $state == on ]]; then
    filters='
      filters:
        - type: RequestMirror
          requestMirror: {backendRef: {name: fortio-mirror, port: 8080}}'
  fi
  if [[ $state == none ]]; then
    kubectl -n "$NS" delete httproute fortio-mirror --ignore-not-found >/dev/null
  else
    kubectl apply -f - >/dev/null <<YAML
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: fortio-mirror, namespace: $NS}
spec:
  parentRefs: [{group: "", kind: Service, name: fortio-server, port: 8080}]
  rules:
    - backendRefs: [{name: fortio-server, port: 8080}]$filters
YAML
  fi
  for attempt in $(seq 24); do
    sent_before=$(mirrored_sent)
    routed_before=$(route_requests)
    warm_up
    sleep 2
    sent_after=$(mirrored_sent)
    routed_after=$(route_requests)
    case $state in
      on) if ((sent_after > sent_before)); then return 0; fi ;;
      *)
        warm_up
        sleep 2
        sent_again=$(mirrored_sent)
        routed_again=$(route_requests)
        if ((sent_after == sent_before && sent_again == sent_after)); then
          if [[ $state == none ]] && ((routed_after == routed_before && routed_again == routed_after)); then return 0; fi
          if [[ $state == route ]] && ((routed_after > routed_before && routed_again > routed_after)); then return 0; fi
        fi
        ;;
    esac
    sleep 3
  done
  echo "FAIL mirror route $state: the client sidecar's view did not follow in 120 s" >&2
  exit 1
}

# mirror_settle <sent before> <primary requests>: waits up to 30 s for the copies of a run with the mirror and
# prints the count. A count other than the primary requests the sidecar routed (RTE-94) is reported on stderr and recorded in the results
# (`copies_match`); run_one sets MIRROR_MISMATCH and the verdict of the setup fails at its end, not the whole run.
mirror_settle() {
  local before=$1 requests=$2 sent deadline=$((SECONDS + 30))
  while :; do
    sent=$(($(mirrored_sent) - before))
    if ((sent >= requests || SECONDS >= deadline)); then break; fi
    sleep 1
  done
  if ((sent != requests)); then
    echo "FAIL mesh-mirror: the mirror counted $sent sent copies for $requests primary requests, as many expected" >&2
  fi
  echo "$sent"
}

# mesh_summary <setup>: the mesh block of the results of a mesh setup, in $WORK/<setup>.mesh.json (BEN-16).
mesh_summary() {
  local setup=$1
  jq -n --arg setup "$setup" --argjson pods "$MESH_PODS" --argjson refusing "$REFUSING_PODS" \
    --argjson budget "$MIRROR_P50_BUDGET_MS" \
    --slurpfile before "$WORK/$setup.routing-before.json" --slurpfile after "$WORK/$setup.routing-after.json" \
    --slurpfile runs "$WORK/$setup.jsonl" '
    {backend_pods: $pods, routing: {before: $before[0], after: $after[0]}}
    + (if $setup == "mesh-churn" then
         {churn: {restarts: ($runs | length), errors_total: ($runs | map(.errors) | add // 0),
                  fallbacks_total: $after[0].fallbacks, reconnects_total: $after[0].reconnects}}
       else {} end)
    + (if $setup == "mesh-mirror" then
         {mirror: {budget_p50_ms: $budget,
                   pairs: ($runs | group_by([.protocol, .rate]) | map(
                     (map(select(.mirror.enabled == false and .mirror.route == false))[0]) as $none
                     | (map(select(.mirror.enabled == false and .mirror.route == true))[0]) as $off
                     | (map(select(.mirror.enabled == true))[0]) as $on
                     | {protocol: $on.protocol, rate: $on.rate, requests: $on.requests,
                        copies_sent: $on.mirror.copies_sent,
                        p50_no_route_ms: $none.latency_ms.p50, p50_without_ms: $off.latency_ms.p50,
                        p50_with_ms: $on.latency_ms.p50,
                        route_delta_p50_ms: ((($off.latency_ms.p50 - $none.latency_ms.p50) * 1000 | round) / 1000),
                        delta_p50_ms: ((($on.latency_ms.p50 - $off.latency_ms.p50) * 1000 | round) / 1000),
                        within_budget: (($on.latency_ms.p50 - $off.latency_ms.p50) <= $budget),
                        primary_requests: $on.mirror.primary_requests,
                        copies_match: ($on.mirror.copies_sent == $on.mirror.primary_requests)}))}}
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

# eject_summary: the mesh-eject block, in $WORK/mesh-eject.extra.json (CBR-34): the requests the resetting pod
# received (its own count, read through the API server) against the ejections of the caller sidecar, and the error
# rate of the runs. Both budgets are in the block: no error, at most eject_bound requests per ejection.
eject_summary() {
  local pod received ejections
  pod=$(running_pods fortio-reset | head -n 1)
  received=$(kubectl get --raw "/api/v1/namespaces/$NS/pods/$pod:8081/proxy/")
  ejections=$(jq -r '.ejections' "$WORK/mesh-eject.routing-after.json")
  jq -n --argjson received "$received" --argjson ejections "$ejections" --argjson per "$(eject_bound)" --argjson fails "$EJECT_PER_EJECTION" --argjson conns "$CONNECTIONS" \
    --slurpfile runs "$WORK/mesh-eject.jsonl" '
    ($runs | map(.requests) | add // 0) as $requests | ($runs | map(.errors) | add // 0) as $errors
    | {requests_total: $requests, errors_total: $errors,
       error_rate_percent: (if $requests == 0 then 0 else (($errors / $requests) * 100000 | round) / 1000 end),
       resetting_pod_requests: $received, ejections: $ejections, consecutive_failures: $fails, connections: $conns, budget_per_ejection: $per,
       requests_per_ejection: (if $ejections == 0 then null else (($received / $ejections) * 100 | round) / 100 end),
       within_budget: ($errors == 0 and $ejections >= 1 and $received <= $per * $ejections)}' \
    >"$WORK/mesh-eject.extra.json"
  echo "mesh-eject: $received requests reached the resetting pod for $ejections ejections ($(jq -r '.requests_per_ejection' "$WORK/mesh-eject.extra.json") per ejection, bound $(eject_bound)), $(jq -r '.errors_total' "$WORK/mesh-eject.extra.json") errors"
}

# breaker_summary: the mesh-breaker block, in $WORK/mesh-breaker.extra.json (CBR-34): the access lines of the client
# sidecar with reason breaker-open, their count and the largest duration_ms, against BREAKER_OPEN_BUDGET_MS, and the
# errors of the runs (the answers that are not 200, the breaker-open ones among them).
breaker_summary() {
  local caller lines
  caller=$(pod_of fortio-client)
  lines=$(kubectl -n "$NS" logs "$caller" -c mimic-sidecar --tail=-1 2>>"$WORK/logs.log" \
    | jq -R -c 'fromjson? | select(.target == "access" and .reason == "breaker-open") | .duration_ms' | jq -s -c .)
  jq -n --argjson durations "$lines" --argjson budget "$BREAKER_OPEN_BUDGET_MS" --argjson max "$BREAKER_MAX_REQUESTS" \
    --argjson minpct "$BREAKER_OPEN_MIN_PERCENT" --argjson conns "$BREAKER_CONNECTIONS" --slurpfile runs "$WORK/mesh-breaker.jsonl" '
    ($runs | map(.requests) | add // 0) as $requests
    | {max_requests: $max, connections: $conns, requests_total: $requests, breaker_open_lines: ($durations | length),
     breaker_open_percent: (if $requests == 0 then 0 else (($durations | length) / $requests * 10000 | round) / 100 end),
     min_breaker_open_percent: $minpct, duration_ms_max: ($durations | max),
     duration_ms_counts: ($durations | group_by(.) | map({ms: .[0], n: length})),
     duration_ms_p99: (if ($durations | length) == 0 then null else ($durations | sort | .[((length * 99 / 100) | floor)]) end),
     budget_duration_ms: $budget, errors_total: ($runs | map(.errors) | add // 0),
     within_budget: ($requests > 0 and ($durations | length) * 100 >= $minpct * $requests and ($durations | sort | .[((length * 99 / 100) | floor)]) <= $budget)}' \
    >"$WORK/mesh-breaker.extra.json"
  echo "mesh-breaker: $(jq -r '.breaker_open_lines' "$WORK/mesh-breaker.extra.json") breaker-open lines ($(jq -r '.breaker_open_percent' "$WORK/mesh-breaker.extra.json") % of the requests, minimum $BREAKER_OPEN_MIN_PERCENT %), longest $(jq -r '.duration_ms_max' "$WORK/mesh-breaker.extra.json") ms"
}

# admission_summary: the mesh-admission block, in $WORK/mesh-admission.extra.json (ADM-32). pairs: the p99 under the
# contract that never binds against the run without any, at the same protocol and rate, and its errors (a refusal is
# one). over: per protocol, the load sent against twice the contract, the rate admitted by each backend pod (its 2xx
# over fortio's ActualDuration), the duration_ms of the overloaded lines (p99) and of the queue-timeout lines (the
# largest, and every value counted so that one outlier shows). Each verdict, and the whole one, is within_budget.
admission_summary() {
  jq -n --argjson p99 "$ADMISSION_P99_BUDGET_MS" --argjson refusal "$ADMISSION_REFUSAL_BUDGET_MS" \
    --argjson queue "$ADMISSION_QUEUE_BUDGET_MS" --argjson pct "$ADMISSION_RATE_PERCENT" --argjson rps "$ADMISSION_RPS" \
    --slurpfile runs "$WORK/mesh-admission.jsonl" '
    def r2: (. * 100 | round) / 100;
    def abs: if . < 0 then -. else . end;
    def p99: if length == 0 then null else (sort | .[((length * 99 / 100) | floor)]) end;
    ($runs | map(select(.admission.variant != "adm-over")) | group_by([.protocol, .rate]) | map(
        (map(select(.admission.variant == "adm-off"))[0]) as $off
        | (map(select(.admission.variant == "adm-under"))[0]) as $under
        | ($under.latency_ms.p99 - $off.latency_ms.p99) as $delta
        | {protocol: $under.protocol, rate: $under.rate, p99_off_ms: $off.latency_ms.p99,
           p99_under_ms: $under.latency_ms.p99, delta_p99_ms: ($delta * 1000 | round / 1000),
           errors_under: $under.errors, within_budget: ($delta <= $p99 and $under.errors == 0)})) as $pairs
    | ($runs | map(select(.admission.variant == "adm-over")) | map(
        (.fortio.ActualDuration / 1e9) as $secs
        | [.admission.pods[].refusals.overloaded[]] as $overloaded
        | [.admission.pods[].refusals["queue-timeout"][]] as $timeouts
        | (.admission.pods | map((.ok / $secs) as $admitted
            | {pod, offered_rps: (.requests / $secs | r2), admitted_rps: ($admitted | r2),
               within_budget: (($admitted - $rps | abs) <= $rps * $pct / 100)})) as $pods
        | {protocol, rate, actual_qps, contract: .admission.contract, pods: $pods,
           overloaded_lines: ($overloaded | length), overloaded_p99_ms: ($overloaded | p99),
           queue_timeout_lines: ($timeouts | length), queue_timeout_max_ms: ($timeouts | max),
           queue_timeout_ms_counts: ($timeouts | group_by(.) | map({ms: .[0], n: length})),
           within_budget: (.actual_qps >= 0.95 * .rate and ($pods | length) > 0 and ($pods | all(.within_budget))
             and ($overloaded | length) >= 1 and ($timeouts | length) >= 1
             and ($overloaded | p99) <= $refusal and ($timeouts | max) <= $queue)})) as $over
    | {budgets: {added_p99_ms: $p99, refusal_p99_ms: $refusal, queue_timeout_max_ms: $queue, rate_percent: $pct},
       pairs: $pairs, over: $over,
       within_budget: (($pairs | length) > 0 and ($over | length) > 0 and (($pairs + $over) | all(.within_budget)))}' \
    >"$WORK/mesh-admission.extra.json"
  jq -r '(.pairs[] | "mesh-admission pair \(.protocol) \(.rate) rps: p99 \(.p99_off_ms) ms without, \(.p99_under_ms) ms under the contract (+\(.delta_p99_ms) ms), \(.errors_under) errors"),
    (.over[] | "mesh-admission over \(.protocol) \(.rate) rps: \(.actual_qps) qps sent, admitted \([.pods[].admitted_rps] | join(" ")) rps per pod, overloaded p99 \(.overloaded_p99_ms) ms (\(.overloaded_lines) lines), queue-timeout max \(.queue_timeout_max_ms) ms (\(.queue_timeout_lines) lines)")' \
    "$WORK/mesh-admission.extra.json"
}

# run_one <setup> <protocol> <rate> <index> <total> <variant>: one fortio run, sidecars measured before and after
# (BEN-03). The variant is none, route or on for mesh-mirror, adm-off, adm-under or adm-over for mesh-admission, - for
# the others. An adm-over run also records, per backend pod, its inbound requests and their 2xx, and its refusal lines.
run_one() {
  local setup=$1 protocol=$2 rate=$3 index=$4 total=$5 variant=$6
  local mirror_json=null admission_json=null sent_before=0 primary_before=0 started pod i
  local caller callee raw cb ca cr callee_before callee_after callee_rss caller_json=null callee_json=null
  local measure_callee=1 churn=0
  local -a backend requests_before ok_before
  fortio_args "$protocol" "$rate" "$setup" "$variant"
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
  echo "== run $index/$total setup $setup protocol $protocol rate $rate rps$(variant_label "$variant")"
  if [[ $variant == on ]]; then sent_before=$(mirrored_sent); primary_before=$(primary_requests); fi
  if [[ $variant == adm-over ]]; then
    mapfile -t backend < <(running_pods fortio-server)
    requests_before=() ok_before=()
    for pod in "${backend[@]}"; do
      requests_before+=("$(inbound_requests "$pod")")
      ok_before+=("$(inbound_requests "$pod" 2xx)")
    done
    started=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  fi
  # mesh-churn: a rolling restart of the backend starts with the load and runs through it (BEN-14).
  if ((churn)); then kubectl -n "$NS" rollout restart deploy/fortio-server >/dev/null; fi
  raw=$(kubectl -n "$NS" exec "$caller" -c fortio -- /usr/bin/fortio "${FORTIO_ARGS[@]}" 2>>"$WORK/fortio.log") \
    || { echo "FAIL fortio run $index failed, see $WORK/fortio.log" >&2; exit 1; }
  if ((churn)); then kubectl -n "$NS" rollout status deploy/fortio-server --timeout=180s >/dev/null; fi
  jq -e '.DurationHistogram.Count' <<<"$raw" >/dev/null || { echo "FAIL run $index printed no fortio JSON" >&2; exit 1; }
  if [[ $setup == mesh-admission ]]; then
    admission_json=$(jq -nc --arg variant "$variant" --argjson contract "$(admission_contract "$variant")" \
      '{variant: $variant, contract: $contract}')
  fi
  if [[ $variant == adm-over ]]; then
    : >"$WORK/admission.pods"
    for i in "${!backend[@]}"; do
      pod=${backend[$i]}
      jq -nc --arg pod "$pod" --argjson requests "$(($(inbound_requests "$pod") - requests_before[i]))" \
        --argjson ok "$(($(inbound_requests "$pod" 2xx) - ok_before[i]))" \
        --argjson refusals "$(admission_refusals "$pod" "$started")" \
        '{pod: $pod, requests: $requests, ok: $ok, refusals: $refusals}' >>"$WORK/admission.pods"
    done
    admission_json=$(jq -c --slurpfile pods "$WORK/admission.pods" '. + {pods: $pods}' <<<"$admission_json")
  fi
  if [[ $variant == none ]]; then mirror_json='{"enabled": false, "route": false}'; fi
  if [[ $variant == route ]]; then mirror_json='{"enabled": false, "route": true}'; fi
  if [[ $variant == on ]]; then
    # The primary requests are the sidecar's own count, which includes fortio's warm-up requests (one per
    # connection, not in fortio's count): each of them is mirrored too.
    local primary copies
    primary=$(($(primary_requests) - primary_before))
    copies=$(mirror_settle "$sent_before" "$primary")
    if ((copies != primary)); then MIRROR_MISMATCH=1; fi
    mirror_json=$(jq -nc --argjson sent "$copies" --argjson primary "$primary" \
      '{enabled: true, route: true, copies_sent: $sent, primary_requests: $primary}')
  fi
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
    --argjson connections "$(setup_connections "$setup" "$variant")" --argjson duration_s "$DURATION_S" --argjson fortio "$raw" \
    --argjson caller "$caller_json" --argjson callee "$callee_json" --argjson churn "$churn" \
    --argjson mirror "$mirror_json" --argjson admission "$admission_json" '
    def pct($p): [$fortio.DurationHistogram.Percentiles[] | select(.Percentile == $p) | .Value * 1000][0];
    {
      setup: $setup, protocol: $protocol, rate: $rate, duration_s: $duration_s, connections: $connections,
      requests: $fortio.DurationHistogram.Count,
      actual_qps: ($fortio.ActualQPS * 100 | round) / 100,
      errors: ($fortio.DurationHistogram.Count - ($fortio.RetCodes["200"] // 0)),
      latency_ms: {p50: pct(50), p90: pct(90), p99: pct(99), "p99.9": pct(99.9)},
      rollout_restart: ($churn == 1),
      mirror: $mirror,
      admission: $admission,
      sidecar: {caller: $caller, callee: $callee},
      fortio: $fortio
    }' >>"$WORK/$setup.jsonl"
}

# write_results <setup>: the JSON file of a setup and one table row per run in the README (BEN-07).
write_results() {
  local setup=$1 file="$OUT_DIR/$TODAY-$1.json" mesh=null
  if [[ -f $WORK/$setup.mesh.json ]]; then mesh=$(cat "$WORK/$setup.mesh.json"); fi
  jq -s --arg date "$TODAY" --arg setup "$setup" --argjson machine "$MACHINE" \
    --arg duration "$DURATION" --argjson connections "$(setup_connections "$setup")" --argjson mesh "$mesh" \
    '{date: $date, setup: $setup, machine: $machine, duration: $duration, connections: $connections, mesh: $mesh, runs: .}' \
    "$WORK/$setup.jsonl" >"$file"
  jq -r --arg date "$TODAY" --arg setup "$setup" '
    def v: if . == null then "-" else tostring end;
    def r2: if . == null then "-" else (. * 100 | round / 100 | tostring) end;
    def admission: if .admission == null then ""
      elif .admission.contract == null then " (no admission)"
      else " (admission \(.admission.contract.concurrency)/\(.admission.contract.rps)\(if .admission.variant == "adm-over" then ", 2x" else "" end))" end;
    "| \($date) | \($setup)\(if .mirror.enabled == false then (if .mirror.route then " (route, no mirror)" else " (no route)" end) else "" end)\(admission) | \(.protocol) | \(.rate) | \(.fortio.ActualQPS | r2) | \(.latency_ms.p50 | r2) | \(.latency_ms.p90 | r2) | \(.latency_ms.p99 | r2) | \(.latency_ms["p99.9"] | r2) | \(.errors) | \(.sidecar.caller.cpu_cores | v) | \(.sidecar.callee.cpu_cores | v) | \(.sidecar.caller.rss_mib | v) | \(.sidecar.callee.rss_mib | v) |"' \
    "$WORK/$setup.jsonl" >>"$OUT_DIR/README.md"
  echo "wrote $file"
}

total=$(run_total)
index=0
for setup in "${ORDERED_SETUPS[@]}"; do
  prepare_setup "$setup"
  if [[ $setup == mesh-* ]]; then mesh_routing "$setup" before 1; fi
  # Read first: a command of a run must not read the list on its stdin.
  mapfile -t runs < <(runs_of "$setup")
  for run in "${runs[@]}"; do
    read -r protocol rate variant <<<"$run"
    index=$((index + 1))
    case $variant in
      none | route | on) mirror_route "$variant" ;;
      adm-*) admission_config "$variant" ;;
    esac
    run_one "$setup" "$protocol" "$rate" "$index" "$total" "$variant"
    if ((index < total)); then sleep "$PAUSE"; fi
  done
  if [[ $setup == mesh-mirror ]]; then mirror_route none; fi
  if [[ $setup == mesh-admission ]]; then admission_config adm-off; fi
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
  if [[ $setup == mesh-eject ]]; then
    eject_summary
    jq -n --slurpfile m "$WORK/$setup.mesh.json" --slurpfile e "$WORK/$setup.extra.json" '$m[0] + {eject: $e[0]}' \
      >"$WORK/$setup.mesh.json.tmp"
    mv "$WORK/$setup.mesh.json.tmp" "$WORK/$setup.mesh.json"
    if [[ $(jq -r '.within_budget' "$WORK/$setup.extra.json") != true ]]; then
      echo "FAIL mesh-eject: the error rate is not 0 % or the resetting pod received more than $(eject_bound) requests per ejection (within_budget false in the results)" >&2
      FAILED_SETUPS+=("$setup")
    fi
  fi
  if [[ $setup == mesh-breaker ]]; then
    breaker_summary
    jq -n --slurpfile m "$WORK/$setup.mesh.json" --slurpfile e "$WORK/$setup.extra.json" '$m[0] + {breaker: $e[0]}' \
      >"$WORK/$setup.mesh.json.tmp"
    mv "$WORK/$setup.mesh.json.tmp" "$WORK/$setup.mesh.json"
    if [[ $(jq -r '.within_budget' "$WORK/$setup.extra.json") != true ]]; then
      echo "FAIL mesh-breaker: fewer than $BREAKER_OPEN_MIN_PERCENT % of the requests answered breaker-open, or their p99 duration_ms is over $BREAKER_OPEN_BUDGET_MS ms (within_budget false in the results)" >&2
      FAILED_SETUPS+=("$setup")
    fi
    kubectl -n "$NS" delete mimicconfig fortio-breaker --ignore-not-found >/dev/null
  fi
  if [[ $setup == mesh-admission ]]; then
    admission_summary
    jq -n --slurpfile m "$WORK/$setup.mesh.json" --slurpfile e "$WORK/$setup.extra.json" '$m[0] + {admission: $e[0]}' \
      >"$WORK/$setup.mesh.json.tmp"
    mv "$WORK/$setup.mesh.json.tmp" "$WORK/$setup.mesh.json"
    if [[ $(jq -r '.within_budget' "$WORK/$setup.extra.json") != true ]]; then
      echo "FAIL mesh-admission: a pair is over +$ADMISSION_P99_BUDGET_MS ms of p99 or has errors under the contract, or an over run sent under 95 % of $ADMISSION_OVER_RATE rps, admitted a pod outside $ADMISSION_RATE_PERCENT % of $ADMISSION_RPS rps, or wrote refusals over $ADMISSION_REFUSAL_BUDGET_MS ms (overloaded p99) or $ADMISSION_QUEUE_BUDGET_MS ms (queue-timeout) (within_budget false in the results)" >&2
      FAILED_SETUPS+=("$setup")
    fi
  fi
  write_results "$setup"
  if ((MIRROR_MISMATCH)); then
    echo "FAIL $setup: the copies counted sent differ from the primary requests (copies_match false in the results)" >&2
    FAILED_SETUPS+=("$setup")
    MIRROR_MISMATCH=0
  fi
done
echo "done: $total runs"
if ((${#FAILED_SETUPS[@]} > 0)); then
  echo "FAIL setups: ${FAILED_SETUPS[*]}" >&2
  exit 1
fi
