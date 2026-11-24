#!/usr/bin/env bash
# Builds the testnode image (with the .deb installed) and runs the
# integration tests. Arguments are passed to docker build.
set -euo pipefail
cd "$(dirname "$0")/.."
scripts/load-modules.sh
docker build -f docker/Dockerfile --target testnode -t "${TESTNODE_IMAGE:-wanderlan-testnode}" "$@" .
exec tests/integration/run.sh
