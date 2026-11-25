# syntax=docker/dockerfile:1
#
# All builds and tests of wanderlan run in this file's stages:
#   base      Rust toolchain, musl targets for amd64 and arm64, cargo-deb
#   check     cargo fmt --check, cargo clippy -D warnings, cargo test
#   build     release build for TARGET plus .deb package (output in /out)
#   artifacts the contents of /out only, for `docker build --output`
#   testnode  slim Debian with the built .deb installed, simulates a site
#   modloader loads the kernel modules for the integration tests
#   aptrepo   apt-ftparchive and gpg to build the APT repository
#   repotest  fresh Debian/Ubuntu that installs wanderlan from the APT repo
#   repoe2e   repotest plus test tools, for two hosts with the example config

ARG RUST_IMAGE=rust:1.99.1-trixie
ARG DEBIAN_IMAGE=debian:trixie-slim
ARG REPOTEST_BASE=debian:13

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
# WANDERLAN_VERSION overrides the version from Cargo.toml (binaries, package
# version and file names: the package is named exactly like the version);
# WANDERLAN_COMMIT is the commit `--version` names. Both come from
# scripts/version.sh, in CI and in the local scripts.
FROM deps AS build
ARG TARGETARCH
ARG RUST_TARGET=""
ARG WANDERLAN_VERSION=""
ARG WANDERLAN_COMMIT=""
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
    version="${WANDERLAN_VERSION}"; \
    if [ -z "$version" ]; then \
      version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1); \
    fi; \
    export WANDERLAN_VERSION="$version" WANDERLAN_COMMIT="${WANDERLAN_COMMIT}"; \
    cargo build --release --locked --offline --target "$target" -p wanderland -p wanderlan; \
    mkdir -p /out; \
    cargo deb --no-build --locked --offline --target "$target" -p wanderlan \
      --deb-version "${version}" --output /out/; \
    case "$target" in x86_64*) arch=amd64 ;; aarch64*) arch=arm64 ;; esac; \
    cp "target/$target/release/wanderland" "/out/wanderland-${version}-linux-${arch}"; \
    cp "target/$target/release/wanderlan" "/out/wanderlan-${version}-linux-${arch}"; \
    ls -l /out

FROM scratch AS artifacts
COPY --from=build /out/ /

# A simulated site for the integration tests: slim Debian 13 with the package
# installed exactly as a user would install it.
FROM ${DEBIAN_IMAGE} AS testnode
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      iproute2 nftables iputils-ping tcpdump netcat-openbsd jq procps tmux conntrack iperf3 \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/ /tmp/out/
RUN cd /tmp/out \
 && apt-get update \
 && apt-get install -y ./wanderlan_*.deb \
 && rm -rf /var/lib/apt/lists/* /tmp/out
COPY tests/integration/node/ /usr/local/bin/
ENTRYPOINT ["/usr/local/bin/node-entrypoint"]

# A site that runs wanderland exactly like a real host: systemd as PID 1, the
# hardened unit from the package (tests/systemd). Run privileged with a
# private cgroup namespace.
FROM ${DEBIAN_IMAGE} AS systemdnode
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      systemd systemd-sysv dbus iproute2 nftables iputils-ping jq procps kmod \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/ /tmp/out/
RUN cd /tmp/out \
 && apt-get update \
 && apt-get install -y ./wanderlan_*.deb \
 && mkdir -p /var/cache/wanderlan-test \
 && cp ./wanderlan_*.deb /var/cache/wanderlan-test/wanderlan.deb \
 && rm -rf /var/lib/apt/lists/* /tmp/out \
 # Like a real host: package scripts may start and stop services (the
 # Docker image forbids it, for builds).
 && rm -f /usr/sbin/policy-rc.d
COPY tests/integration/node/ /usr/local/bin/
COPY tests/systemd/wl-node-setup /usr/local/bin/
COPY tests/systemd/wl-node-setup.service /etc/systemd/system/
RUN systemctl enable wl-node-setup.service wanderland.service \
 && systemctl mask getty.target console-getty.service systemd-firstboot.service
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]

# Loads the kernel modules needed by the integration tests into the host
# kernel (run privileged with /lib/modules mounted).
FROM ubuntu:24.04 AS modloader
RUN apt-get update && apt-get install -y --no-install-recommends kmod && rm -rf /var/lib/apt/lists/*
ENTRYPOINT ["modprobe", "-a"]

# Tools to build and sign the APT repository (apt-ftparchive, gpg).
FROM ${DEBIAN_IMAGE} AS aptrepo
RUN apt-get update \
 && apt-get install -y --no-install-recommends apt-utils gnupg gzip ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# A fresh Debian/Ubuntu that installs wanderlan from the published APT
# repository with exactly the commands from the README. MIN_VERSION: the
# installed (newest) version must be at least this one.
FROM ${REPOTEST_BASE} AS repotest
ARG REPO_URL=https://upchui.github.io/Wanderlan
ARG MIN_VERSION=""
ARG CACHE_BUST=""
ENV DEBIAN_FRONTEND=noninteractive
# Prerequisites a minimal container lacks (a normal installation has them),
# and "yes" to apt's question so the README commands run unchanged.
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl gpg ca-certificates sudo \
 && rm -rf /var/lib/apt/lists/* \
 && echo 'APT::Get::Assume-Yes "true";' > /etc/apt/apt.conf.d/90assume-yes
RUN echo "repotest ${CACHE_BUST}" \
 && curl -fsSL ${REPO_URL}/key.gpg | sudo gpg --dearmor -o /usr/share/keyrings/wanderlan.gpg \
 && echo "deb [signed-by=/usr/share/keyrings/wanderlan.gpg] ${REPO_URL}/apt stable main" | sudo tee /etc/apt/sources.list.d/wanderlan.list \
 && sudo apt update && sudo apt install wanderlan
RUN wanderlan --version && wanderland --version \
 && installed=$(dpkg-query -W -f '${Version}' wanderlan) \
 && echo "installed wanderlan ${installed}" \
 && if [ -n "${MIN_VERSION}" ]; then dpkg --compare-versions "${installed}" ge "${MIN_VERSION}"; fi \
 && getent group wanderlan \
 && test -f /lib/systemd/system/wanderland.service -o -f /usr/lib/systemd/system/wanderland.service \
 && wanderlan init | grep -q '^\[network\]' 

# The repository installation plus test tools, to run two hosts with the
# example configuration (tests/repotest).
FROM repotest AS repoe2e
RUN apt-get update \
 && apt-get install -y --no-install-recommends iproute2 iputils-ping procps jq \
 && rm -rf /var/lib/apt/lists/*
COPY tests/integration/node/ /usr/local/bin/
ENTRYPOINT ["/usr/local/bin/node-entrypoint"]
