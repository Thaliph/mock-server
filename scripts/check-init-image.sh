#!/usr/bin/env bash
# INJ-17: the mimic-init image runs as UID 65532 by default, has no shell, prints its version,
# and carries iptables-restore and ip6tables-restore with the nft backend.
# Usage: scripts/check-init-image.sh <image>
set -euo pipefail
image=${1:?usage: check-init-image.sh <image>}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$(dirname "$0")/../Cargo.toml" | head -1)

fail() { echo "FAIL inj_17: $*" >&2; exit 1; }

user=$(docker inspect --format '{{.Config.User}}' "$image")
[[ $user == 65532:65532 ]] || fail "image user is '$user', want 65532:65532"

for shell in /bin/sh /bin/bash /busybox/sh; do
  if docker run --rm --entrypoint "$shell" "$image" -c true >/dev/null 2>&1; then
    fail "$shell exists in the image"
  fi
done

out=$(docker run --rm --entrypoint /usr/local/bin/mimic-init "$image" --version)
[[ $out == "mimic-init $version" ]] || fail "mimic-init --version printed '$out', want 'mimic-init $version'"

for restore in iptables-restore ip6tables-restore; do
  out=$(docker run --rm --entrypoint "/usr/sbin/$restore" "$image" --version)
  [[ $out == *"(nf_tables)"* ]] || fail "$restore is not the nft backend: '$out'"
done
echo "ok inj_17: user 65532, no shell, mimic-init $version, nft backend"
