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
