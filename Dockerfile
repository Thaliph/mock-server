# Image unique : le sidecar (mimic-proxy) et l'init container (mimic-init).
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p mimic-proxy -p mimic-init

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates iptables \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/mimic-proxy /src/target/release/mimic-init /usr/local/bin/
# Le sidecar tourne sous l'UID exclu des redirections (spec §3.1) ; l'init container
# repasse en root avec NET_ADMIN dans son securityContext.
USER 1337:1337
EXPOSE 15000 15001 15006
ENTRYPOINT ["/usr/local/bin/mimic-proxy"]
