# The refusal durations of an inbound sidecar's access lines, as counts per millisecond (ADM-63): run with -n -R on the
# lines of its log; a line that is not a JSON object, an outbound line, another reason or a duration that is not a
# number is left out. Both keys are always there:
#   {"overloaded": [{"ms": 0, "n": 2}, ...], "queue-timeout": [{"ms": 150, "n": 1}, ...]}
def counts($reason):
  map(select(.reason == $reason) | .duration_ms) | group_by(.) | map({ms: .[0], n: length});

[inputs | fromjson? | select(type == "object" and .target == "access" and .direction == "inbound"
    and (.reason == "overloaded" or .reason == "queue-timeout") and (.duration_ms | type) == "number")]
| {overloaded: counts("overloaded"), "queue-timeout": counts("queue-timeout")}
