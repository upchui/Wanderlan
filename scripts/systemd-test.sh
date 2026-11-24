#!/usr/bin/env bash
# Builds the systemdnode image (systemd as PID 1, the .deb installed) and
# runs wanderland under its hardened systemd unit through the steps users
# take (tests/systemd/run.sh). Arguments are passed to docker build.
set -euo pipefail
cd "$(dirname "$0")/.."
scripts/load-modules.sh
docker build -f docker/Dockerfile --target systemdnode -t "${SYSTEMDNODE_IMAGE:-wanderlan-systemdnode}" "$@" .
exec tests/systemd/run.sh
