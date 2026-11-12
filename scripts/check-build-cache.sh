#!/usr/bin/env bash
# #496: two builds of different source trees on one daemon produce binaries of their own sources.
# The build stage's target cache (`/src/target`) is shared by every local build on the daemon, so
# the second tree must not get the first tree's artifacts. Both trees carry a marker of their own in
# mimic-trust's --version, so each build compiles its own mimic-trust (a cached layer would prove
# nothing: the markers are unique to this run, and `--no-cache-filter` is not used because it
# empties the cache mounts). Manual check, not run in CI: the cache is fresh there, and two image
# builds of the workspace take minutes (docs/wiki/Developing.md). It leaves the last tree's
# artifacts in the shared cache, as any build does.
# Usage: scripts/check-build-cache.sh
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
nonce=$$
work=$(mktemp -d)
trap 'rm -rf "$work"; for t in a b; do docker image rm -f "mimic-build-cache:$t" >/dev/null 2>&1 || true; done' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }

# Two trees with the build inputs of the working copy, and a marker line in mimic-trust each.
marker_line='const VERSION: \&str = concat!(env!("CARGO_PKG_VERSION"), "+MARK");'
for tree in a b; do
  mkdir "$work/$tree"
  tar -C "$root" -cf - .dockerignore Dockerfile Cargo.toml Cargo.lock rust-toolchain.toml crates |
    tar -xf - -C "$work/$tree"
  sed -i "s|^const VERSION: &str = env!(\"CARGO_PKG_VERSION\");|${marker_line//MARK/$tree$nonce}|" \
    "$work/$tree/crates/mimic-trust/src/main.rs"
  grep -q "+$tree$nonce\"" "$work/$tree/crates/mimic-trust/src/main.rs" ||
    fail "the marker line is not in crates/mimic-trust/src/main.rs of tree $tree"
done
# Checkout-like mtimes, older than any artifact a build leaves in the cache.
find "$work/a" "$work/b" -exec touch -d @1700000000 {} +

# build <tree>: the app image of tree <tree>, timed.
build() {
  local start end
  start=$(date +%s)
  docker build -q --target app -t "mimic-build-cache:$1" "$work/$1" >/dev/null ||
    fail "docker build of tree $1 failed"
  end=$(date +%s)
  echo "built tree $1 in $((end - start)) s"
}

# expect <tree>: the image of tree <tree> prints its own version, marker included.
expect() {
  local out
  out=$(docker run --rm --entrypoint /usr/local/bin/mimic-trust "mimic-build-cache:$1" --version)
  [[ $out == "mimic-trust $version+$1$nonce" ]] ||
    fail "tree $1: mimic-trust --version printed '$out', want 'mimic-trust $version+$1$nonce'"
  echo "ok tree $1: $out"
}

build a
expect a
build b
expect b
echo "ok #496: each build ran its own sources"
