#!/usr/bin/env bash
# FND-07: the image runs as UID 65532, has no shell, and both binaries print their version.
# Usage: scripts/check-image.sh <image>
set -euo pipefail
image=${1:?usage: check-image.sh <image>}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$(dirname "$0")/../Cargo.toml" | head -1)

fail() { echo "FAIL fnd_07: $*" >&2; exit 1; }

user=$(docker inspect --format '{{.Config.User}}' "$image")
[[ $user == 65532:65532 ]] || fail "image user is '$user', want 65532:65532"

for shell in /bin/sh /bin/bash /busybox/sh; do
  if docker run --rm --entrypoint "$shell" "$image" -c true >/dev/null 2>&1; then
    fail "$shell exists in the image"
  fi
done

for bin in mimic-sidecar mimic-controller; do
  out=$(docker run --rm --entrypoint "/usr/local/bin/$bin" "$image" --version)
  [[ $out == "$bin $version" ]] || fail "$bin --version printed '$out', want '$bin $version'"
done
echo "ok fnd_07: user 65532, no shell, versions $version"
