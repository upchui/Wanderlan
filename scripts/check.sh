#!/usr/bin/env bash
# Runs cargo fmt --check, cargo clippy -D warnings and cargo test in Docker.
set -euo pipefail
cd "$(dirname "$0")/.."
exec docker build -f docker/Dockerfile --target check -t wanderlan-check "$@" .
