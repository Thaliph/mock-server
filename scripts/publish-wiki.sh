#!/usr/bin/env bash
# Publishes docs/wiki/ to the GitHub wiki of this repository (Thaliph/mock-server.wiki.git).
# The wiki pages are reviewed in docs/wiki/ by PR; this copies them as they are on the current
# checkout. GitHub creates the wiki repository only once a first page has been saved in the web
# UI: do that once (any content, it is replaced).
#   WIKI_REMOTE=git@github.com:Thaliph/mock-server.wiki.git   remote of the wiki
set -euo pipefail
root=$(git rev-parse --show-toplevel)
remote=${WIKI_REMOTE:-git@github.com:Thaliph/mock-server.wiki.git}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git clone -q "$remote" "$work" \
  || { echo "cannot clone $remote: save a first page in the repository's Wiki tab once" >&2; exit 1; }
find "$work" -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf {} +
cp "$root"/docs/wiki/*.md "$work"/
cd "$work"
git add -A
if git diff --cached --quiet; then
  echo "wiki already up to date"
  exit 0
fi
git commit -q -m "Sync from docs/wiki at $(git -C "$root" rev-parse --short HEAD)"
git push -q origin HEAD
echo "wiki published from $(git -C "$root" rev-parse --short HEAD)"
