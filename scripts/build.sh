#!/usr/bin/env bash
# Builds the release binaries and the .deb for amd64 or arm64 into dist/.
# Usage: scripts/build.sh [amd64|arm64]
# The version and commit are those of the checkout (scripts/version.sh);
# WANDERLAN_VERSION and WANDERLAN_COMMIT in the environment override them.
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
  --build-arg WANDERLAN_VERSION="${WANDERLAN_VERSION:-$(scripts/version.sh)}" \
  --build-arg WANDERLAN_COMMIT="${WANDERLAN_COMMIT:-$(scripts/version.sh --commit)}" \
  --output "type=local,dest=dist" .
ls -l dist
