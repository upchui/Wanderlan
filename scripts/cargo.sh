#!/usr/bin/env bash
# Runs cargo inside the wanderlan base image with the repository mounted.
# Usage: scripts/cargo.sh <cargo arguments...>
# Example: scripts/cargo.sh test -p wanderlan-core
set -euo pipefail
cd "$(dirname "$0")/.."
docker build -q -f docker/Dockerfile --target base -t wanderlan-base . >/dev/null
docker volume create wanderlan-cargo-registry >/dev/null
docker run --rm -t \
  --user "$(id -u):$(id -g)" \
  -e CARGO_HOME=/cargo -e CARGO_TERM_COLOR=always \
  -v wanderlan-cargo-registry:/cargo \
  -v "$PWD":/src -w /src \
  wanderlan-base cargo "$@"
