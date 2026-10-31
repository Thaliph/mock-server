# syntax=docker/dockerfile:1
# Two images (spec §10), both distroless, non-root by default, no shell:
#   - `mimic` (default target): mimic-sidecar and mimic-controller.
#   - `mimic-init` (--target init): mimic-init plus iptables-restore and ip6tables-restore
#     (nft backend) and the libraries they load. The pod runs it as UID 0 with NET_ADMIN/NET_RAW.
# Base images are pinned by digest; Dependabot keeps them current.
ARG RUST_IMAGE=rust:1.99.1-trixie@sha256:931c2c0531a8b143e044f0f3914dfe802f15bb3d1955f0b657e9359327ac2ffc
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian13:nonroot@sha256:3ee00291cf386ec52a552e6a858d555debd74776eb4bfa53fa3df2cd6677515e

FROM ${RUST_IMAGE} AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates crates
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked --bin mimic-sidecar --bin mimic-controller --bin mimic-init \
    && mkdir /out /out-init \
    && cp target/release/mimic-sidecar target/release/mimic-controller /out/ \
    && cp target/release/mimic-init /out-init/

# The iptables user space with the nft backend, and every shared library it loads at run time
# (the libraries of the binary and of the xtables extensions). Copied into the init image, not
# into the default one.
FROM ${RUST_IMAGE} AS netfilter
RUN apt-get update \
    && apt-get install -y --no-install-recommends iptables \
    && rm -rf /var/lib/apt/lists/*
RUN set -eux; \
    root=/rootfs; \
    multiarch="$(gcc -print-multiarch)"; \
    xtables="/usr/lib/$multiarch/xtables"; \
    mkdir -p "$root/usr/sbin" "$root$xtables" "$root/etc"; \
    cp /usr/sbin/xtables-nft-multi "$root/usr/sbin/"; \
    cp -L /etc/protocols "$root/etc/"; \
    ln -s xtables-nft-multi "$root/usr/sbin/iptables-restore"; \
    ln -s xtables-nft-multi "$root/usr/sbin/ip6tables-restore"; \
    cp "$xtables"/*.so "$root$xtables/"; \
    for file in /usr/sbin/xtables-nft-multi "$xtables"/*.so; do \
      ldd "$file" | awk '/=>/ { print $3 }'; \
    done | sort -u | while read -r lib; do \
      case "$lib" in */libc.so.6 | */libgcc_s.so.1) ;; \
        *) dir="$root$(readlink -f "$(dirname "$lib")")"; mkdir -p "$dir"; cp -L "$lib" "$dir/" ;; \
      esac; \
    done; \
    test -e "$root/usr/sbin/xtables-nft-multi"

FROM ${RUNTIME_IMAGE} AS init
COPY --from=netfilter /rootfs/ /
COPY --from=build /out-init/mimic-init /usr/local/bin/mimic-init
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/mimic-init"]

# Default target, last so that a plain `docker build` produces it.
FROM ${RUNTIME_IMAGE} AS app
COPY --from=build /out/ /usr/local/bin/
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/mimic-controller"]
