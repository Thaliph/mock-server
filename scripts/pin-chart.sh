#!/usr/bin/env bash
# Pins a chart's values to the images of one release (#271, spec FND-10): `image.digest` (mimic)
# and `inject.initImage.digest` (mimic-init, the privileged init container). Edits the values file
# in place with yq, so the key paths are matched exactly, not by whitespace.
#
#   scripts/pin-chart.sh <values.yaml> <mimic-digest> <mimic-init-digest>
#
# Exit 2 on a usage error, 1 when a digest is not `sha256:<64 hex>` or a key did not take.
set -euo pipefail

if [ $# -ne 3 ]; then
  echo "usage: $0 <values.yaml> <mimic-digest> <mimic-init-digest>" >&2
  exit 2
fi
values=$1
app_digest=$2
init_digest=$3

for digest in "$app_digest" "$init_digest"; do
  if ! [[ $digest =~ ^sha256:[0-9a-f]{64}$ ]]; then
    echo "not a sha256 digest: $digest" >&2
    exit 1
  fi
done

APP_DIGEST="$app_digest" INIT_DIGEST="$init_digest" \
  yq -i '.image.digest = strenv(APP_DIGEST) | .inject.initImage.digest = strenv(INIT_DIGEST)' "$values"

# Read the keys back: an empty or misspelt path must fail here, not in a published chart.
[ "$(yq '.image.digest' "$values")" = "$app_digest" ] || { echo "image.digest not pinned in $values" >&2; exit 1; }
[ "$(yq '.inject.initImage.digest' "$values")" = "$init_digest" ] || { echo "inject.initImage.digest not pinned in $values" >&2; exit 1; }
