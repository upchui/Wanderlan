#!/usr/bin/env bash
# Loads the kernel modules the integration tests need into the host kernel.
# On GitHub runners this uses sudo; elsewhere a privileged helper container
# with the host's module tree mounted, so nothing has to be installed locally.
set -euo pipefail
cd "$(dirname "$0")/.."
MODULES=(bridge 8021q ip_gre gre sch_ingress cls_flower act_vlan act_gact nf_tables veth)

if [ "${GITHUB_ACTIONS:-}" = true ]; then
  sudo modprobe -a "${MODULES[@]}"
  exit 0
fi
modules_dir=$(readlink -f /lib/modules)
docker build -q -f docker/Dockerfile --target modloader -t wanderlan-modloader . >/dev/null
docker run --rm --privileged -v "$modules_dir:/lib/modules:ro" wanderlan-modloader "${MODULES[@]}"
echo "loaded: ${MODULES[*]}"
