#!/usr/bin/env bash
# #364, spec §12.14 (REL-01): the workspace version, the chart `version` and the chart `appVersion` are
# one string, and it is a semver version (MAJOR.MINOR.PATCH with an optional pre-release part, no
# build metadata). Reads the files with awk only. Exit 0 when that holds; otherwise exit 1 with one
# line naming the file and the value. REL-03: the wiki, Install and SECURITY.md name that version
# and the cosign identity of the release workflow; the wiki sidebar and the pages agree.
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

# REL-03: the documentation of the release.
wiki=docs/wiki
identity="https://github.com/Thaliph/mock-server/.github/workflows/release.yml@refs/tags/v$workspace"
pre=
[[ $workspace == *-* ]] && pre=" (pre-release)"

# has <file> <fixed string> <what>: the file contains the string.
has() { grep -qF -- "$2" "$1" || fail "$1: $3"; }

# Home: the version note comes before the first section.
note="This wiki documents mimic **$workspace**$pre."
before_section=$(awk '/^## / { exit } { print }' "$wiki/Home.md")
grep -qxF -- "$note" <<<"$before_section" || fail "$wiki/Home.md: no line '$note' before the first section"
has "$wiki/Home.md" "F0 to F11" "does not describe F0 to F11"
grep -qi 'monitoring' "$wiki/Home.md" || fail "$wiki/Home.md: does not describe the monitoring"

# Install and SECURITY.md: the pinned install and the verification of both images and the chart.
has "$wiki/Install.md" "oci://ghcr.io/thaliph/charts/mimic --version $workspace" "does not install oci://ghcr.io/thaliph/charts/mimic --version $workspace"
for doc in "$wiki/Install.md" SECURITY.md; do
  has "$doc" "cosign verify" "has no cosign verify"
  has "$doc" "$identity" "does not name the identity $identity"
  has "$doc" "--certificate-oidc-issuer" "does not name the OIDC issuer"
  for ref in ghcr.io/thaliph/mimic ghcr.io/thaliph/mimic-init ghcr.io/thaliph/charts/mimic; do
    has "$doc" "$ref:$workspace" "does not verify $ref:$workspace"
  done
done
has SECURITY.md "$workspace" "does not name the supported release $workspace"

# No page claims there is no v2 release, and the Load-test page exists.
stale=$(grep -rlF 'No v2 release' "$wiki" | head -1 || true)
[[ -z $stale ]] || fail "$stale: says 'No v2 release'"
[[ -f $wiki/Load-test.md ]] || fail "$wiki/Load-test.md: missing"

# The sidebar links every page, and every link resolves to a page.
for page in "$wiki"/*.md; do
  name=$(basename "$page" .md)
  [[ $name == _Sidebar ]] && continue
  grep -qF "]($name)" "$wiki/_Sidebar.md" || fail "$wiki/_Sidebar.md: no link to $name"
done
while IFS= read -r target; do
  [[ -f $wiki/$target.md ]] || fail "$wiki/_Sidebar.md: the link ($target) has no page"
done < <(grep -o '](\([^)#:]*\))' "$wiki/_Sidebar.md" | sed 's/^](//; s/)$//')

echo "check-release: version $workspace in $cargo_toml and $chart; documentation of $workspace"
