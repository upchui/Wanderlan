# shellcheck shell=bash
# Stopping the daemon removes everything it created.

begin "Stopping the daemon removes all interfaces it created"

for s in site1 site2 site3 site4 site5 natc1 natc2; do
  x "$s" "wl-daemon status" >/dev/null 2>&1 || continue
  x "$s" "wl-daemon stop"
  check "$s: no br0 and no tunnels left" \
    x "$s" "! ip link show br0 2>/dev/null && [ -z \"\$(ip -o link show | grep ' vx')\" ]"
  check "$s: trunk0 still exists and is detached" \
    x "$s" "ip -j link show trunk0 | jq -e '.[0].master == null'"
  check "$s: no nftables table bridge wanderlan" \
    x "$s" "! nft list table bridge wanderlan 2>/dev/null"
  check "$s: no nftables table ip wanderlan" \
    x "$s" "! nft list table ip wanderlan 2>/dev/null"
  check "$s: no breakout interfaces, tables or routing rules" \
    x "$s" "[ -z \"\$(ip -o link show | grep -E ' wlb[op][0-9]')\" ] && ! nft list table inet wanderlan-breakout 2>/dev/null && ! ip rule show | grep -q 'proto 87'"
  check "$s: no tc filters on trunk0" \
    x "$s" "[ -z \"\$(tc filter show dev trunk0 ingress 2>/dev/null)\" ] && [ -z \"\$(tc filter show dev trunk0 egress 2>/dev/null)\" ]"
done
