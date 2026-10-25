#!/usr/bin/env bash
# Tests de scripts/ci-changes.sh sur un dépôt jetable (renommages, noms non ASCII, échec fermé).
set -euo pipefail

script="$(cd "$(dirname "$0")" && pwd)/ci-changes.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"
git init -q -b main .
git config user.email t@example.org
git config user.name t
mkdir -p crates/a deploy/monitoring docs
echo 1 >crates/x.rs
echo 1 >deploy/a.yaml
echo 1 >deploy/monitoring/x.yaml
echo 1 >docs/d.md
echo 1 >crates/a/é.rs
git add -A
git commit -qm base
base="$(git rev-parse HEAD)"

fail=0
# run <nom> <attendu : "clé=valeur ..."> ; le dépôt est remis sur $base ensuite.
check() {
  local name="$1" expected="$2" out
  git add -A
  git commit -qm change
  out="$(EVENT_NAME=pull_request BASE_REF=main BASE_SHA="$base" LABELS_JSON='[]' GITHUB_OUTPUT=/dev/null "$script" 2>/dev/null)"
  for kv in $expected; do
    if ! printf '%s\n' "$out" | grep -qx "$kv"; then
      echo "ECHEC $name : $kv attendu, obtenu : $(echo "$out" | tr '\n' ' ')"
      fail=1
    fi
  done
  git reset -q --hard "$base"
}

git mv crates/x.rs docs/x.rs
check "renommage crates -> docs" "rust=true"

git mv deploy/a.yaml docs/a.yaml
check "renommage deploy -> docs" "e2e=true"

echo 2 >crates/a/é.rs
check "nom non ASCII seul" "rust=true"

echo 2 >deploy/monitoring/x.yaml
check "monitoring seul" "e2e=false cluster=false"

echo 2 >docs/d.md
check "docs seul" "rust=false e2e=false"

mkdir -p scripts
echo 2 >scripts/lo-up.py
check "lo-up.py" "cluster=true netns=true"

# Échec fermé : base inconnue.
out="$(EVENT_NAME=pull_request BASE_REF=main BASE_SHA=deadbeef LABELS_JSON='[]' GITHUB_OUTPUT=/dev/null "$script" 2>/dev/null)"
printf '%s\n' "$out" | grep -qx 'e2e=true' || { echo "ECHEC échec fermé : $out"; fail=1; }

# Étiquette e2e.
out="$(EVENT_NAME=pull_request BASE_REF=main BASE_SHA="$base" LABELS_JSON='["e2e"]' GITHUB_OUTPUT=/dev/null "$script" 2>/dev/null)"
printf '%s\n' "$out" | grep -qx 'e2e=true' || { echo "ECHEC étiquette e2e : $out"; fail=1; }

[ "$fail" = 0 ] && echo "ci-changes : tous les tests passent"
exit "$fail"
