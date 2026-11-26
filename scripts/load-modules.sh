#!/usr/bin/env bash
# Loads the kernel modules the integration tests need into the host kernel.
# On GitHub runners this uses sudo; elsewhere a privileged helper container
# with the host's module tree mounted, so nothing has to be installed locally.
set -euo pipefail
cd "$(dirname "$0")/.."
MODULES=(bridge 8021q vxlan sch_ingress cls_flower act_vlan act_gact nf_tables veth)
# For the NAT router and the local breakout of the integration tests; built in
# on some kernels.
NAT_MODULES=(nf_conntrack nf_nat nft_masq nft_chain_nat nft_ct nft_meta_bridge)

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

modprobe_host -a "${MODULES[@]}"
echo "loaded: ${MODULES[*]}"
for module in "${NAT_MODULES[@]}"; do
  modprobe_host "$module" 2>/dev/null || true
done
