#!/usr/bin/env bash
# shellcheck disable=SC2329 # the helpers and ben_* tests are called by name, from the loop at the end
# Self-test of scripts/bench.sh (BEN-01 to BEN-09, docs/SPEC.md §12.19). Every test runs the script
# with --dry-run and reads its plan. Stub docker, kind, kubectl and helm come first on PATH and record
# any call: a dry run must record none. Run from anywhere: bash scripts/test_bench.sh
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
bench="$root/scripts/bench.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fail=0
stubs="$tmp/stubs"
calls="$tmp/calls"
mkdir -p "$stubs"
for tool in docker kind kubectl helm; do
  printf '#!/usr/bin/env bash\necho "%s $*" >> "%s"\nexit 99\n' "$tool" "$calls" > "$stubs/$tool"
  chmod +x "$stubs/$tool"
done
today=$(date +%F)

# State of the current test: the lock directory and the output directory it runs with.
lock=""
outdir=""
rc=0
out=""
case_fail=0

# run [NAME=value ...] [-- args]: runs the script with a clean environment; sets rc and out.
run() {
  local -a envs=()
  while [[ $# -gt 0 && $1 != -- ]]; do envs+=("$1"); shift; done
  [[ ${1:-} == -- ]] && shift
  rc=0
  out=$(env -i PATH="$stubs:/usr/bin:/bin" HOME="$tmp" MIMIC_LOCK_DIR="$lock" BENCH_OUT_DIR="$outdir" \
    "${envs[@]}" "$bench" "$@" 2>&1) || rc=$?
}

# check <description> <command...>: a false command fails the current test, and the description says which.
check() {
  local desc=$1
  shift
  if ! "$@"; then
    echo "    not ok: $desc"
    case_fail=1
  fi
}
has() { grep -qF -- "$1" <<<"$out"; }
lacks() { ! grep -qF -- "$1" <<<"$out"; }
count_lines() { grep -c "^$1" <<<"$out" || true; }
no_calls() { [[ ! -s $calls ]]; }
rc_is() { [[ $rc -eq $1 ]]; }
runs_is() { [[ $(count_lines run) -eq $1 ]]; }
pauses_is() { [[ $(count_lines pause) -eq $1 ]]; }
digest_in() { grep -q "@sha256:" <<<"$1"; }

ben_01_arguments_only_dry_run() {
  run -- --dry-run
  check "--dry-run exits 0" rc_is 0
  check "--dry-run prints the plan" has "plan: dry run"
  run -- --bogus
  check "an unknown argument exits 2" rc_is 2
  check "an unknown argument prints a usage line" has "usage: scripts/bench.sh [--dry-run]"
  run -- --dry-run --dry-run
  check "a second argument exits 2" rc_is 2
  check "nothing is run for a bad argument" no_calls
}

ben_02_invalid_configuration_refused() {
  local case
  for case in "SETUPS=mock" "PROTOCOLS=http2" "RATES=0" "RATES=201" "RATES=1.5" "RATES=" \
    "DURATION=0s" "DURATION=301s" "DURATION=30" "CONNECTIONS=0" "CONNECTIONS=65" "PAUSE=-1" \
    "CLUSTER=Bad_Name" "CLUSTER=mimic-e2e" "CLUSTER=mimic-e2e2" "NODE_IMAGE=kindest/node:v1.33.12" \
    "RATES=9223372036854775808" "RATES=18446744073709551616" "RATES=0200" \
    "CONNECTIONS=18446744073709551617" "CONNECTIONS=064"; do
    run "$case" -- --dry-run
    local name=${case%%=*}
    check "$case: exits 2" rc_is 2
    check "$case: names $name" has "$name"
  done
  # 500 is above the cap of 200 rps: the value is named in the message.
  run "RATES=50 500" -- --dry-run
  check "a rate above 200 exits 2" rc_is 2
  check "a rate above 200 is named" has "500"
  # An output file of today's date already there is never overwritten.
  mkdir -p "$tmp/existing"
  : > "$tmp/existing/$today-plain.json"
  outdir="$tmp/existing"
  run -- --dry-run
  outdir=""
  check "an existing output file exits 2" rc_is 2
  check "an existing output file is named" has "$today-plain.json"
  check "nothing is run for invalid configuration" no_calls
}

ben_03_rate_cap_and_sequential_runs() {
  run "RATES=50 200" "SETUPS=plain" "PROTOCOLS=http1" -- --dry-run
  check "rates 50 and 200 are accepted" rc_is 0
  check "one run per rate" runs_is 2
  check "the runs use -qps 50 and -qps 200" has "-qps 50 "
  check "the runs use -qps 200" has "-qps 200 "
  check "a pause after the first run only" pauses_is 1
  check "nothing is run" no_calls
}

ben_04_defaults_are_the_matrix() {
  run -- --dry-run
  check "the default cluster is mimic-bench" has "cluster mimic-bench"
  check "the default namespace is mimic-bench" has "namespace mimic-bench"
  check "the default matrix is 18 runs" runs_is 18
  check "the default matrix pauses 17 times" pauses_is 17
  check "the default duration is 30s" has "-t 30s"
  check "the default pause is 10s" has "pause 10s"
  check "the default matrix has the plain setup" has "setup plain"
  check "the default matrix has the discovery setup" has "setup discovery"
  check "the default matrix has the none setup" has "setup none "
  check "the default matrix has h2c" has "protocol h2c"
  check "the default matrix has rate 100" has "rate 100 "
  check "nothing is run" no_calls
}

ben_05_images_pinned_by_digest() {
  local fortio kind_node refusing
  fortio=$("$root/scripts/pinned-image.sh" fortio 2>/dev/null) || fortio=""
  kind_node=$("$root/scripts/pinned-image.sh" kind-node 2>/dev/null) || kind_node=""
  refusing=$("$root/scripts/pinned-image.sh" e2e-app 2>/dev/null) || refusing=""
  run -- --dry-run
  check "the fortio image is the pinned stage" has "fortio image $fortio"
  check "the fortio image is pinned by digest" digest_in "$fortio"
  check "the node image is the pinned stage" has "kind node image $kind_node"
  check "the node image is pinned by digest" digest_in "$kind_node"
  check "the refusing pod image is the pinned stage" has "refusing pod image $refusing"
  check "the refusing pod image is pinned by digest" digest_in "$refusing"
  check "nothing is pulled" no_calls
}

ben_06_lock_held_refused_stale_reclaimed() {
  local dead
  # A live process holds the lock of scripts/e2e.sh: this test shell.
  mkdir "$lock/mimic-e2e.lock"
  echo $$ > "$lock/mimic-e2e.lock/pid"
  run -- --dry-run
  check "the e2e lock held exits 3" rc_is 3
  check "the e2e lock is named" has "mimic-e2e.lock"
  rm -rf "$lock/mimic-e2e.lock"
  # The benchmark's own lock, held by a live process.
  mkdir "$lock/mimic-bench.lock"
  echo $$ > "$lock/mimic-bench.lock/pid"
  run -- --dry-run
  check "the bench lock held exits 3" rc_is 3
  check "the bench lock is named" has "mimic-bench.lock"
  rm -rf "$lock/mimic-bench.lock"
  # A lock of a process that is gone is stale: the dry run reports it and leaves it alone.
  sleep 0 & dead=$!
  wait "$dead"
  mkdir "$lock/mimic-bench.lock"
  echo "$dead" > "$lock/mimic-bench.lock/pid"
  run -- --dry-run
  check "a stale lock does not refuse" rc_is 0
  check "a stale lock is reported" has "stale"
  check "a dry run leaves the stale lock in place" test -d "$lock/mimic-bench.lock"
  check "nothing is run" no_calls
}

ben_07_outputs_named_and_dry_run_writes_nothing() {
  mkdir -p "$tmp/out-empty"
  outdir="$tmp/out-empty"
  run -- --dry-run
  outdir=""
  check "the plain output file is named" has "$today-plain.json"
  check "the discovery output file is named" has "$today-discovery.json"
  check "the results README is named" has "README.md"
  check "the dry run writes no output file" test -z "$(ls -A "$tmp/out-empty")"
  check "nothing is run" no_calls
}

ben_08_namespace_cleanup_trap_planned() {
  run -- --dry-run
  check "the plan ends with the namespace cleanup on exit" has "cleanup on exit: kubectl delete namespace mimic-bench"
  check "the cluster is kept by default" has "cluster mimic-bench kept afterwards"
  run "KEEP_CLUSTER=0" -- --dry-run
  check "KEEP_CLUSTER=0 deletes the cluster on exit" has "cluster mimic-bench deleted on exit"
  check "nothing is run" no_calls
}

ben_09_machine_description_fields_no_host_identity() {
  run -- --dry-run
  check "the plan lists the machine fields" has "machine fields: cpu cores ram kernel docker kind kubernetes node_image"
  check "the plan names no host identity" lacks "hostname"
  check "the script reads no host identity" lacks_in_script
  check "nothing is run" no_calls
}
lacks_in_script() { ! grep -qE '(^|[^a-z_])(hostname|whoami|ip addr|ifconfig)([^a-z_]|$)' "$bench" 2>/dev/null; }

# count_is <text> <n>: the plan has the text on exactly n lines.
count_is() { [[ $(grep -c -- "$1" <<<"$out") -eq $2 ]]; }
# results_named <dir> <setup...>: the plan names <dir>/<today>-<setup>.json for each setup.
results_named() {
  local dir=$1 setup
  shift
  for setup in "$@"; do
    grep -qF -- "results $dir/$today-$setup.json" <<<"$out" || return 1
  done
}

# ben_10: the mesh-refuse setup (RTY-23) is accepted, runs last among the mesh setups, and its plan line names
# the two injected pods and the refusing one. The results go to the file of its own name.
ben_10_mesh_refuse_setup_planned() {
  mkdir -p "$tmp/out-refuse"
  outdir="$tmp/out-refuse"
  run "SETUPS=mesh-refuse" "PROTOCOLS=http1" "RATES=50" -- --dry-run
  check "mesh-refuse is accepted" rc_is 0
  check "one run of mesh-refuse" runs_is 1
  check "the plan names the mesh-refuse run" has "run 1/1 setup mesh-refuse protocol http1 rate 50 rps"
  check "the plan has 2 injected and 1 refusing pod" has "mesh-refuse: backend 3 pods, 2 injected and 1 not injected and refusing connections"
  check "the results file of mesh-refuse is named" has "$today-mesh-refuse.json"
  check "mesh-refuse has no propagation measure" lacks "propagation"
  check "mesh-refuse has no synthetic view" lacks "synthetic view"
  check "mesh-refuse is not a rollout restart" lacks "rollout restart"
  # mesh-refuse runs after mesh-lb whatever the order of SETUPS: its refusing pod must not reach the others.
  run "SETUPS=mesh-refuse mesh-lb" "PROTOCOLS=http1" "RATES=50" -- --dry-run
  check "mesh-lb runs first whatever the order of SETUPS" has "run 1/2 setup mesh-lb protocol http1"
  check "mesh-refuse runs last whatever the order of SETUPS" has "run 2/2 setup mesh-refuse protocol http1"
  run "SETUPS=mesh-refuse-x" -- --dry-run
  check "a misspelt mesh setup exits 2" rc_is 2
  check "a misspelt mesh setup is named" has "mesh-refuse-x is not plain"
  check "nothing is run" no_calls
  outdir=""
}

# ben_11_16: the plan of the mesh setups (BEN-11 to BEN-16): the mode, the routing assertion, the propagation and
# synthetic measures only for mesh-lb, the rollout restart only for mesh-churn, and one results file per setup.
ben_11_16_mesh_setups_plan() {
  mkdir -p "$tmp/out-mesh"
  outdir="$tmp/out-mesh"
  run "SETUPS=mesh-lb mesh-churn mesh-refuse" "PROTOCOLS=http1" "RATES=50" -- --dry-run
  check "the three mesh setups are accepted" rc_is 0
  check "BEN-11: the three mesh setups run in discovery mode" count_is "mode discovery" 3
  check "BEN-11: every mesh setup has a backend of 3 pods" count_is "backend 3 pods" 3
  check "BEN-12: the routing is asserted in every mesh setup" count_is "routing asserted before and after" 3
  check "BEN-13: the propagation measure is planned for mesh-lb" has "mesh-lb propagation: 20 EndpointSlice changes"
  check "BEN-15: the synthetic view is planned for mesh-lb" has "mesh-lb synthetic view: 100 Services"
  check "BEN-14: the rollout restart is planned for mesh-churn" has "mesh-churn: backend 3 pods, mode discovery; rollout restart of the backend in every run; routing asserted before and after the runs"
  check "BEN-16: one results file per mesh setup" results_named "$tmp/out-mesh" mesh-lb mesh-churn mesh-refuse
  check "nothing is run" no_calls
  run "SETUPS=mesh-churn mesh-refuse" "PROTOCOLS=http1" "RATES=50" -- --dry-run
  outdir=""
  check "BEN-13: no propagation measure without mesh-lb" lacks "propagation"
  check "BEN-15: no synthetic view without mesh-lb" lacks "synthetic view"
  check "BEN-14: the rollout restart is planned for mesh-churn only" count_is "rollout restart" 1
  check "nothing is run without mesh-lb" no_calls
}

for name in ben_01_arguments_only_dry_run ben_02_invalid_configuration_refused \
  ben_03_rate_cap_and_sequential_runs ben_04_defaults_are_the_matrix \
  ben_05_images_pinned_by_digest ben_06_lock_held_refused_stale_reclaimed \
  ben_07_outputs_named_and_dry_run_writes_nothing ben_08_namespace_cleanup_trap_planned \
  ben_09_machine_description_fields_no_host_identity ben_10_mesh_refuse_setup_planned \
  ben_11_16_mesh_setups_plan; do
  lock="$tmp/locks-$name"
  mkdir -p "$lock"
  outdir=""
  rm -f "$calls"
  case_fail=0
  "$name"
  if (( case_fail )); then
    echo "FAIL $name"
    fail=1
  else
    echo "ok   $name"
  fi
done

exit "$fail"
