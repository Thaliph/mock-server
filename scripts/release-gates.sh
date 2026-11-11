#!/usr/bin/env bash
# #364, spec §12.14 (REL-04): the gates of the release workflow, in one script that runs locally
# too (`gh` and `docker` only). Stops at the first gate that fails, with one line naming it:
#   release-gates: <gate>: <reason>
# Inputs (the GitHub Actions variables by default; the ref reaches this script only through them):
#   GITHUB_EVENT_NAME   push | workflow_dispatch               (default workflow_dispatch)
#   GITHUB_REF          refs/tags/v<version> | refs/heads/main  (default refs/heads/main)
#   GITHUB_SHA          the commit under release                (default HEAD)
#   GITHUB_REPOSITORY   owner/name                              (default Thaliph/mock-server)
#   GH_TOKEN            for `gh` (a local `gh auth login` works too)
# With GITHUB_OUTPUT set it writes `version=` and `prerelease=` (true when the version has a
# pre-release part). The registry and release gates come before the ones that need the commit to
# be on main, so a branch whose version already exists is refused by the registry gate locally.
set -euo pipefail
cd "$(dirname "$0")/.."

event=${GITHUB_EVENT_NAME:-workflow_dispatch}
ref=${GITHUB_REF:-refs/heads/main}
sha=${GITHUB_SHA:-$(git rev-parse HEAD)}
repo=${GITHUB_REPOSITORY:-Thaliph/mock-server}
semver='^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'

fail() { echo "release-gates: $1: $2" >&2; exit 1; }

# Gate trigger: only a tag push or a dispatch on main may release or dry-run.
case "$event $ref" in
  "push refs/tags/v"*) tag=${ref#refs/tags/} ;;
  "workflow_dispatch refs/heads/main") tag= ;;
  *) fail trigger "event '$event' on ref '$ref': only a push of a v* tag or a dispatch on main is allowed" ;;
esac

# Gates version and tag.
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[[ $version =~ $semver ]] || fail version "workspace version '$version' is not semver (MAJOR.MINOR.PATCH[-pre-release])"
if [[ -n $tag ]]; then
  [[ $tag == "v$version" ]] || fail tag "tag '$tag' is not v$version (the workspace version)"
fi
tag=v$version

# exists_in_registry <reference>: 0 when the manifest exists, 1 when it does not; anything else
# (network, authentication) stops the script, it is never read as "absent". GHCR answers 403 to
# a repository that does not exist yet (the chart before its first release): that reads as absent
# for the chart only, and the workflow logs in with the job token first, so a package of this
# repository that exists is readable.
exists_in_registry() {
  local out
  if out=$(docker buildx imagetools inspect --raw "$1" 2>&1); then return 0; fi
  case ${out,,} in
    *"not found"*|*"name unknown"*|*"manifest unknown"*) return 1 ;;
    *"403 forbidden"*|*denied*) [[ $1 == ghcr.io/thaliph/charts/* ]] && return 1 ;&
    *) fail registry "cannot read $1: ${out%%$'\n'*}" ;;
  esac
}
# Gate registry: this is what keeps ghcr.io/thaliph/mimic:0.1.0 from ever being reused.
for reference in "ghcr.io/thaliph/mimic:$version" "ghcr.io/thaliph/mimic-init:$version" "ghcr.io/thaliph/charts/mimic:$version"; do
  if exists_in_registry "$reference"; then fail registry "$reference already exists"; fi
done

# Gate release.
releases=$(gh api --paginate "repos/$repo/releases" --jq '.[].tag_name') || fail release "cannot list the releases of $repo"
if grep -qxF "$tag" <<<"$releases"; then fail release "GitHub release $tag already exists"; fi

# remote_tag_commit: the commit that the tag points to on the remote (an annotated tag is
# dereferenced); prints nothing when the tag does not exist.
remote_tag_commit() {
  local obj type object
  obj=$(gh api "repos/$repo/git/matching-refs/tags/$tag" \
    --jq "[.[] | select(.ref == \"refs/tags/$tag\")] | first | if . == null then \"\" else \"\(.object.type) \(.object.sha)\" end") \
    || fail tag "cannot read the tags of $repo"
  [[ -n $obj ]] || return 0
  read -r type object <<<"$obj"
  if [[ $type == tag ]]; then
    object=$(gh api "repos/$repo/git/tags/$object" --jq .object.sha) || fail tag "cannot read the annotated tag $tag"
  fi
  echo "$object"
}
# Gate tag: a dry run needs the tag absent, a release needs it on the run's commit.
remote_commit=$(remote_tag_commit)
if [[ $event == workflow_dispatch ]]; then
  [[ -z $remote_commit ]] || fail tag "tag $tag already exists (a dry run needs the version untagged)"
else
  [[ $remote_commit == "$sha" ]] || fail tag "tag $tag points to '${remote_commit:-nothing}', not to the run's commit $sha"
fi

# Gate on-main: "behind" means the commit is an ancestor of main, "identical" that it is main.
status=$(gh api "repos/$repo/compare/main...$sha" --jq .status) || fail on-main "cannot compare main with $sha"
case $status in
  behind|identical) ;;
  *) fail on-main "$sha is not on main (compare status: $status)" ;;
esac

# Gate ci-ok: the latest check run of that name on the commit.
conclusion=$(gh api "repos/$repo/commits/$sha/check-runs?check_name=ci-ok&filter=latest&per_page=100" \
  --jq '.check_runs | sort_by(.started_at) | last | .conclusion // "none"') || fail ci-ok "cannot read the check runs of $sha"
[[ $conclusion == success ]] || fail ci-ok "the latest ci-ok of $sha is '$conclusion', not success"

# Gates check-release and release-notes.
scripts/check-release.sh >/dev/null || fail check-release "scripts/check-release.sh failed (run it for the reason)"
python3 -I scripts/release-notes.py "$version" >/dev/null || fail release-notes "no valid CHANGELOG.md section for $version (run scripts/release-notes.py $version)"

prerelease=false
[[ $version == *-* ]] && prerelease=true
if [[ -n ${GITHUB_OUTPUT:-} ]]; then
  { echo "version=$version"; echo "prerelease=$prerelease"; } >> "$GITHUB_OUTPUT"
fi
echo "release-gates: ok, $tag ($event), prerelease=$prerelease"
