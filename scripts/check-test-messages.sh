#!/usr/bin/env bash
# Flags the messages of Rust asserts that interpolate a secret-like or raw-text variable, in the lines
# that a change adds (CodeQL's cleartext-logging rule fails on them; a pre-existing hit is not reported).
#
#   scripts/check-test-messages.sh [BASE]   BASE: the commit to diff against (default: the merge base
#                                           of HEAD and origin/main). Compares BASE with HEAD, *.rs only.
#
# Flagged: an added line, or a line of the same assert statement up to 12 lines above it, that holds
# an assert!/assert_eq!/assert_ne!/prop_assert*!/debug_assert* macro and a placeholder such as {raw},
# {json}, {body}, {text}, {line}, {canary}, {token}, {jwt}, {secret}, {password} or {…secret…}.
# Exit 0 when nothing is flagged, 1 for each flagged line (file:line: text), 2 on a usage or git error.
# Tests: scripts/test_check_test_messages.sh.
set -euo pipefail

base=${1:-}
if [[ -z $base ]]; then
  base=$(git merge-base HEAD origin/main) || { echo "check-test-messages: no BASE and no merge base with origin/main" >&2; exit 2; }
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

git diff -U0 --no-renames --diff-filter=AM "$base" HEAD -- '*.rs' > "$tmp/diff" \
  || { echo "check-test-messages: git diff $base HEAD failed" >&2; exit 2; }

# The added line numbers of each file of the diff: "path<TAB>number" (the post-image numbering of HEAD).
awk '
  /^\+\+\+ / { path = substr($0, 7); next }
  /^@@ / {
    # @@ -a,b +c,d @@: the added lines start at c; d lines (0 for a pure deletion).
    split($3, p, ","); start = substr(p[1], 2) + 0; count = (p[2] == "" ? 1 : p[2] + 0)
    for (i = 0; i < count; i++) printf "%s\t%d\n", path, start + i
  }
' "$tmp/diff" > "$tmp/added"

status=0
while IFS= read -r path; do
  [[ -n $path ]] || continue
  git show "HEAD:$path" > "$tmp/src" 2>/dev/null || continue
  awk -F'\t' -v file="$path" '
    FILENAME == ARGV[1] { if ($1 == file) added[$2] = 1; next }
    { line[FNR] = $0 }
    END {
      # A placeholder of a secret-like or raw-text variable, exact names or a secret-like word inside.
      exact = "[{](json|raw|body|text|line|canary|token|jwt|secret|password|passwd|credentials?)(:[^}]*)?[}]"
      like  = "[{][A-Za-z0-9_]*(secret|token|password|canary|jwt|credential)[A-Za-z0-9_]*(:[^}]*)?[}]"
      assert_re = "(prop_|debug_)?assert(_eq|_ne)?!"
      n = 0
      for (k in added) order[++n] = k + 0
      # Sort the numbers (small lists: insertion sort).
      for (i = 2; i <= n; i++) { v = order[i]; j = i - 1; while (j >= 1 && order[j] > v) { order[j + 1] = order[j]; j-- } order[j + 1] = v }
      for (i = 1; i <= n; i++) {
        k = order[i]
        text = line[k]
        if (text !~ exact && text !~ like) continue
        found = 0
        for (m = k; m >= 1 && m >= k - 12; m--) {
          # a line above that ends a statement closes the search: the placeholder is not in an assert of its own
          if (m < k && line[m] ~ /;[ \t]*$/) break
          if (line[m] ~ assert_re) { found = 1; break }
        }
        if (found) { printf "%s:%d: %s\n", file, k, text; bad = 1 }
      }
      exit bad
    }
  ' "$tmp/added" "$tmp/src" || status=1
done < <(cut -f1 "$tmp/added" | sort -u)

if [[ $status -ne 0 ]]; then
  echo "check-test-messages: an assert message of this change interpolates a secret-like or raw value (CodeQL cleartext logging); name the value's length or a fixed text instead" >&2
fi
exit "$status"
