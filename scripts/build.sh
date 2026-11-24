#!/usr/bin/env bash
# Builds the release binaries and the .deb for amd64 or arm64 into dist/.
# Usage: scripts/build.sh [amd64|arm64]
# WANDERLAN_VERSION in the environment overrides the version from Cargo.toml.
set -euo pipefail
cd "$(dirname "$0")/.."
arch="${1:-amd64}"
case "$arch" in
  amd64) target=x86_64-unknown-linux-musl ;;
  arm64) target=aarch64-unknown-linux-musl ;;
  *) echo "usage: $0 [amd64|arm64]" >&2; exit 2 ;;
esac
mkdir -p dist
docker build -f docker/Dockerfile --target artifacts --build-arg RUST_TARGET="$target" \
  --build-arg WANDERLAN_VERSION="${WANDERLAN_VERSION:-}" --output "type=local,dest=dist" .
ls -l dist
