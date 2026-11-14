#!/usr/bin/env bash
# #273: the images that the chart and CI use outside the product images are the ones pinned in
# images/Dockerfile, which Dependabot updates. Fails when the Valkey image of values.yaml or the kind
# node image of ci.yml differs from its stage there, so a bump of images/Dockerfile must be carried
# into both places (the e2e script reads images/Dockerfile directly).
set -euo pipefail
cd "$(dirname "$0")/.."
fail_rte25() { echo "rte_25: $*" >&2; exit 1; }

valkey_chart=$(yq '.valkey.image.repository + ":" + .valkey.image.tag + "@" + .valkey.image.digest' deploy/helm/mimic/values.yaml)
valkey_pin=$(scripts/pinned-image.sh valkey)
if [ "$valkey_chart" != "$valkey_pin" ]; then
  echo "values.yaml valkey.image is $valkey_chart, images/Dockerfile says $valkey_pin" >&2
  exit 1
fi

exporter_chart=$(yq '.valkey.metrics.image.repository + ":" + .valkey.metrics.image.tag + "@" + .valkey.metrics.image.digest' deploy/helm/mimic/values.yaml)
exporter_pin=$(scripts/pinned-image.sh redis-exporter)
if [ "$exporter_chart" != "$exporter_pin" ]; then
  echo "values.yaml valkey.metrics.image is $exporter_chart, images/Dockerfile says $exporter_pin" >&2
  exit 1
fi

# #379 (MON-15): the bundled Prometheus image of the chart (monitoring.prometheus.image) is the stage prometheus.
prometheus_chart=$(yq '.monitoring.prometheus.image.repository + ":" + .monitoring.prometheus.image.tag + "@" + .monitoring.prometheus.image.digest' deploy/helm/mimic/values.yaml)
prometheus_pin=$(scripts/pinned-image.sh prometheus)
if [ "$prometheus_chart" != "$prometheus_pin" ]; then
  echo "values.yaml monitoring.prometheus.image is $prometheus_chart, images/Dockerfile says $prometheus_pin" >&2
  exit 1
fi

# #379 (MON-15): the bundled Grafana and its dashboard sidecar (monitoring.grafana.image, monitoring.grafana.sidecarImage).
grafana_chart=$(yq '.monitoring.grafana.image.repository + ":" + .monitoring.grafana.image.tag + "@" + .monitoring.grafana.image.digest' deploy/helm/mimic/values.yaml)
grafana_pin=$(scripts/pinned-image.sh grafana)
if [ "$grafana_chart" != "$grafana_pin" ]; then
  echo "values.yaml monitoring.grafana.image is $grafana_chart, images/Dockerfile says $grafana_pin" >&2
  exit 1
fi

sidecar_chart=$(yq '.monitoring.grafana.sidecarImage.repository + ":" + .monitoring.grafana.sidecarImage.tag + "@" + .monitoring.grafana.sidecarImage.digest' deploy/helm/mimic/values.yaml)
sidecar_pin=$(scripts/pinned-image.sh k8s-sidecar)
if [ "$sidecar_chart" != "$sidecar_pin" ]; then
  echo "values.yaml monitoring.grafana.sidecarImage is $sidecar_chart, images/Dockerfile says $sidecar_pin" >&2
  exit 1
fi

kind_pin=$(scripts/pinned-image.sh kind-node)
if ! grep -qF "node_image: $kind_pin" .github/workflows/ci.yml; then
  echo "ci.yml does not use the kind node image of images/Dockerfile: $kind_pin" >&2
  exit 1
fi

# GRP-28: the Python packages of the e2e gRPC image are pinned by version and sha256 (pip --require-hashes).
req=scripts/e2e/apps/grpc-requirements.txt
if ! grep -qx 'grpcio==.*' <(sed -n 's/ *\\$//p' "$req" | grep -v '^#' | grep -v '^ *--hash'); then
  echo "$req does not pin grpcio by version" >&2
  exit 1
fi
if [ "$(grep -c '^[a-z].*==' "$req")" != "$(grep -c '^[a-z].*==.*\\$' "$req")" ] || ! grep -q -- '--hash=sha256:[0-9a-f]\{64\}' "$req"; then
  echo "$req: every requirement must be pinned with its --hash=sha256" >&2
  exit 1
fi

# RTE-25 (#447): the Gateway API standard manifest of the e2e script is pinned once (version, URL and SHA-256),
# and the version, URL and digest agree with the spec (§2 and the RTE-25 row) and, once it names them, the wiki.
e2e=scripts/e2e.sh
[[ $(grep -c '^gw_version=' "$e2e") == 1 && $(grep -c '^gw_sha256=' "$e2e") == 1 ]] \
  || fail_rte25 "$e2e: the Gateway API version and digest are not pinned once"
gw_version=$(sed -n 's/^gw_version=//p' "$e2e")
gw_sha256=$(sed -n 's/^gw_sha256=//p' "$e2e")
[[ $gw_version == v1.7.0 ]] || fail_rte25 "$e2e: the Gateway API version is $gw_version, not v1.7.0"
[[ $gw_sha256 =~ ^[0-9a-f]{64}$ ]] || fail_rte25 "$e2e: the Gateway API digest is not 64 hex digits"
grep -qF '"https://github.com/kubernetes-sigs/gateway-api/releases/download/$gw_version/standard-install.yaml"' "$e2e" \
  || fail_rte25 "$e2e: the Gateway API manifest is not downloaded from its release URL"
spec_version=$(grep -o 'tested with \*\*v[0-9.]*\*\*' docs/SPEC.md | head -1 | grep -o 'v[0-9.]*')
[[ $spec_version == "$gw_version" ]] || fail_rte25 "docs/SPEC.md §2 names $spec_version, $e2e pins $gw_version"
spec_sha256=$(grep '^| RTE-25 ' docs/SPEC.md | grep -o '[0-9a-f]\{64\}' | head -1)
[[ $spec_sha256 == "$gw_sha256" ]] || fail_rte25 "the RTE-25 row of docs/SPEC.md names $spec_sha256, $e2e pins $gw_sha256"
if grep -q 'Gateway API (optional)' docs/wiki/Install.md; then
  wiki_version=$(grep -o 'v[0-9]\.[0-9.]*' <(sed -n '/Gateway API (optional)/,/^## /p' docs/wiki/Install.md) | head -1)
  [[ $wiki_version == "$gw_version" ]] || fail_rte25 "docs/wiki/Install.md names $wiki_version, $e2e pins $gw_version"
fi
echo "rte_25: the Gateway API manifest is pinned once at $gw_version with its SHA-256, the same in the spec and the wiki"

echo "pins: the e2e gRPC requirements are hashed; the Valkey image, the Valkey exporter image, the Prometheus and Grafana images and the kind node image match images/Dockerfile"
