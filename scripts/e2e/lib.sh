# Helpers shared by scripts/e2e.sh, scripts/e2e-load.sh and scripts/e2e-autoscaling.sh, sourced from the repository root.
# The cluster and image helpers read CLUSTER, NODE_IMAGE, SKIP_BUILD, IMAGE and INIT_IMAGE and set BUILT; the caller defines log().
# shellcheck shell=bash disable=SC2329,SC2034 # the helpers are called by the scripts that source this file

# sidecar_scrape <namespace> <pod>: the Prometheus text of the pod's sidecar, read from its admin port over loopback
# (not captured) through the app container's Python. <pod> is anything kubectl exec takes (deploy/app too). Non-zero on
# any failure.
sidecar_scrape() {
  kubectl -n "$1" exec "$2" -c app -- python3 -c \
    'import sys, urllib.request; sys.stdout.write(urllib.request.urlopen("http://127.0.0.1:15100/metrics", timeout=5).read().decode())'
}

# series <file> <series>: the value of the exact series (name and labels, or a bare name), empty when absent. Never a
# prefix match.
series() { awk -v s="$2" '$1 == s { print $2; exit }' "$1"; }

# live_pods <namespace> <selector>: the names of the Running pods of the selector that are not being deleted, one per line.
live_pods() {
  kubectl -n "$1" get pods -l "$2" -o json \
    | jq -r '.items[] | select(.metadata.deletionTimestamp == null and .status.phase == "Running") | .metadata.name'
}

# e2e_cluster_up: creates the kind cluster $CLUSTER unless it exists (with --image $NODE_IMAGE when that is set), then makes
# it the current context.
e2e_cluster_up() {
  local clusters status=0
  clusters=$(kind get clusters) || { echo "FAIL kind get clusters failed" >&2; exit 1; }
  grep -qx "$CLUSTER" <<<"$clusters" || status=$?
  if (( status > 1 )); then
    echo "FAIL grep exited with status $status" >&2
    exit 1
  fi
  if (( status == 1 )); then
    log "create kind cluster $CLUSTER"
    local node_args=()
    [[ -z ${NODE_IMAGE:-} ]] || node_args=(--image "$NODE_IMAGE")
    kind create cluster --name "$CLUSTER" --wait 120s "${node_args[@]}"
  fi
  kubectl config use-context "kind-$CLUSTER" >/dev/null
}

# e2e_load_images: builds $IMAGE and $INIT_IMAGE unless SKIP_BUILD=1 (then sets BUILT=1, for the script's cleanup), and
# loads both into the cluster.
e2e_load_images() {
  if [[ ${SKIP_BUILD:-0} != 1 ]]; then
    log "build $IMAGE and $INIT_IMAGE"
    docker build -q -t "$IMAGE" .
    docker build -q -t "$INIT_IMAGE" --target init .
    BUILT=1
  fi
  kind load docker-image --name "$CLUSTER" "$IMAGE" "$INIT_IMAGE"
}

# wait_for <seconds> <command...>: the command succeeds within the time. Its errors are no failure while it polls.
wait_for() {
  local end=$((SECONDS + $1))
  shift
  until "$@" 2>/dev/null; do
    (( SECONDS < end )) || return 1
    sleep 2
  done
}
