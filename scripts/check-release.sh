#!/usr/bin/env bash
# #364, spec §12.14 (REL-01): the workspace version, the chart `version` and the chart `appVersion` are
# one string, and it is a semver version (MAJOR.MINOR.PATCH with an optional pre-release part, no
# build metadata). Reads the files with awk only. Exit 0 when that holds; otherwise exit 1 with one
# line naming the file and the value.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo_toml=Cargo.toml
chart=deploy/helm/mimic/Chart.yaml
semver='^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'

fail() { echo "check-release: $*" >&2; exit 1; }

# The unquoted value of `key:` at top level (chart) or `key = "value"` in a [section] (Cargo.toml).
chart_value() {
  awk -v key="$2" '
    index($0, key ":") == 1 { v = substr($0, length(key) + 2); sub(/^[[:space:]]*/, "", v); gsub(/"/, "", v); print v; exit }
  ' "$1"
}
cargo_value() {
  awk '
    /^\[/ { in_section = ($0 == "[workspace.package]"); next }
    in_section && /^version[[:space:]]*=/ { v = $0; sub(/^[^=]*=[[:space:]]*"/, "", v); sub(/".*$/, "", v); print v; exit }
  ' "$1"
}

workspace=$(cargo_value "$cargo_toml")
version=$(chart_value "$chart" version)
app_version=$(chart_value "$chart" appVersion)

[[ -n $workspace ]] || fail "$cargo_toml: [workspace.package] version is missing"
[[ -n $version ]] || fail "$chart: version is missing"
[[ -n $app_version ]] || fail "$chart: appVersion is missing"

check_semver() {
  [[ $2 =~ $semver ]] || fail "$1: '$2' is not a semver version (MAJOR.MINOR.PATCH[-pre-release])"
}
check_semver "$cargo_toml" "$workspace"
check_semver "$chart" "$version"
check_semver "$chart" "$app_version"

[[ $version == "$workspace" ]] || fail "$chart: version '$version' differs from $cargo_toml '$workspace'"
[[ $app_version == "$workspace" ]] || fail "$chart: appVersion '$app_version' differs from $cargo_toml '$workspace'"

echo "check-release: version $workspace in $cargo_toml and $chart"
