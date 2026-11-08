#!/usr/bin/env bash
# ship.sh <pr>: the merge gate of step 4. Waits for `ci-ok` on the PR head, then merges only if
# the last `mimic-review` marker from the review account is APPROVE and was posted after the head
# commit, and only that head (`--match-head-commit`). Never forces, never bypasses a required
# check; any missing answer stops it (fails closed).
#
#   IGNORE=CodeQL ship.sh 386   # do not stop on these failing check names (one per line);
#                               # only when the maintainer decided so for that PR.
#
# MIMIC_REVIEWER: the login whose comments carry the verdict (the agents post as the maintainer).
# Exit codes: 0 merged (or already merged), 1 a check failed, 2 a GitHub answer was missing,
# 3 not approved, 4 approval older than the head, 5 merge refused (head moved, behind main,
# protection), 6 timeout (2 h).
set -uo pipefail
pr=${1:?usage: ship.sh <pr>}
repo=${MIMIC_REPO:-Thaliph/mock-server}
reviewer=${MIMIC_REVIEWER:-Thaliph}
need() { [[ -n $2 ]] || { echo "#$pr: no $1 from GitHub, stopping"; exit 2; }; }
for _ in $(seq 1 120); do
  state=$(gh pr view "$pr" -R "$repo" --json state -q .state); need state "$state"
  [[ $state == MERGED ]] && { echo "#$pr already merged"; exit 0; }
  [[ $state == OPEN ]] || { echo "#$pr is $state"; exit 5; }
  sha=$(gh pr view "$pr" -R "$repo" --json headRefOid -q .headRefOid); need "head sha" "$sha"
  checks=$(gh pr checks "$pr" -R "$repo" --json name,state 2>/dev/null || echo '[]')
  ciok=$(jq -r '[.[] | select(.name == "ci-ok") | .state] | first // ""' <<<"$checks")
  failed=$(jq -r '.[] | select(.state == "FAILURE") | .name' <<<"$checks" \
    | grep -vxF -e "${IGNORE:-@none@}" || true)
  committed=$(gh api "repos/$repo/commits/$sha" -q .commit.committer.date); need "commit date" "$committed"
  # The verdict line, alone on its line, in a comment of the review account only.
  verdict=$(gh pr view "$pr" -R "$repo" --json comments | jq -r --arg who "$reviewer" '
    [.comments[] | select(.author.login == $who)
      | select(.body | test("(^|\n)<!-- mimic-review: (APPROVE|REQUEST_CHANGES|COMMENT) -->(\r?\n|$)"))]
    | last | if . == null then "- NONE" else
      "\(.createdAt) \(.body | capture("(^|\n)<!-- mimic-review: (?<v>[A-Z_]+) -->").v)" end')
  need verdict "$verdict"
  vdate=${verdict%% *}; v=${verdict##* }
  echo "#$pr sha=${sha:0:7} ci-ok=${ciok:-pending} review=$v"
  if [[ -n $failed ]]; then echo "failed: $failed"; exit 1; fi
  if [[ $ciok == SUCCESS ]]; then
    [[ $v == APPROVE ]] || { echo "#$pr not approved"; exit 3; }
    # ISO 8601 UTC timestamps compare as strings.
    [[ $vdate > $committed ]] || { echo "#$pr approval older than the head"; exit 4; }
    gh pr merge "$pr" -R "$repo" --merge --delete-branch --match-head-commit "$sha" \
      && { echo "#$pr merged"; exit 0; }
    echo "#$pr merge refused (head moved, behind main?)"; exit 5
  fi
  sleep 60
done
echo "#$pr timeout"; exit 6
