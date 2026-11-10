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

echo "pins: the Valkey image, the Valkey exporter image, the Prometheus and Grafana images and the kind node image match images/Dockerfile"
