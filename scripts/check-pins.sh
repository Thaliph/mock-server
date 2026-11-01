#!/usr/bin/env bash
# #273: the images that the chart and CI use outside the product images are the ones pinned in
# images/Dockerfile, which Dependabot updates. Fails when the Valkey image of values.yaml or the kind
# node image of ci.yml differs from its stage there, so a bump of images/Dockerfile must be carried
# into both places (the e2e script reads images/Dockerfile directly).
set -euo pipefail
cd "$(dirname "$0")/.."

valkey_chart=$(yq '.valkey.image.repository + ":" + .valkey.image.tag + "@" + .valkey.image.digest' deploy/helm/mimic/values.yaml)
valkey_pin=$(scripts/pinned-image.sh valkey)
if [ "$valkey_chart" != "$valkey_pin" ]; then
  echo "values.yaml valkey.image is $valkey_chart, images/Dockerfile says $valkey_pin" >&2
  exit 1
fi

kind_pin=$(scripts/pinned-image.sh kind-node)
if ! grep -qF "node_image: $kind_pin" .github/workflows/ci.yml; then
  echo "ci.yml does not use the kind node image of images/Dockerfile: $kind_pin" >&2
  exit 1
fi

echo "pins: the Valkey image and the kind node image match images/Dockerfile"
