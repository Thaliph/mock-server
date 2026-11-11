#!/usr/bin/env bash
# #364, spec §12.14 (REL-07, REL-10): a multi-arch index as it is in the registry holds exactly the
# platforms linux/amd64 and linux/arm64, and one attestation manifest (SBOM and provenance, from
# BuildKit) for each of them. Needs docker buildx and jq.
#
#   scripts/check-index.sh <repo>@<index digest>
set -euo pipefail

ref=${1:?usage: check-index.sh <repo>@<index digest>}
manifest=$(docker buildx imagetools inspect "$ref" --format '{{json .Manifest}}')

platforms=$(jq -r '[.manifests[] | select(.annotations["vnd.docker.reference.type"] != "attestation-manifest") | "\(.platform.os)/\(.platform.architecture)"] | sort | join(",")' <<<"$manifest")
[[ $platforms == linux/amd64,linux/arm64 ]] || { echo "check-index: $ref has the platforms '$platforms', want linux/amd64,linux/arm64" >&2; exit 1; }

attestations=$(jq -r '[.manifests[] | select(.annotations["vnd.docker.reference.type"] == "attestation-manifest") | .annotations["vnd.docker.reference.digest"]] | unique | length' <<<"$manifest")
[[ $attestations == 2 ]] || { echo "check-index: $ref has attestations for $attestations platform image(s), want 2" >&2; exit 1; }

echo "check-index: $ref holds linux/amd64 and linux/arm64, each with an attestation"
