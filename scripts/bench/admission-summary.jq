# The mesh-admission verdict of scripts/bench.sh (ADM-32, ADM-63): run with -n, --slurpfile runs (the JSONL of the runs)
# and --argjson p90, refusal, queue, pct and rps (the budgets). The refusal durations of each pod are counts per
# millisecond (admission-refusals.jq), merged and expanded before their percentile.

# Counts per millisecond [{ms, n}] back to one duration per refusal (at most the lines of one run).
def expand: map(. as $c | range($c.n) | $c.ms);
def r2: (. * 100 | round) / 100;
def abs: if . < 0 then -. else . end;
def p99: if length == 0 then null else (sort | .[((length * 99 / 100) | floor)]) end;
def r3: (. * 1000 | round) / 1000;
def mean: add / length;
def median: sort | if length % 2 == 1 then .[length / 2 | floor] else (.[length / 2 - 1] + .[length / 2]) / 2 end;
# Each side is the mean of its two runs (ABBA); the latencies are the exact ones of the access log.
($runs | map(select(.admission.variant != "adm-over")) | group_by([.protocol, .rate]) | map(
    map(select(.admission.variant == "adm-off")) as $off
    | map(select(.admission.variant == "adm-under")) as $under
    | ($off | map(.admission.latency_ms)) as $o
    | ($under | map(.admission.latency_ms)) as $u
    | {protocol: $under[0].protocol, rate: $under[0].rate,
       p99_off_runs_ms: ($o | map(.p99)), p99_under_runs_ms: ($u | map(.p99)),
       p99_off_ms: ($o | map(.p99) | mean | r3), p99_under_ms: ($u | map(.p99) | mean | r3),
       delta_p50_ms: (($u | map(.p50) | mean) - ($o | map(.p50) | mean) | r3),
       delta_p90_ms: (($u | map(.p90) | mean) - ($o | map(.p90) | mean) | r3),
       delta_p99_ms: (($u | map(.p99) | mean) - ($o | map(.p99) | mean) | r3),
       errors_under: ($under | map(.errors) | add)})) as $pairs
# The verdict (ADM-32): per protocol, the median over the rates of the p90 deltas, and no error under the contract;
# the p50 and p99 deltas are recorded only (the p99 of a 30 s run is noise at this budget, #453).
| ($pairs | group_by(.protocol) | map({protocol: .[0].protocol, rates: map(.rate),
    median_delta_p90_ms: (map(.delta_p90_ms) | median | r3),
    median_delta_p99_ms: (map(.delta_p99_ms) | median | r3), errors_under: (map(.errors_under) | add)}
    | . + {within_budget: (.median_delta_p90_ms <= $p90 and .errors_under == 0)})) as $medians
| ($runs | map(select(.admission.variant == "adm-over")) | map(
    (.fortio.ActualDuration / 1e9) as $secs
    | ([.admission.pods[].refusals.overloaded] | add // [] | expand) as $overloaded
    | ([.admission.pods[].refusals["queue-timeout"]] | add // [] | expand) as $timeouts
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
| {budgets: {added_p90_ms: $p90, refusal_p99_ms: $refusal, queue_timeout_max_ms: $queue, rate_percent: $pct},
   pairs: $pairs, pair_medians: $medians, over: $over,
   within_budget: (($medians | length) > 0 and ($over | length) > 0 and (($medians + $over) | all(.within_budget)))}
