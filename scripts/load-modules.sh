#!/usr/bin/env bash
# Loads the kernel modules the integration tests need into the host kernel.
# On GitHub runners this uses sudo; elsewhere a privileged helper container
# with the host's module tree mounted, so nothing has to be installed locally.
#
#   scripts/load-modules.sh            load them
#   scripts/load-modules.sh --unload   unload ip_gre and gre again (and with
#                                      them the kernel's gre0, gretap0 and
#                                      erspan0); the other modules stay, other
#                                      software (Docker) needs them
set -euo pipefail
cd "$(dirname "$0")/.."
MODULES=(bridge 8021q ip_gre gre sch_ingress cls_flower act_vlan act_gact nf_tables veth)
GRE=(ip_gre gre)

modprobe_host() { # modprobe_host <modprobe arguments...>
  if [ "${GITHUB_ACTIONS:-}" = true ]; then
    sudo modprobe "$@"
    return
  fi
  local modules_dir
  modules_dir=$(readlink -f /lib/modules)
  docker build -q -f docker/Dockerfile --target modloader -t wanderlan-modloader . >/dev/null
  docker run --rm --privileged -v "$modules_dir:/lib/modules:ro" \
    --entrypoint modprobe wanderlan-modloader "$@"
}

if [ "${1:-}" = --unload ]; then
  # Never an error: a module another GRE tunnel still uses stays.
  if modprobe_host -r "${GRE[@]}" 2>/dev/null; then
    echo "unloaded: ${GRE[*]}"
  else
    echo "kept: ${GRE[*]} (still in use)"
  fi
  exit 0
fi
modprobe_host -a "${MODULES[@]}"
echo "loaded: ${MODULES[*]}"
