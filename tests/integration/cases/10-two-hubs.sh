# shellcheck shell=bash
# Two hubs: one GRETAP tunnel, bridge with VLAN filtering, ping in VLAN 20.

begin "Two hubs: tunnel, VLAN filtering and ping in VLAN 20"

mesh_toml "10-50" hub1:hub hub2:hub >"$RUN_DIR/mesh-2hubs.toml"
for s in hub1 hub2; do
  configure "$s" "$RUN_DIR/mesh-2hubs.toml"
  x "$s" "wl-daemon start"
done

wait_for 30 "gt-hub2 is up on hub1" x hub1 "ip -d link show gt-hub2 | grep -q 'state UNKNOWN\|state UP'"
wait_for 30 "gt-hub1 is up on hub2" x hub2 "ip -d link show gt-hub1 | grep -q 'state UNKNOWN\|state UP'"

check "gt-hub2 is a GRETAP tunnel 10.100.1.10 -> 10.100.2.10 without PMTU discovery" \
  x hub1 "ip -d link show gt-hub2 | grep -q 'gretap remote 10.100.2.10 local 10.100.1.10 .*nopmtudisc.*ignore-df'"
check "gt-hub2 has MTU 1500" x hub1 "ip link show gt-hub2 | grep -q 'mtu 1500'"
check "br0 has VLAN filtering on and STP off" \
  x hub1 "ip -d link show br0 | grep -q 'stp_state 0 .*vlan_filtering 1'"
check "trunk0 and gt-hub2 are ports of br0" \
  x hub1 "[ \"\$(ip -j link show master br0 | jq -r '[.[].ifname] | sort | join(\" \")')\" = 'gt-hub2 trunk0' ]"
check "gt-hub2 is isolated (split horizon between hubs)" \
  x hub1 "bridge -d link show dev gt-hub2 | grep -q 'isolated on'"
expect_vlans hub1 trunk0 "10-50"
expect_vlans hub1 gt-hub2 "10-50"
check "br0 itself has no VLANs" \
  x hub1 "[ \"\$(bridge -j vlan show dev br0 | jq '[.[]?.vlans[]?] | length')\" = 0 ]"

client_vlan c-hub1 20 10.20.0.1/24
client_vlan c-hub2 20 10.20.0.2/24
expect_ping c-hub1 10.20.0.2 "ping in VLAN 20 from hub1's client to hub2's client"

client_vlan c-hub1 60 10.60.0.1/24
client_vlan c-hub2 60 10.60.0.2/24
expect_no_ping c-hub1 10.60.0.2 "VLAN 60 (outside 10-50) is not bridged"
client_vlan_del c-hub1 60
client_vlan_del c-hub2 60

client_native c-hub1 10.99.0.1/24
client_native c-hub2 10.99.0.2/24
expect_no_ping c-hub1 10.99.0.2 "untagged traffic is not bridged"
client_native_del c-hub1
client_native_del c-hub2

# Idempotence: reloading the same configuration changes nothing. Wait for
# the first path MTU measurement first, it installs the MSS rules.
wait_for 60 "hub1 clamps the MSS on gt-hub2 after measuring the path MTU" \
  x hub1 "nft list table bridge wanderlan 2>/dev/null | grep -q 'oifname \"gt-hub2\" .*maxseg size set 1418'"
mark=$(daemon_log_lines hub1)
x hub1 "wl-daemon reload"
sleep 8
changes=$(daemon_log_since hub1 "$mark" | grep -cE "$CHANGE_RE" || true)
if [ "$changes" = 0 ]; then
  pass "reloading the same configuration changes nothing"
else
  fail "reloading the same configuration made $changes changes"
  daemon_log_since hub1 "$mark"
fi
