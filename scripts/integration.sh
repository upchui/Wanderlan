#!/usr/bin/env bash
# Builds the testnode image (with the .deb installed) and runs the
# integration tests. Arguments are passed to docker build.
set -euo pipefail
cd "$(dirname "$0")/.."
# The GRE module only for the time of the tests (it brings the kernel's
# gre0, gretap0 and erspan0), unless it was loaded before or the
# containers are kept.
gre_was_loaded=$([ -d /sys/module/ip_gre ] && echo yes || true)
unload_gre() {
  if [ -z "$gre_was_loaded" ] && [ "${KEEP:-0}" != 1 ]; then
    scripts/load-modules.sh --unload
  fi
}
trap unload_gre EXIT
scripts/load-modules.sh
docker build -f docker/Dockerfile --target testnode -t "${TESTNODE_IMAGE:-wanderlan-testnode}" \
  --build-arg WANDERLAN_VERSION="$(scripts/version.sh)" \
  --build-arg WANDERLAN_COMMIT="$(scripts/version.sh --commit)" "$@" .
tests/integration/run.sh
