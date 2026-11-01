# Digest pins of the images that the chart's default and the end-to-end tests use, outside the product
# Dockerfile (#273). Dependabot (docker ecosystem, directory /images) updates every FROM line; this
# file is never built. Each stage name is the key that scripts read:
#   scripts/pinned-image.sh <stage>   prints the pinned reference (scripts/e2e.sh uses it)
#   scripts/check-pins.sh             checks that values.yaml and ci.yml carry the same references
# Changing a digest here is the only way to change these images.

# Valkey: the chart's default (deploy/helm/mimic/values.yaml, valkey.image), also scanned by Trivy in CI.
FROM docker.io/valkey/valkey:9.0-alpine@sha256:2d04268dfa26ab31cfa3fdc538866e76438b2d00e41db41b8cb86d780ac5a416 AS valkey

# kind node image of the Kubernetes floor of the e2e matrix (.github/workflows/ci.yml).
FROM kindest/node:v1.37.0@sha256:a1ed56cfb0e7b93589bdf97c8cd566405a265939e3620fc4f5de89adff580ae5 AS kind-node

# e2e: the application image of the scenarios (Python with a shell).
FROM docker.io/library/python:3.13-alpine@sha256:7984cf56ceaddebe4fefeb75d64000b9f45d8b77b11a09b24524da6cdc3af0c8 AS e2e-app

# e2e: base images of the clients, with the e2e CA added to their trust store.
FROM curlimages/curl:8.11.1@sha256:c1fe1679c34d9784c1b0d1e5f62ac0a79fca01fb6377cdd33e90473c6f9f9a69 AS e2e-curl
FROM python:3.13-slim@sha256:2d44738ebf4ce5f63b9d9ea871e9c7cfdc0426f7b0c97cac915fd88ded1d86bb AS e2e-python
FROM node:22-slim@sha256:4b150b48b4116e2a22f2c39cc27025fbf4f153e2078fd3b14d159fdb5c7279da AS e2e-node
FROM eclipse-temurin:21-jdk@sha256:ebfbb18d11ff163ee83b21258acf12248af48d22a0fdfdc68fa1a1516c05976a AS e2e-jdk
