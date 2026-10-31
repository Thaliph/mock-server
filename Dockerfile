# syntax=docker/dockerfile:1
# One image for mimic-sidecar and mimic-controller (spec §10): distroless, non-root, no shell.
# Base images are pinned by digest; Dependabot keeps them current.
ARG RUST_IMAGE=rust:1.99.1-trixie@sha256:931c2c0531a8b143e044f0f3914dfe802f15bb3d1955f0b657e9359327ac2ffc
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian13:nonroot@sha256:3ee00291cf386ec52a552e6a858d555debd74776eb4bfa53fa3df2cd6677515e

FROM ${RUST_IMAGE} AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates crates
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked --bin mimic-sidecar --bin mimic-controller \
    && mkdir /out && cp target/release/mimic-sidecar target/release/mimic-controller /out/

FROM ${RUNTIME_IMAGE}
COPY --from=build /out/ /usr/local/bin/
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/mimic-controller"]
