# syntax=docker/dockerfile:1
#
# All builds and tests of wanderlan run in this file's stages:
#   base      Rust toolchain, musl targets for amd64 and arm64, cargo-deb
#   check     cargo fmt --check, cargo clippy -D warnings, cargo test
#   build     release build for TARGET plus .deb package (output in /out)
#   artifacts the contents of /out only, for `docker build --output`
#   testnode  slim Debian with the built .deb installed, simulates a site
#   repotest  fresh Debian/Ubuntu that installs wanderlan from the APT repo

ARG RUST_IMAGE=rust:1.99.1-bookworm
ARG DEBIAN_IMAGE=debian:bookworm-slim

FROM ${RUST_IMAGE} AS base
ARG CARGO_DEB_VERSION=3.8.0
RUN rustup component add rustfmt clippy \
 && rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install --locked cargo-deb --version "${CARGO_DEB_VERSION}"
ENV CARGO_TERM_COLOR=always
WORKDIR /src

# Pre-build dependencies so that source changes only rebuild the workspace.
FROM base AS deps
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY .cargo .cargo
COPY crates/wanderlan-core/Cargo.toml crates/wanderlan-core/Cargo.toml
COPY crates/wanderland/Cargo.toml crates/wanderland/Cargo.toml
COPY crates/wanderlan/Cargo.toml crates/wanderlan/Cargo.toml
RUN mkdir -p crates/wanderlan-core/src crates/wanderland/src crates/wanderlan/src \
 && touch crates/wanderlan-core/src/lib.rs \
 && echo 'fn main() {}' > crates/wanderland/src/main.rs \
 && echo 'fn main() {}' > crates/wanderlan/src/main.rs \
 && cargo fetch --locked

FROM deps AS check
COPY . .
RUN --mount=type=cache,target=/src/target,id=wanderlan-target-check \
    cargo fmt --all --check \
 && cargo clippy --workspace --all-targets --locked --offline -- -D warnings \
 && cargo test --workspace --locked --offline

# Release build and .deb package. RUST_TARGET defaults to the musl target of
# the platform being built; set it to cross-compile (e.g. arm64 on amd64).
FROM deps AS build
ARG TARGETARCH
ARG RUST_TARGET=""
COPY . .
RUN --mount=type=cache,target=/src/target,id=wanderlan-target-build \
    set -eu; \
    target="${RUST_TARGET}"; \
    if [ -z "$target" ]; then \
      case "${TARGETARCH:-amd64}" in \
        amd64) target=x86_64-unknown-linux-musl ;; \
        arm64) target=aarch64-unknown-linux-musl ;; \
        *) echo "unsupported architecture ${TARGETARCH}" >&2; exit 1 ;; \
      esac; \
    fi; \
    cargo build --release --locked --offline --target "$target" -p wanderland -p wanderlan; \
    mkdir -p /out; \
    cargo deb --no-build --locked --offline --target "$target" -p wanderlan --output /out/; \
    case "$target" in x86_64*) arch=amd64 ;; aarch64*) arch=arm64 ;; esac; \
    version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1); \
    cp "target/$target/release/wanderland" "/out/wanderland-${version}-linux-${arch}"; \
    cp "target/$target/release/wanderlan" "/out/wanderlan-${version}-linux-${arch}"; \
    ls -l /out

FROM scratch AS artifacts
COPY --from=build /out/ /

# A simulated site for the integration tests: slim Debian with the package
# installed exactly as a user would install it.
FROM ${DEBIAN_IMAGE} AS testnode
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      iproute2 nftables iputils-ping tcpdump netcat-openbsd jq procps tmux \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/ /tmp/out/
RUN cd /tmp/out \
 && apt-get update \
 && apt-get install -y ./wanderlan_*.deb \
 && rm -rf /var/lib/apt/lists/* /tmp/out
COPY tests/integration/node/ /usr/local/bin/
ENTRYPOINT ["/usr/local/bin/node-entrypoint"]

# Loads the kernel modules needed by the integration tests into the host
# kernel (run privileged with /lib/modules mounted).
FROM ubuntu:24.04 AS modloader
RUN apt-get update && apt-get install -y --no-install-recommends kmod && rm -rf /var/lib/apt/lists/*
ENTRYPOINT ["modprobe", "-a"]
