# Image unique : le sidecar (mimic-proxy), l'init container (mimic-init), le contrôleur,
# le webhook d'injection et mimicctl (tir de charge depuis un Job, spec §5.9).
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p mimic-proxy -p mimic-init -p mimic-controller -p mimic-injector -p mimicctl

FROM debian:bookworm-slim
LABEL org.opencontainers.image.source="https://github.com/Thaliph/mock-server" \
      org.opencontainers.image.licenses="Apache-2.0"
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates iptables \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/mimic-proxy /src/target/release/mimic-init /src/target/release/mimic-controller \
     /src/target/release/mimic-injector /src/target/release/mimicctl /usr/local/bin/
# Le sidecar tourne sous l'UID exclu des redirections (spec §3.1) ; l'init container
# repasse en root avec NET_ADMIN dans son securityContext.
USER 1338:1338
EXPOSE 15100 15101 15106
ENTRYPOINT ["/usr/local/bin/mimic-proxy"]
