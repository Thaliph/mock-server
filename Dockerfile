# syntax=docker/dockerfile:1.10
# Deux images (spec §9.4b) :
#   - `runtime` (cible par défaut, ghcr.io/thaliph/mimic) : sidecar, contrôleur, webhook,
#     mimicctl (Job de tir de charge) et plugin CNI ; distroless, sans shell ni gestionnaire
#     de paquets ;
#   - `init` (ghcr.io/thaliph/mimic-init) : mimic-init et iptables.
# Bases épinglées par digest ; Dependabot propose les nouveaux.
ARG RUST_IMAGE=rust:1.99.1-trixie@sha256:931c2c0531a8b143e044f0f3914dfe802f15bb3d1955f0b657e9359327ac2ffc
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian13:nonroot@sha256:3ee00291cf386ec52a552e6a858d555debd74776eb4bfa53fa3df2cd6677515e
ARG INIT_IMAGE=debian:trixie-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad

FROM ${RUST_IMAGE} AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends musl-tools \
 && rm -rf /var/lib/apt/lists/*
# La chaîne de l'image fait foi : sans cela, le `channel = "stable"` de rust-toolchain.toml
# ferait télécharger la dernière version pendant la construction.
ENV RUSTUP_TOOLCHAIN=1.99.1 \
    CARGO_PROFILE_RELEASE_STRIP=symbols \
    CARGO_INCREMENTAL=0
RUN rustup target add "$(uname -m)-unknown-linux-musl"
WORKDIR /src
COPY . .
# Commit de la construction, label revision de mimic_build_info (spec §9.3).
ARG MIMIC_REVISION=unknown
ENV MIMIC_REVISION=${MIMIC_REVISION}
# Le plugin CNI est copié sur le nœud et y tourne avec la libc de l'hôte : statique (musl).
# Les autres binaires sont liés à la glibc de la base `runtime`.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    musl="$(uname -m)-unknown-linux-musl" \
 && cargo build --release --locked \
      -p mimic-proxy -p mimic-init -p mimic-controller -p mimic-injector -p mimicctl \
 && cargo build --release --locked --target "$musl" -p mimic-cni \
 && mkdir -p /out \
 && install -m 0555 target/release/mimic-proxy target/release/mimic-init \
      target/release/mimic-controller target/release/mimic-injector target/release/mimicctl \
      "target/$musl/release/mimic-cni" /out/

# mimic-init : root avec NET_ADMIN et NET_RAW par le securityContext posé par le webhook.
FROM ${INIT_IMAGE} AS init
ARG MIMIC_VERSION=0.1.0
ARG MIMIC_REVISION=unknown
LABEL org.opencontainers.image.source="https://github.com/Thaliph/mock-server" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.version="${MIMIC_VERSION}" \
      org.opencontainers.image.revision="${MIMIC_REVISION}" \
      org.opencontainers.image.description="mimic-init : règles iptables du pod injecté"
RUN apt-get update \
 && apt-get install -y --no-install-recommends iptables \
 && rm -rf /var/lib/apt/lists/* /var/cache/apt/* /var/log/*
COPY --from=build /out/mimic-init /usr/local/bin/
USER 1338:1338
ENTRYPOINT ["/usr/local/bin/mimic-init"]

# Cible par défaut, en dernier.
FROM ${RUNTIME_IMAGE} AS runtime
ARG MIMIC_VERSION=0.1.0
ARG MIMIC_REVISION=unknown
LABEL org.opencontainers.image.source="https://github.com/Thaliph/mock-server" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.version="${MIMIC_VERSION}" \
      org.opencontainers.image.revision="${MIMIC_REVISION}" \
      org.opencontainers.image.description="mimic : sidecar, contrôleur, webhook, mimicctl, plugin CNI"
COPY --from=build /out/mimic-proxy /out/mimic-controller /out/mimic-injector /out/mimicctl \
     /out/mimic-cni /usr/local/bin/
# Le sidecar tourne sous l'UID exclu des redirections (spec §3.1) ; jamais 1337, l'UID
# d'istio-proxy.
USER 1338:1338
EXPOSE 15100 15101 15106
ENTRYPOINT ["/usr/local/bin/mimic-proxy"]
