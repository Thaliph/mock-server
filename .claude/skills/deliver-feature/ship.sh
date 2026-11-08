#!/usr/bin/env bash
# ship.sh <pr>: the merge gate of step 4. Waits for `ci-ok` on the PR head, then merges only if
# the last `mimic-review` marker is APPROVE and was posted after the head commit. Never forces,
# never bypasses a required check.
#
#   IGNORE=CodeQL ship.sh 386   # do not stop on these failing check names (one per line);
#                               # only when the maintainer decided so for that PR.
#
# Exit codes: 0 merged (or already merged), 1 a check failed, 3 not approved, 4 approval older
# than the head, 5 merge refused (behind main, protection), 6 timeout (2 h).
set -uo pipefail
pr=${1:?usage: ship.sh <pr>}
repo=${MIMIC_REPO:-Thaliph/mock-server}
for _ in $(seq 1 120); do
  state=$(gh pr view "$pr" -R "$repo" --json state -q .state)
  [[ $state == MERGED ]] && { echo "#$pr already merged"; exit 0; }
  sha=$(gh pr view "$pr" -R "$repo" --json headRefOid -q .headRefOid)
  checks=$(gh pr checks "$pr" -R "$repo" --json name,state 2>/dev/null || echo '[]')
  ciok=$(jq -r '.[] | select(.name == "ci-ok") | .state' <<<"$checks")
  failed=$(jq -r '.[] | select(.state == "FAILURE") | .name' <<<"$checks" \
    | grep -vxF -e "${IGNORE:-@none@}" || true)
  committed=$(gh api "repos/$repo/commits/$sha" -q .commit.committer.date)
  verdict=$(gh pr view "$pr" -R "$repo" --json comments -q '
    [.comments[] | select(.body | test("mimic-review: (APPROVE|REQUEST_CHANGES|COMMENT)"))]
    | last | if . == null then "- NONE" else
      "\(.createdAt) \(.body | capture("mimic-review: (?<v>[A-Z_]+)").v)" end')
  vdate=${verdict%% *}; v=${verdict##* }
  echo "#$pr sha=${sha:0:7} ci-ok=${ciok:-pending} review=$v"
  if [[ -n $failed ]]; then echo "failed: $failed"; exit 1; fi
  if [[ $ciok == SUCCESS ]]; then
    [[ $v == APPROVE ]] || { echo "#$pr not approved"; exit 3; }
    [[ ! $vdate < $committed ]] || { echo "#$pr approval older than the head"; exit 4; }
    gh pr merge "$pr" -R "$repo" --merge --delete-branch && { echo "#$pr merged"; exit 0; }
    echo "#$pr merge refused (behind main?)"; exit 5
  fi
  sleep 60
done
echo "#$pr timeout"; exit 6
