#!/usr/bin/env bash
# #271, spec FND-10: the chart as the release publishes it renders every image by digest, and the
# two mimic images by the digest of the release. Pins a copy of the chart the way release.yml does
# (scripts/pin-chart.sh, with two distinct digests so a swapped key is caught), renders it, and
# fails when an `image:` has no `@sha256:`, when a mimic reference (the sidecar image, and the init
# image that the controller is given as an environment value) is not the pinned digest, or when one
# of the two pinned references is missing. Needs helm and yq (both on the GitHub-hosted runner image).
set -euo pipefail
cd "$(dirname "$0")/.."

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp -R deploy/helm/mimic "$work/mimic"
app=sha256:$(printf 'a%.0s' {1..64})
init=sha256:$(printf 'b%.0s' {1..64})
scripts/pin-chart.sh "$work/mimic/values.yaml" "$app" "$init"

helm template mimic "$work/mimic" -n mimic-system > "$work/rendered.yaml"

# Every `image:` field of every document (containers, initContainers, the Valkey StatefulSet...).
yq -N '.. | select(tag == "!!map" and has("image")) | .image' "$work/rendered.yaml" > "$work/images.txt"
count=$(grep -c . "$work/images.txt" || true)
[ "$count" -gt 0 ] || { echo "no image in the rendered chart" >&2; exit 1; }

fail=0
while IFS= read -r image; do
  if [[ $image != *@sha256:* ]]; then
    echo "not pinned by digest: $image" >&2
    fail=1
  fi
done < "$work/images.txt"

# Every mimic reference in the whole output, including environment values such as the init image
# that the controller is given (MIMIC_INIT_IMAGE).
grep -o 'ghcr\.io/thaliph/mimic[^"[:space:]]*' "$work/rendered.yaml" | sort -u > "$work/mimic-refs.txt" || true
for expected in "ghcr.io/thaliph/mimic@$app" "ghcr.io/thaliph/mimic-init@$init"; do
  grep -qxF "$expected" "$work/mimic-refs.txt" || { echo "missing from the rendered chart: $expected" >&2; fail=1; }
done
while IFS= read -r ref; do
  case $ref in
    ghcr.io/thaliph/mimic@"$app"|ghcr.io/thaliph/mimic-init@"$init") ;;
    *) echo "unexpected mimic reference: $ref" >&2; fail=1 ;;
  esac
done < "$work/mimic-refs.txt"

[ "$fail" -eq 0 ] || exit 1
echo "release chart: $count images, all pinned by digest; mimic and mimic-init at the release digests"
