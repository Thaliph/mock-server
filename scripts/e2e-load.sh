#!/usr/bin/env bash
# End-to-end checks of the load test on kind (LDT-06, LDT-17, LDT-16, LDT-19 E; spec §7.8, §11, §12.12).
# Idempotent: reuses the cluster if it exists and upgrades the release mimic in place. It
# shares the cluster and the release with scripts/e2e.sh: never run the two at the same time.
#
#   scripts/e2e-load.sh            LDT-06 and LDT-17 on a cluster of its own (builds and installs)
#   scripts/e2e-load.sh --suite    LDT-16 and LDT-19 (E), called by scripts/e2e.sh after its F10 section: it needs
#                                  the MCK-19 setup of e2e.sh (front in mck-19, users in disc-22, the provider in
#                                  mck-19-provider, all in mock or discovery) and the cluster and images of that run
#
#   TAG=e2e-load-<sha>-<pid>                    tag of the images built here (unique per run)
#   IMAGE=ghcr.io/thaliph/mimic:$TAG            image to test (built here unless SKIP_BUILD=1; --suite: the one e2e.sh loaded)
#   INIT_IMAGE=ghcr.io/thaliph/mimic-init:$TAG  mimic-init image (built here unless SKIP_BUILD=1)
#   CLUSTER=mimic-e2e                           kind cluster name
#   NODE_IMAGE=kindest/node:v1.33.12@sha256:…    kind node image (default: kind's own)
#   KEEP_CLUSTER=1                              keep the cluster afterwards (default: keep locally, delete in CI; --suite: kept)
#
# The chart needs image.digest with loadTest.enabled: the digest is the one the node's containerd
# holds for IMAGE, read with crictl (or ctr, see image_digest). The Job and NetworkPolicy checked below are the objects that
# mimic_core::load::job builds (examples/load_objects.rs), created as the controller account with a
# server-side dry run, so the chart's ValidatingAdmissionPolicy is what decides.
set -Eeuo pipefail
# A command that fails outside a condition ends the run: name it, so that no exit is silent.
trap 'e2e_status=$?; echo "FAIL line $LINENO: \"$BASH_COMMAND\" exited with status $e2e_status" >&2' ERR
cd "$(dirname "$0")/.."

SUITE=0
case ${1:-} in
  "") ;;
  --suite) SUITE=1 ;;
  *) echo "usage: $0 [--suite]" >&2; exit 2 ;;
esac
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
# --suite leaves the cluster to scripts/e2e.sh.
(( SUITE == 0 )) || KEEP_CLUSTER=1
BUILT=0
UI_PF=""
CHROME=""
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
  [[ -z $UI_PF ]] || kill "$UI_PF" 2>/dev/null || true
  [[ -z $CHROME ]] || docker rm -f "$CHROME" >/dev/null 2>&1 || true
  rm -rf "$WORK"
  [[ $KEEP_CLUSTER == 1 ]] || kind delete cluster --name "$CLUSTER"
}
trap cleanup EXIT

# image_digest <tag>: the manifest digest that the node's containerd holds for the tag: the repo digest crictl lists
# for it, or, when the node lists none (seen on kindest/node v1.37 in CI, images from the artifact), the target
# digest of the tag in containerd, which is the same image.
image_digest() {
  local digest
  digest=$(docker exec "$CLUSTER-control-plane" crictl images --digests -o json \
    | jq -r --arg tag "$1" '.images[] | select(.repoTags | index($tag)) | .repoDigests[]' \
    | sed -n 's/.*@\(sha256:[0-9a-f]\{64\}\)$/\1/p' \
    | head -n 1)
  if [[ -z $digest ]]; then
    digest=$(docker exec "$CLUSTER-control-plane" ctr -n k8s.io images ls \
      | awk -v ref="$1" '$1 == ref { print $3; exit }')
  fi
  printf '%s\n' "$digest"
}

# --- --suite: LDT-16 and LDT-19 (E), spec §12.12 -----------------------------------------------------------
# scripts/e2e.sh calls this after its F10 section, before that section removes its namespaces. There mck-19 holds front
# (in discovery, 3 pods, injected), disc-22 users (discovery) and mck-19-provider. The run below switches front to mock
# (spec.mode), so the names and the numbers of this part are those of e2e.sh's MCK-19 setup. The release is upgraded with
# loadTest.enabled; the values of e2e.sh are reused, and its later installs do not reuse values, so loadTest goes off again.
PSA_NS=ldt-16-psa
UI_PORT=18083
CHROME_PORT=19224
# Set by suite_main: the Secret's canary (never printed), the digest of IMAGE and the run objects' names.
canary=""
digest=""
users0=""; prov0=""; exch0=""
# The start timeout of a run (MIMIC_LOAD_START_TIMEOUT, 300 s by default) and the margin that the Pending re-check of run C keeps
# from it; c_t0 is the SECONDS of its creation.
START_TIMEOUT=300
START_MARGIN=60
c_t0=0

# wait_for <seconds> <command...>: the command succeeds within the time. Its errors are no failure while it polls.
wait_for() {
  local end=$((SECONDS + $1))
  shift
  until "$@" 2>/dev/null; do
    (( SECONDS < end )) || return 1
    sleep 2
  done
}
# exists <kubectl get arguments>: the object is there.
exists() { kubectl get "$@" -o name >/dev/null 2>&1; }
# absent <kubectl get arguments>: the object is NotFound; any other error is not an absence.
absent() {
  local out
  if out=$(kubectl get "$@" -o name 2>&1); then return 1; fi
  [[ $out == *NotFound* ]]
}
# run_status <ns> <name>: phase/reason of a MimicLoadTest.
run_status() { kubectl -n "$1" get mimicloadtest "$2" -o jsonpath='{.status.phase}/{.status.reason}' 2>/dev/null; }
# run_is <ns> <name> <pattern>: the status of the run matches the glob pattern (Running/*, Aborted/ModeLeft).
run_is() {
  local s
  s=$(run_status "$1" "$2") || return 1
  # shellcheck disable=SC2053 # the pattern is a glob on purpose
  [[ $s == $3 ]]
}
# job_of <ns> <run>: the name of the run's Job and NetworkPolicy, mimic-load-<12 hex digits of the uid> (spec §7.8).
job_of() {
  local uid
  uid=$(kubectl -n "$1" get mimicloadtest "$2" -o jsonpath='{.metadata.uid}') || return 1
  uid=${uid//-/}
  echo "mimic-load-${uid:0:12}"
}
# owned_by <ns> <kind/name> <run>: the object is owned by the run.
owned_by() {
  [[ $(kubectl -n "$1" get "$2" -o jsonpath='{.metadata.ownerReferences[0].kind}/{.metadata.ownerReferences[0].name}' 2>/dev/null) \
    == "MimicLoadTest/$3" ]]
}
# calls <ns> <label> <container>: the access lines (with a method, the /users/0 probes aside) of the pods' container,
# summed. A failed read is an error: no pipe hides it.
calls() {
  local pods p log n total=0
  pods=$(kubectl -n "$1" get pods -l "$2" -o jsonpath='{.items[*].metadata.name}') || return 1
  [[ -n $pods ]] || return 1
  for p in $pods; do
    log=$(kubectl -n "$1" logs "$p" -c "$3") || return 1
    n=$(jq -R -s 'split("\n") | map(fromjson? | select(. != null and (.method? != null) and (.path? != "/users/0"))) | length' <<< "$log") || return 1
    total=$((total + n))
  done
  echo "$total"
}
# exchanges: the discovery exchanges of front (status of its MimicWorkload).
exchanges() { kubectl -n mck-19 get mimicworkload front -o jsonpath='{.status.discovery.exchanges}'; }
# front_in_mock: every pod of front is in mock (status.mode of its MimicWorkload, as e2e.sh's MCK-07 wait).
front_in_mock() {
  kubectl -n mck-19 get mimicworkload front -o jsonpath='{.status.mode}' 2>/dev/null \
    | jq -e '.mode == "mock" and .otherPods == 0' >/dev/null 2>&1
}
# run_create <ns> <name> <deployment> <credentials YAML or empty>: a run of the shape of LDT-16 (8 in flight, 50 rps,
# 60 s, rampUp 10 s).
run_create() {
  kubectl apply -f - >/dev/null <<YAML
apiVersion: mimic.platform.io/v1alpha1
kind: MimicLoadTest
metadata: {name: $2, namespace: $1}
spec:
  deployment: $3
  duration: 60s
  concurrency: 8
  rate: 50
  rampUp: 10s
$4
YAML
}
# psa_restricted <ns> <Job>: the Job's pod template is admitted as a Pod in a namespace of Pod Security restricted
# (server dry run, so the pod is not created).
psa_restricted() {
  kubectl -n "$1" get job "$2" -o json \
    | jq --arg ns "$PSA_NS" '{apiVersion: "v1", kind: "Pod", metadata: {generateName: "psa-", namespace: $ns, labels: .spec.template.metadata.labels}, spec: .spec.template.spec}' \
    > "$WORK/psa-pod.json" || return 1
  kubectl create --dry-run=server -f "$WORK/psa-pod.json" -o name >/dev/null
}
# sweep_logs: no log line of any pod of the cluster holds the canary of the credentials Secret (LDT-16, LDT-14).
sweep_logs() {
  local ns pod
  kubectl get pods -A -o jsonpath='{range .items[*]}{.metadata.namespace}{" "}{.metadata.name}{"\n"}{end}' > "$WORK/pods.txt" \
    || fail "ldt_16: cannot list the pods"
  while read -r ns pod; do
    if ! kubectl -n "$ns" logs "$pod" --all-containers --tail=-1 > "$WORK/pod.log" 2> "$WORK/pod.err"; then
      # A pod that is gone, or whose containers do not run yet, has no log to read.
      if found -e NotFound -e 'waiting to start' -e PodInitializing "$WORK/pod.err"; then continue; fi
      fail "ldt_16: cannot read the logs of pod $pod in $ns"
    fi
    if found -F -e "$canary" "$WORK/pod.log"; then
      fail "ldt_16: a canary of the credentials Secret is in a log line of pod $pod in $ns"
    fi
  done < "$WORK/pods.txt"
  # The generator pods of the runs are gone by now (the controller deletes a run's Job when it ends): their logs are
  # the snapshots taken while the runs ran (snapshot_generator_logs).
  local snap
  for snap in "$WORK"/gen-*/*.log; do
    [[ -e $snap ]] || continue
    if found -F -e "$canary" "$snap"; then
      fail "ldt_16: a canary of the credentials Secret is in the logs of a generator pod of run $(basename "$(dirname "$snap")" | sed 's/^gen-//')"
    fi
  done
}
# snapshot_generator_logs <ns> <Job> <dir>: keeps the newest logs of each pod of the Job in <dir>, one file per pod. A
# read that fails keeps the previous snapshot (a pod may go between two reads).
snapshot_generator_logs() {
  local ns=$1 job=$2 dir=$3 p
  mkdir -p "$dir"
  for p in $(kubectl -n "$ns" get pods -l "job-name=$job" -o jsonpath='{.items[*].metadata.name}' 2>/dev/null); do
    if kubectl -n "$ns" logs "$p" --all-containers --tail=-1 > "$dir/$p.tmp" 2>/dev/null; then
      mv "$dir/$p.tmp" "$dir/$p.log"
    fi
    rm -f "$dir/$p.tmp"
  done
}
# wait_run_logged <ns> <run> <Job> <pattern> <seconds> <dir>: wait_for run_is, snapshotting the generator logs every 2 s.
wait_run_logged() {
  local ns=$1 run=$2 job=$3 pattern=$4 end=$((SECONDS + $5)) dir=$6
  until run_is "$ns" "$run" "$pattern"; do
    snapshot_generator_logs "$ns" "$job" "$dir"
    (( SECONDS < end )) || return 1
    sleep 2
  done
}
# snapshot_for <ns> <Job> <dir> <seconds>: snapshots the generator logs for the given time.
snapshot_for() {
  local ns=$1 job=$2 dir=$3 end=$((SECONDS + $4))
  while (( SECONDS < end )); do
    snapshot_generator_logs "$ns" "$job" "$dir"
    sleep 2
  done
}
# ui_forward_start <label>: a port-forward to the read API of a controller pod on UI_PORT, unless one is up.
ui_forward_start() {
  local pod
  [[ -z $UI_PF ]] || return 0
  pod=$(kubectl -n "$NS" get pods -l app.kubernetes.io/name=mimic-controller --field-selector=status.phase=Running \
    -o jsonpath='{.items[0].metadata.name}') || fail "$1: cannot list the controller pods"
  [[ -n $pod ]] || fail "$1: no running controller pod"
  kubectl -n "$NS" port-forward "pod/$pod" "$UI_PORT:8080" >/dev/null 2>&1 &
  UI_PF=$!
  wait_for 30 ui_up || fail "$1: port-forward to the read API of a controller pod"
}
ui_forward_stop() {
  [[ -z $UI_PF ]] || kill "$UI_PF" 2>/dev/null || true
  UI_PF=""
}
devtools_up() { curl -fs "127.0.0.1:$CHROME_PORT/json/version" >/dev/null; }
ui_up() { curl -fs "localhost:$UI_PORT/v1/workloads" >/dev/null; }

# ldt_19_check: the deployment page of front, in pinned headless Chromium, shows the "Load tests" table with runs A and
# B (LDT-19 E). It reads the same routes as the browser does, through a port-forward to a controller pod.
ldt_19_check() {
  ui_forward_start ldt_19
  # The container holds no capability and writes only to its tmpfs; --no-sandbox as in UI-12 (the container is the sandbox).
  local chrome_image
  chrome_image=$(scripts/pinned-image.sh e2e-chromium)
  CHROME=mimic-e2e-load-chromium-$$
  docker run -d --rm --name "$CHROME" --network host --cap-drop=ALL --security-opt no-new-privileges --read-only \
    --tmpfs /tmp --tmpfs /root --entrypoint /headless-shell/headless-shell "$chrome_image" \
    --no-sandbox --user-data-dir=/tmp/chrome --disable-gpu --remote-debugging-address=127.0.0.1 --remote-debugging-port=$CHROME_PORT \
    --host-resolver-rules='MAP * ~NOTFOUND , EXCLUDE localhost' about:blank >/dev/null || fail "ldt_19: cannot start Chromium"
  wait_for 30 devtools_up || fail "ldt_19: Chromium does not answer on its DevTools port"
  python3 -I scripts/e2e/ui_browser.py "$CHROME_PORT" "http://localhost:$UI_PORT/ui/workloads/mck-19/front" \
    --wait "Load tests" --wait ldt16-a --wait ldt16-b > "$WORK/browse.json" \
    || fail "ldt_19: the browser did not show the Load tests table of front"
  [[ $(jq '.violations | length' "$WORK/browse.json") == 0 ]] || fail "ldt_19: CSP violations on the deployment page"
  [[ $(jq --arg o "http://localhost:$UI_PORT/" '[.requests[] | select((startswith($o) or startswith("data:")) | not)] | length' "$WORK/browse.json") == 0 ]] \
    || fail "ldt_19: the deployment page requested something outside the read API"
  [[ $(jq '.requests | length' "$WORK/browse.json") -gt 0 ]] || fail "ldt_19: the browser recorded no request"
  [[ $(jq '[.buttons[] | select(test("try it out|execute"; "i"))] | length' "$WORK/browse.json") == 0 ]] \
    || fail "ldt_19: a Try it out or Execute button is shown"
  # The rows: run A is Succeeded/Completed with requests, run B is Aborted/ModeLeft (the cells: name, phase, reason, start,
  # end, requests, missed, statuses, errors, RPS, p99).
  jq -e --arg a ldt16-a --arg b ldt16-b '
    ([.rows[] | select(.[0] == $a and .[1] == "Succeeded" and .[2] == "Completed" and ((.[5] | tonumber? // 0) > 0))] | length == 1)
    and ([.rows[] | select(.[0] == $b and .[1] == "Aborted" and .[2] == "ModeLeft")] | length == 1)' "$WORK/browse.json" >/dev/null \
    || fail "ldt_19: the Load tests table lacks run A (Succeeded/Completed with requests) or run B (Aborted/ModeLeft)"
  if found -F -e "$canary" "$WORK/browse.json"; then fail "ldt_19: a canary of the credentials Secret is on the deployment page"; fi
  docker rm -f "$CHROME" >/dev/null 2>&1 || true
  CHROME=""
  ui_forward_stop
}

# suite_main: LDT-16 and LDT-19 (E) on the cluster of scripts/e2e.sh.
suite_main() {
  local clusters repo mimic_ref users1 prov1 users2 prov2 sent missing job_a job_b job_c creds
  clusters=$(kind get clusters) || fail "ldt_16: kind get clusters failed"
  found -x "$CLUSTER" <<<"$clusters" || fail "ldt_16: no kind cluster $CLUSTER: --suite runs after scripts/e2e.sh"
  kubectl config use-context "kind-$CLUSTER" >/dev/null
  kubectl -n mck-19 get mimicworkload front >/dev/null 2>&1 && kubectl -n disc-22 get mimicworkload users-a >/dev/null 2>&1 \
    || fail "ldt_16: the MCK-19 setup is not there: --suite runs from scripts/e2e.sh after its F10 section"

  log "ldt_16: helm upgrade with loadTest.enabled, the digest of the image under test"
  digest=$(image_digest "$IMAGE" || true)
  [[ $digest =~ ^sha256:[0-9a-f]{64}$ ]] || fail "ldt_16: no digest of $IMAGE on the node (got '$digest')"
  repo=${IMAGE%:*}
  mimic_ref="$repo@$digest"
  # As in the standalone run: containerd knows the image by its tag only; the digest reference is an alias of it.
  docker exec "$CLUSTER-control-plane" ctr -n k8s.io images tag --force "$IMAGE" "$mimic_ref" >/dev/null
  helm upgrade "$RELEASE" deploy/helm/mimic -n "$NS" --reuse-values --set loadTest.enabled=true \
    --set image.digest="$digest" --wait --timeout 4m >/dev/null || fail "ldt_16: helm upgrade with loadTest.enabled"
  kubectl -n "$NS" rollout status deploy/$RELEASE-controller --timeout=2m
  # The webhook sets MIMIC_LOAD_ENABLED on the sidecars it injects while load testing is on (§10.3): the pods of front
  # were injected before, so they are restarted to get it (§10.3: a pod injected before keeps its env).
  kubectl -n mck-19 rollout restart deploy/front >/dev/null
  kubectl -n mck-19 rollout status deploy/front --timeout=3m >/dev/null || fail "ldt_16: front did not roll out after enabling load tests"
  # The old pods may still be listed right after the rollout: wait until every pod of front has the variable.
  front_load_enabled() {
    local envs
    envs=$(kubectl -n mck-19 get pods -l app=front -o json | jq -r '[.items[] | [.spec.initContainers[]? | select(.name == "mimic-sidecar") | .env[]? | select(.name == "MIMIC_LOAD_ENABLED") | .value][0] // "none"] | join(" ")') || return 1
    [[ -n $envs && $envs != *none* ]]
  }
  wait_for 120 front_load_enabled || fail "ldt_16: the pods of front have no MIMIC_LOAD_ENABLED=1 120 s after the restart"

  # The Secret of the credentials: a canary, held by the Secret and by the generator's requests only.
  canary="ldt16-$(openssl rand -hex 16)"
  kubectl -n mck-19 create secret generic front-load --from-literal=token="$canary" --from-literal=tenant="$canary" >/dev/null \
    || fail "ldt_16: cannot create the credentials Secret"
  creds='  credentials:
    secretName: front-load
    fields:
      - {field: header/authorization, key: token}
      - {field: header/x-tenant-key, key: tenant}'

  kubectl delete namespace "$PSA_NS" --ignore-not-found --wait=true --timeout=2m >/dev/null
  kubectl create namespace "$PSA_NS" >/dev/null
  kubectl label namespace "$PSA_NS" pod-security.kubernetes.io/enforce=restricted >/dev/null
  wait_for 30 exists serviceaccount default -n "$PSA_NS" || fail "ldt_16: no default service account in $PSA_NS"

  # Run C: a deployment in discovery (users). Created first, so that its start timeout (MIMIC_LOAD_START_TIMEOUT, 5 min)
  # runs alongside runs A and B.
  log "ldt_16: a run on users, in discovery, is Pending without a Job, then ends Failed/NotMock"
  c_t0=$SECONDS
  run_create disc-22 ldt16-c users ""
  job_c=$(job_of disc-22 ldt16-c) || fail "ldt_16: no uid of run C"
  wait_for 60 run_is disc-22 ldt16-c 'Pending/WaitingForMock' \
    || fail "ldt_16: run C is not Pending/WaitingForMock: $(run_status disc-22 ldt16-c)"
  absent job "$job_c" -n disc-22 || fail "ldt_16: run C has a Job while it is Pending"

  log "ldt_16: front goes to mock; run A runs against it"
  kubectl -n mck-19 patch mimicworkload front --type merge -p '{"spec":{"mode":"mock"}}' >/dev/null
  wait_for 150 front_in_mock || fail "ldt_16: front did not converge to mock within 150 s"
  sleep 10
  users0=$(calls disc-22 app=users app) || fail "ldt_16: cannot count the calls of users"
  prov0=$(calls mck-19-provider app=provider provider) || fail "ldt_16: cannot count the calls of the provider"
  exch0=$(exchanges) || fail "ldt_16: cannot read the exchanges of front"

  # Run A: concurrency 8, rate 50, 60 s, rampUp 10 s on front.
  run_create mck-19 ldt16-a front "$creds"
  job_a=$(job_of mck-19 ldt16-a) || fail "ldt_16: no uid of run A"
  wait_for 240 run_is mck-19 ldt16-a 'Running/*' || fail "ldt_16: run A did not start: $(run_status mck-19 ldt16-a)"
  wait_for 30 exists job "$job_a" -n mck-19 || fail "ldt_16: run A has no Job in mck-19"
  exists networkpolicy "$job_a" -n mck-19 || fail "ldt_16: run A has no NetworkPolicy in mck-19"
  owned_by mck-19 "job/$job_a" ldt16-a || fail "ldt_16: the Job of run A is not owned by the run"
  owned_by mck-19 "networkpolicy/$job_a" ldt16-a || fail "ldt_16: the NetworkPolicy of run A is not owned by the run"
  psa_restricted mck-19 "$job_a" || fail "ldt_16: the pod of the Job of run A does not pass Pod Security restricted"
  # The generator pods' logs are read while the run runs: the controller deletes the Job when the run ends (the sweep reads them).
  wait_run_logged mck-19 ldt16-a "$job_a" 'Succeeded/Completed' 240 "$WORK/gen-a" \
    || fail "ldt_16: run A ended $(run_status mck-19 ldt16-a), not Succeeded/Completed"
  [[ -n $(ls -A "$WORK/gen-a" 2>/dev/null) ]] || fail "ldt_16: no log of a generator pod of run A was read while it ran"
  kubectl -n mck-19 get mimicloadtest ldt16-a -o json > "$WORK/run-a.json" || fail "ldt_16: cannot read run A"
  jq -e '(.status.results.endpoints | length) > 0' "$WORK/run-a.json" >/dev/null \
    || fail "ldt_16: run A has no planned endpoint in its results"
  # requests and p99 are numbers above 0 (a null or a missing field is not a result).
  missing=$(jq '[.status.results.endpoints[] | select(([.requests, .latencyMicros.p99] | all(type == "number" and . > 0)) | not)] | length' "$WORK/run-a.json")
  [[ $missing == 0 ]] || fail "ldt_16: $missing planned endpoint(s) of run A have no request or no p99 above 0"
  # The endpoints of the run are the plan's: the inbound endpoints of front, as its read API lists them.
  ui_forward_start ldt_16
  curl -fs --max-time 10 "localhost:$UI_PORT/v1/workloads/mck-19/front/endpoints" > "$WORK/front-endpoints.json" \
    || fail "ldt_16: cannot read the endpoints of front"
  jq -r '[.endpoints[] | select(.direction == "inbound") | "\(.method) \(.template)"] | sort | .[]' "$WORK/front-endpoints.json" \
    > "$WORK/plan-endpoints.txt"
  jq -r '[.status.results.endpoints[] | "\(.method) \(.template)"] | sort | .[]' "$WORK/run-a.json" > "$WORK/run-a-endpoints.txt"
  [[ -s $WORK/plan-endpoints.txt ]] || fail "ldt_16: front has no inbound endpoint to plan"
  cmp -s "$WORK/plan-endpoints.txt" "$WORK/run-a-endpoints.txt" \
    || fail "ldt_16: the endpoints of run A are not the inbound endpoints of front: $(diff "$WORK/plan-endpoints.txt" "$WORK/run-a-endpoints.txt" | tr '\n' ' ')"
  [[ $(calls disc-22 app=users app) == "$users0" ]] || fail "ldt_16: users logged calls during run A"
  [[ $(calls mck-19-provider app=provider provider) == "$prov0" ]] || fail "ldt_16: the provider logged calls during run A"
  [[ $(exchanges) == "$exch0" ]] || fail "ldt_16: the discovery exchanges of front grew during run A"
  if found -F -e "$canary" "$WORK/run-a.json"; then fail "ldt_16: a canary of the credentials Secret is in the status of run A"; fi
  # Run C is still Pending, with no Job, within its start timeout: checked a margin before the timeout, not after it.
  (( SECONDS - c_t0 < START_TIMEOUT - START_MARGIN )) \
    || fail "ldt_16: the Pending re-check of run C comes $((SECONDS - c_t0)) s after its creation, not within $((START_TIMEOUT - START_MARGIN)) s"
  run_is disc-22 ldt16-c 'Pending/*' || fail "ldt_16: run C is no longer Pending: $(run_status disc-22 ldt16-c)"
  absent job "$job_c" -n disc-22 || fail "ldt_16: run C has a Job while it is Pending"

  # Run B: switched out of mock 20 s after its start.
  log "ldt_16: run B is switched to discovery 20 s after its start: Aborted/ModeLeft, its Job gone within 10 s"
  run_create mck-19 ldt16-b front "$creds"
  job_b=$(job_of mck-19 ldt16-b) || fail "ldt_16: no uid of run B"
  wait_for 240 run_is mck-19 ldt16-b 'Running/*' || fail "ldt_16: run B did not start: $(run_status mck-19 ldt16-b)"
  snapshot_for mck-19 "$job_b" "$WORK/gen-b" 20
  users1=$(calls disc-22 app=users app) || fail "ldt_16: cannot count the calls of users"
  prov1=$(calls mck-19-provider app=provider provider) || fail "ldt_16: cannot count the calls of the provider"
  [[ $users1 == "$users0" && $prov1 == "$prov0" ]] || fail "ldt_16: users or the provider logged calls before the switch of run B"
  kubectl -n mck-19 patch mimicworkload front --type merge -p '{"spec":{"mode":"discovery"}}' >/dev/null
  wait_run_logged mck-19 ldt16-b "$job_b" 'Aborted/ModeLeft' 150 "$WORK/gen-b" \
    || fail "ldt_16: run B ended $(run_status mck-19 ldt16-b), not Aborted/ModeLeft"
  wait_for 10 absent job "$job_b" -n mck-19 || fail "ldt_16: the Job of run B is still there 10 s after it ended"
  wait_for 10 absent networkpolicy "$job_b" -n mck-19 || fail "ldt_16: the NetworkPolicy of run B is still there 10 s after it ended"
  sleep 5
  users2=$(calls disc-22 app=users app) || fail "ldt_16: cannot count the calls of users"
  prov2=$(calls mck-19-provider app=provider provider) || fail "ldt_16: cannot count the calls of the provider"
  # Each is bounded on its own: a call to users must not hide behind a missing one to the provider, or the reverse.
  sent_users=$(( users2 - users1 ))
  sent_prov=$(( prov2 - prov1 ))
  (( sent_users <= 8 )) || fail "ldt_16: users logged $sent_users calls from front after the switch of run B (at most 8)"
  (( sent_prov <= 8 )) || fail "ldt_16: the provider logged $sent_prov calls from front after the switch of run B (at most 8)"
  log "ldt_16: $sent_users call(s) reached users and $sent_prov the provider after the switch of run B (at most 8 each)"

  log "ldt_19: the deployment page of front shows the Load tests table with runs A and B"
  ldt_19_check

  log "ldt_16: no canary of the credentials Secret in a log line of the cluster"
  sweep_logs

  log "ldt_16: run C ends Failed/NotMock at its start timeout, without a Job or NetworkPolicy"
  wait_for 420 run_is disc-22 ldt16-c 'Failed/NotMock' \
    || fail "ldt_16: run C ended $(run_status disc-22 ldt16-c), not Failed/NotMock"
  absent job "$job_c" -n disc-22 || fail "ldt_16: run C has a Job after Failed/NotMock"
  absent networkpolicy "$job_c" -n disc-22 || fail "ldt_16: run C has a NetworkPolicy after Failed/NotMock"

  log "ldt_16: deleting the runs removes run A's Job and NetworkPolicy"
  kubectl -n mck-19 delete mimicloadtest ldt16-a ldt16-b >/dev/null || fail "ldt_16: cannot delete runs A and B"
  kubectl -n disc-22 delete mimicloadtest ldt16-c >/dev/null || fail "ldt_16: cannot delete run C"
  wait_for 90 absent job "$job_a" -n mck-19 || fail "ldt_16: the Job of run A is there after its MimicLoadTest was deleted"
  wait_for 90 absent networkpolicy "$job_a" -n mck-19 \
    || fail "ldt_16: the NetworkPolicy of run A is there after its MimicLoadTest was deleted"

  kubectl -n mck-19 delete secret front-load >/dev/null || true
  kubectl delete namespace "$PSA_NS" --wait=false >/dev/null || true
}

if (( SUITE == 1 )); then
  suite_main
  log "e2e-load --suite passed (LDT-16, LDT-19 E)"
  exit 0
fi

# --- the chart without a cluster (LDT-06) -----------------------------------------------------
log "ldt_06: the default chart installs no load test object and no load test RBAC or policy"
off=$(helm template "$RELEASE" deploy/helm/mimic -n "$NS")
for word in MimicLoadTest mimicloadtests endpointslices networkpolicies "kind: Job" "-load-job" "-load-secret"; do
  if found -F -e "$word" <<<"$off"; then fail "ldt_06: the default chart renders $word"; fi
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
found -F -e "image.digest" <<<"$tag_only" || fail "ldt_06: the message does not name image.digest: $tag_only"

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
  found -F -e "$key" <<<"$out" || fail "ldt_17: the refusal does not name '$key': $out"
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

log "ldt_17: the Job policy refuses another fsGroup, and a credentials Secret not read by the group at 0440"
refuse "fsGroup must be 65532" job '.spec.template.spec.securityContext.fsGroup = 0'
refuse "defaultMode must be 0440" job-secret '.spec.template.spec.volumes |= map(if .secret then .secret.defaultMode = 420 else . end)'

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
  found -F -e "metadata labels must be those of spec 7.8" <<<"$out" \
    || fail "ldt_17: the refusal of the delete of $object does not name the labels: $out"
done
# Without the owner reference: its MimicLoadTest does not exist, so the garbage collector
# would delete the objects before the controller does.
jq '.spec.suspend = true | del(.metadata.ownerReferences)' "$WORK/job.json" > "$WORK/job-own.json"
jq 'del(.metadata.ownerReferences)' "$WORK/np.json" > "$WORK/np-own.json"
kubectl create -f "$WORK/job-own.json" >/dev/null || fail "ldt_17: the controller's Job was not created by the admin"
kubectl create -f "$WORK/np-own.json" >/dev/null || fail "ldt_17: the controller's NetworkPolicy was not created by the admin"
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
