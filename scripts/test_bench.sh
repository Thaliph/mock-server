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

# ben_12: the mesh-eject and mesh-breaker setups (CBR-34, #451) are accepted, run after mesh-refuse, and their plan
# lines name the resetting pod, the ejection budget, the breaker and its budget. Only mesh-breaker asks for the delay.
ben_12_mesh_eject_and_breaker_planned() {
  mkdir -p "$tmp/out-eject"
  outdir="$tmp/out-eject"
  run "SETUPS=mesh-breaker mesh-eject mesh-refuse mesh-lb" "PROTOCOLS=http1" "RATES=200" -- --dry-run
  check "the four mesh setups are accepted" rc_is 0
  check "four runs" runs_is 4
  check "mesh-lb runs first whatever the order of SETUPS" has "run 1/4 setup mesh-lb protocol http1"
  check "mesh-refuse runs third" has "run 2/4 setup mesh-refuse protocol http1"
  check "mesh-eject runs after mesh-refuse" has "run 3/4 setup mesh-eject protocol http1 rate 200 rps"
  check "mesh-breaker runs last" has "run 4/4 setup mesh-breaker protocol http1 rate 200 rps"
  check "the plan has 2 injected and 1 resetting pod" has "mesh-eject: backend 3 pods, 2 injected and 1 not injected and resetting every connection"
  check "the plan names the budget of mesh-eject" has "at most 12 requests per ejection of the caller sidecar (5 consecutive failures + 8 connections - 1)"
  check "the plan names the breaker" has "mesh-breaker: backend 3 pods answering after 50 ms, MimicConfig circuitBreaker.maxRequests 8, 32 connections"
  check "the plan names the budget of mesh-breaker" has "at least 10 % of the requests must be answered breaker-open and the p99 of the access duration_ms of the breaker-open lines must be at most 1 ms"
  check "only mesh-breaker asks for the delay" count_is "delay=50ms" 1
  check "the results file of mesh-eject is named" has "$today-mesh-eject.json"
  check "the results file of mesh-breaker is named" has "$today-mesh-breaker.json"
  check "nothing is run" no_calls
  run "SETUPS=mesh-eject mesh-breaker" "PROTOCOLS=http1" "RATES=50" -- --dry-run
  check "no propagation measure" lacks "propagation"
  check "no rollout restart" lacks "rollout restart"
  run -- --dry-run
  check "the default matrix has none of them" lacks "mesh-eject"
  run "SETUPS=mesh-eject-x" -- --dry-run
  check "a misspelt setup exits 2" rc_is 2
  check "the message names the new setups" has "mesh-eject, mesh-breaker"
  outdir=""
}

# adm_32: the mesh-admission setup (ADM-32, #453) is accepted and runs last. Each rate of RATES runs without the
# admission, then under a contract that never binds; then one run per protocol at twice the contract's rps (per pod),
# with more connections and the initial errors allowed. The plan names the budgets. Not in the default matrix.
# runs_with <text> <n>: exactly n run lines hold the text.
runs_with() { [[ $(grep "^run " <<<"$out" | grep -cF -- "$1") -eq $2 ]]; }
adm_32_mesh_admission_setup_planned() {
  mkdir -p "$tmp/out-admission"
  outdir="$tmp/out-admission"
  run "SETUPS=mesh-admission mesh-breaker" "PROTOCOLS=http1" "RATES=50 200" -- --dry-run
  check "mesh-admission is accepted" rc_is 0
  check "eleven runs" runs_is 11
  check "ten pauses" pauses_is 10
  check "mesh-breaker runs first" has "run 1/11 setup mesh-breaker protocol http1 rate 50 rps"
  check "mesh-breaker takes run 2" has "run 2/11 setup mesh-breaker protocol http1 rate 200 rps"
  # Each rate in ABBA order: without, under, under, without, so that a drift over the four runs cancels.
  check "A: the baseline of a rate" has "run 3/11 setup mesh-admission protocol http1 rate 50 rps (no admission)"
  check "B: the contract at the same rate" has "run 4/11 setup mesh-admission protocol http1 rate 50 rps (admission 64/1000)"
  check "B: the contract again" has "run 5/11 setup mesh-admission protocol http1 rate 50 rps (admission 64/1000)"
  check "A: the baseline again" has "run 6/11 setup mesh-admission protocol http1 rate 50 rps (no admission)"
  check "the next rate starts with its baseline" has "run 7/11 setup mesh-admission protocol http1 rate 200 rps (no admission)"
  check "and ends with it" has "run 10/11 setup mesh-admission protocol http1 rate 200 rps (no admission)"
  check "the over run comes last, at 180 rps" has "run 11/11 setup mesh-admission protocol http1 rate 180 rps (admission 64/30, 2x)"
  check "the over run asks for 180 rps" runs_with "-qps 180" 1
  check "the over run has 64 connections" runs_with "-c 64" 1
  check "only the over run spreads its connections in time" runs_with "-uniform" 1
  check "only the over run and mesh-breaker allow the initial errors" runs_with "-allow-initial-errors" 3
  check "the over run allows the initial errors" has "(admission 64/30, 2x): fortio load -qps 180 -t 30s -c 64"
  check "the other runs have 32 connections" runs_with "-c 32" 10
  check "every run asks for the 50 ms delay" runs_with "delay=50ms" 11
  check "the pairs log every request's latency" runs_with "-access-log-file /dev/stderr" 8
  check "the plan names the order" has "ABBA"
  check "the plan names the added p90 budget" has "+0.5 ms"
  check "the plan names the median" has "median"
  check "the plan names the refusal budget" has "at most 1 ms"
  check "the plan names the queue-timeout budget" has "at most 160 ms"
  check "the plan names the rate budget" has "within 5 % of 30 rps per pod"
  check "the results file of mesh-admission is named" has "$today-mesh-admission.json"
  check "nothing is run" no_calls
  run "SETUPS=mesh-admission" "PROTOCOLS=http1 h2c" "RATES=100" -- --dry-run
  check "two protocols: ten runs" runs_is 10
  check "one over run per protocol" runs_with "(admission 64/30, 2x)" 2
  check "the h2c over run" has "run 10/10 setup mesh-admission protocol h2c rate 180 rps (admission 64/30, 2x)"
  run -- --dry-run
  check "the default matrix lacks mesh-admission" lacks "mesh-admission"
  run "SETUPS=mesh-admission-x" -- --dry-run
  check "a misspelt setup exits 2" rc_is 2
  check "the message names the setup" has "mesh-breaker or mesh-admission"
  check "nothing is run" no_calls
  outdir=""
}

# --- ADM-63: the refusal durations as counts per millisecond, and the M5 runs table -----------------------------------
# refused <command...>: the command fails (its stderr dropped). not_in <text> <file>: the file lacks the text.
refused() { ! "$@" 2>/dev/null; }
not_in() { ! grep -qF -- "$1" "$2"; }
access() { printf '{"target":"access","direction":"%s","reason":"%s","duration_ms":%s}\n' "$1" "$2" "$3"; }
refusal_counts='{"overloaded":[{"ms":0,"n":2},{"ms":1,"n":1},{"ms":3,"n":1}],"queue-timeout":[{"ms":150,"n":2},{"ms":152,"n":1}]}'

adm_63_refusal_durations_are_counts_per_millisecond() {
  local got
  {
    access inbound overloaded 3
    access inbound queue-timeout 152
    access inbound overloaded 0
    echo 'not json'
    echo 42
    access outbound overloaded 99
    echo '{"target":"access","direction":"inbound","status":200,"duration_ms":7}'
    access inbound connect-failed 4
    echo '{"target":"mesh","direction":"inbound","reason":"overloaded","duration_ms":8}'
    access inbound queue-timeout 150
    access inbound overloaded 1
    echo '{"target":"access","direction":"inbound","reason":"overloaded","duration_ms":"5"}'
    echo '{"target":"access","direction":"inbound","reason":"overloaded"}'
    access inbound overloaded 0
    access inbound queue-timeout 150
  } > "$tmp/refusals.log"
  got=$(jq -n -R -c -f "$root/scripts/bench/admission-refusals.jq" < "$tmp/refusals.log")
  check "the counts per millisecond of the fixture" [ "$got" == "$refusal_counts" ]
  got=$(jq -n -R -c -f "$root/scripts/bench/admission-refusals.jq" < /dev/null)
  check "no line gives both keys, empty" [ "$got" == '{"overloaded":[],"queue-timeout":[]}' ]
  check "bench.sh runs the program file with -n -R" grep -qF -- "jq -n -R -c -f \"\$JQ_DIR/admission-refusals.jq\"" "$bench"
  check "bench.sh keeps no inline copy" not_in 'map(select(.reason == "overloaded")' "$bench"
}

# over_run <pods JSON>: one adm-over run of the JSONL of mesh-admission, with these pods.
over_run() {
  jq -n -c --argjson pods "$1" '{protocol: "http1", rate: 180, actual_qps: 176, fortio: {ActualDuration: 30e9},
    admission: {variant: "adm-over", contract: {concurrency: 64, rps: 30}, pods: $pods}}'
}
summary_of() {
  jq -n -c -f "$root/scripts/bench/admission-summary.jq" --argjson p90 0.5 --argjson refusal 1 --argjson queue 160 \
    --argjson pct 5 --argjson rps 30 --slurpfile runs "$1" | jq -c '.over[0]
      | {overloaded_lines, overloaded_p99_ms, queue_timeout_lines, queue_timeout_max_ms, queue_timeout_ms_counts, within_budget}'
}

adm_63_the_verdict_reads_the_counts() {
  local got
  over_run "[{\"pod\": \"a\", \"requests\": 1000, \"ok\": 900, \"refusals\": $refusal_counts}]" > "$tmp/one.jsonl"
  got=$(summary_of "$tmp/one.jsonl")
  check "the verdict of the fixture's counts" [ "$got" == '{"overloaded_lines":4,"overloaded_p99_ms":3,"queue_timeout_lines":3,"queue_timeout_max_ms":152,"queue_timeout_ms_counts":[{"ms":150,"n":2},{"ms":152,"n":1}],"within_budget":false}' ]
  over_run '[{"pod": "a", "requests": 1000, "ok": 900, "refusals": {"overloaded": [{"ms": 0, "n": 200}], "queue-timeout": [{"ms": 150, "n": 3}]}},
    {"pod": "b", "requests": 1000, "ok": 900, "refusals": {"overloaded": [{"ms": 5, "n": 1}], "queue-timeout": [{"ms": 152, "n": 1}]}}]' \
    > "$tmp/two.jsonl"
  got=$(summary_of "$tmp/two.jsonl")
  check "the counts of two pods merged before the percentile" [ "$got" == '{"overloaded_lines":201,"overloaded_p99_ms":0,"queue_timeout_lines":4,"queue_timeout_max_ms":152,"queue_timeout_ms_counts":[{"ms":150,"n":3},{"ms":152,"n":1}],"within_budget":true}' ]
  check "bench.sh runs the program file" grep -qF -- "-f \"\$JQ_DIR/admission-summary.jq\"" "$bench"
  check "bench.sh keeps no def p99" not_in "def p99:" "$bench"
}

# m5_table_ok <README>: the M5 runs have their own table, the last of the file (BEN-07 appends below it).
m5_table_ok() {
  awk '
    function cells(line,   n) { n = gsub(/\|/, "|", line); return n - 1 }
    { lines[NR] = $0 }
    /^### M5 runs \(mesh-admission\)$/ { heading++; at = NR }
    /^#/ { last_hash = $0 }
    /^## / { last_h2 = $0 }
    /^## M4/ { m4 = 1 } /^## M5/ { m4 = 0 }
    m4 && /mesh-admission/ { bad = bad " mesh-admission in M4 at " NR }
    /^\| [0-9-]+ \| mesh-admission/ { rows++; if (!at) bad = bad " row before the heading at " NR }
    /^\|---/ { if (cells($0) != cells(lines[NR - 1])) bad = bad " delimiter of " cells($0) " cells at " NR }
    NF { last = $0 }
    END {
      if (heading != 1) bad = bad " " heading + 0 " headings"
      if (last_hash != "### M5 runs (mesh-admission)") bad = bad " last heading " last_hash
      if (last_h2 !~ /^## M5/) bad = bad " last ## " last_h2
      for (i = at + 1; i <= NR && lines[i] !~ /^\|/; i++) ;
      if (lines[i] != header) bad = bad " table header at " i
      if (cells(lines[i + 1]) != 14 || lines[i + 1] !~ /^\|---/) bad = bad " table delimiter at " i + 1
      if (rows < 26) bad = bad " " rows + 0 " rows"
      if (last !~ /^\| /) bad = bad " last line " last
      if (bad != "") { print bad > "/dev/stderr"; exit 1 }
    }' header="$(sed -n '112p' "$1")" "$1" || return 1
  [[ $(sed -n '112p' "$1") == "| Date | Setup | Protocol |"* ]] || return 1
  [[ $(tail -c1 "$1" | od -An -c | tr -d ' ') == '\n' ]]
}

adm_63_m5_runs_have_their_own_table_at_the_end() {
  local readme="$root/docs/bench/README.md"
  check "the M5 runs table of the README" m5_table_ok "$readme"
  # The check refuses the faults it is for: a row left in M4, text after the table, a 13-cell delimiter, no final newline.
  awk '/^## M5/ && !done { print "| 2026-11-19 | mesh-admission (no admission) | http1 | 50 |"; done = 1 } { print }' "$readme" > "$tmp/r1.md"
  check "a row in the M4 table is refused" refused m5_table_ok "$tmp/r1.md"
  { cat "$readme"; printf '\nSome text.\n'; } > "$tmp/r2.md"
  check "text after the table is refused" refused m5_table_ok "$tmp/r2.md"
  sed 's/^|---|---|---|---|---|---|---|---|---|---|---|---|---|---|$/|---|---|---|---|---|---|---|---|---|---|---|---|---|/' "$readme" > "$tmp/r3.md"
  check "a 13-cell delimiter is refused" refused m5_table_ok "$tmp/r3.md"
  head -c -1 "$readme" > "$tmp/r4.md"
  check "no final newline is refused" refused m5_table_ok "$tmp/r4.md"
}

for name in ben_01_arguments_only_dry_run ben_02_invalid_configuration_refused \
  ben_03_rate_cap_and_sequential_runs ben_04_defaults_are_the_matrix \
  ben_05_images_pinned_by_digest ben_06_lock_held_refused_stale_reclaimed \
  ben_07_outputs_named_and_dry_run_writes_nothing ben_08_namespace_cleanup_trap_planned \
  ben_09_machine_description_fields_no_host_identity ben_10_mesh_refuse_setup_planned \
  ben_11_16_mesh_setups_plan ben_12_mesh_eject_and_breaker_planned adm_32_mesh_admission_setup_planned \
  adm_63_refusal_durations_are_counts_per_millisecond adm_63_the_verdict_reads_the_counts \
  adm_63_m5_runs_have_their_own_table_at_the_end; do
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
