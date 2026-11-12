# Digest pins of the images that the chart's default and the end-to-end tests use, outside the product
# Dockerfile (#273). Dependabot (docker ecosystem, directory /images) updates every FROM line; this
# file is never built. Each stage name is the key that scripts read:
#   scripts/pinned-image.sh <stage>   prints the pinned reference (scripts/e2e.sh uses it)
#   scripts/check-pins.sh             checks that values.yaml and ci.yml carry the same references
# Changing a digest here is the only way to change these images.

# Valkey exporter (#378): the chart's valkey.metrics image (deploy/helm/mimic/values.yaml), v1.95.0 published 2026-10-21.
FROM docker.io/oliver006/redis_exporter:v1.95.0@sha256:afb21407aba3df1b4010899f155fc6b1ba2026c97b725d72aca4019d772e7b6b AS redis-exporter

# Bundled Prometheus (#379, spec §10.7): the chart's monitoring.prometheus.image, v3.15.1 published 2026-10-21
# (GitHub release), outside the 14-day cooldown.
FROM quay.io/prometheus/prometheus:v3.15.1@sha256:fa3302279710be2f21a8301ec17a8b4f2301fd829bd113839b3b6b02b6f191a2 AS prometheus

# Bundled Grafana (#379, spec §10.7): the chart's monitoring.grafana.image, v13.2.4 published 2026-10-13 (GitHub
# release), outside the 14-day cooldown.
FROM docker.io/grafana/grafana:13.2.4-distroless@sha256:ae3d3add290adbe6a88412229f5d76fd7bb82743050881698642d3d62f277e35 AS grafana

# Dashboard sidecar of the bundled Grafana (#379): the chart's monitoring.grafana.sidecarImage, 2.14.1 published
# 2026-10-08 (GitHub release), outside the 14-day cooldown.
FROM quay.io/kiwigrid/k8s-sidecar:2.14.1@sha256:092dbe0c335fba9af414a27630ca68f34df6185784fa847d9954187e69dbfdd7 AS k8s-sidecar

# Valkey: the chart's default (deploy/helm/mimic/values.yaml, valkey.image), also scanned by Trivy in CI.
FROM docker.io/valkey/valkey:9.0-alpine@sha256:2d04268dfa26ab31cfa3fdc538866e76438b2d00e41db41b8cb86d780ac5a416 AS valkey

# kind node image of the Kubernetes floor of the e2e matrix (.github/workflows/ci.yml).
FROM kindest/node:v1.33.12@sha256:3f5c8443c620245e4d355cfe09e96a91ead32ceaa569d3f1ca9edf0cb2fe2ff4 AS kind-node

# e2e: the application image of the scenarios (Python with a shell).
FROM docker.io/library/python:3.13-alpine@sha256:7984cf56ceaddebe4fefeb75d64000b9f45d8b77b11a09b24524da6cdc3af0c8 AS e2e-app

# e2e: base images of the clients, with the e2e CA added to their trust store.
FROM curlimages/curl:8.22.0@sha256:58adaa4e8dca9c988bae2aba4ab3434a0bb2da16bbe3f92dec39ec7785166777 AS e2e-curl
FROM python:3.13-slim@sha256:2d44738ebf4ce5f63b9d9ea871e9c7cfdc0426f7b0c97cac915fd88ded1d86bb AS e2e-python
FROM node:22-slim@sha256:4b150b48b4116e2a22f2c39cc27025fbf4f153e2078fd3b14d159fdb5c7279da AS e2e-node
FROM eclipse-temurin:21-jdk@sha256:ebfbb18d11ff163ee83b21258acf12248af48d22a0fdfdc68fa1a1516c05976a AS e2e-jdk

# e2e: headless Chromium of the UI check (UI-12, scripts/e2e/ui_browser.py). The digest is the multi-arch index
# of the pinned tag. Dependabot proposes later releases after its cooldown.
FROM docker.io/chromedp/headless-shell:158.0.8106.0@sha256:7e9783a01393515a5b1998a92a1ae46959f56ad3e05de8b9c0500f2e24fc3887 AS e2e-chromium

# Benchmarks (#436, scripts/bench.sh): fortio v1.76.0, the load generator and the echo server of the baseline, published
# 2026-10-11 (GitHub release), outside the 14-day cooldown.
FROM docker.io/fortio/fortio:1.76.0@sha256:3dd31ddeb4f962a1dae8cc2a0bce62c408ac830c8435bbea51b1e0fd4c8f7181 AS fortio
