#!/usr/bin/env bash
# Publishes docs/wiki/ to the GitHub wiki of this repository (Thaliph/mock-server.wiki.git).
# The wiki pages are reviewed in docs/wiki/ by PR; this copies them as they are on the current
# checkout. GitHub creates the wiki repository only once a first page has been saved in the web
# UI: do that once (any content, it is replaced).
#   WIKI_REMOTE=git@github.com:Thaliph/mock-server.wiki.git   remote of the wiki
# The commit uses this repository's git identity (user.name, user.email), local or global.
# Only the wiki's Markdown pages are replaced: other files there (images, attachments) are kept.
set -euo pipefail
root=$(git rev-parse --show-toplevel)
remote=${WIKI_REMOTE:-git@github.com:Thaliph/mock-server.wiki.git}
name=$(git -C "$root" config user.name) || { echo "set git user.name in this repository" >&2; exit 1; }
email=$(git -C "$root" config user.email) || { echo "set git user.email in this repository" >&2; exit 1; }
shopt -s nullglob
pages=("$root"/docs/wiki/*.md)
(( ${#pages[@]} > 0 )) || { echo "no page in $root/docs/wiki" >&2; exit 1; }
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
if ! clone_error=$(git clone -q "$remote" "$work" 2>&1); then
  echo "cannot clone $remote: $clone_error" >&2
  echo "if the repository is not found, save a first page in the repository's Wiki tab once" >&2
  exit 1
fi
rm -f "$work"/*.md
cp "${pages[@]}" "$work"/
cd "$work"
git add -A
if git diff --cached --quiet; then
  echo "wiki already up to date"
  exit 0
fi
git -c user.name="$name" -c user.email="$email" commit -q \
  -m "Sync from docs/wiki at $(git -C "$root" rev-parse --short HEAD)"
branch=$(git symbolic-ref --short HEAD)
git push -q origin "HEAD:$branch"
echo "wiki published from $(git -C "$root" rev-parse --short HEAD) to $branch"
