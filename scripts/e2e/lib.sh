# Helpers shared by scripts/e2e.sh and scripts/e2e-load.sh, sourced from the repository root.
# shellcheck shell=bash disable=SC2329 # the helpers are called by the scripts that source this file

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
