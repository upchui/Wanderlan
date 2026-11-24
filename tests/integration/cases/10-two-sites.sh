# shellcheck shell=bash
# Two sites: one GRETAP tunnel, bridge with VLAN filtering, ping in VLAN 20.

begin "Two sites: tunnel, VLAN filtering and ping in VLAN 20"

mesh_toml "10-50" site1 site2 >"$RUN_DIR/mesh-2.toml"
for s in site1 site2; do
  configure "$s" "$RUN_DIR/mesh-2.toml"
  x "$s" "wl-daemon start"
done

wait_for 30 "gt-site2 is up on site1" x site1 "ip -d link show gt-site2 | grep -q 'state UNKNOWN\|state UP'"
wait_for 30 "gt-site1 is up on site2" x site2 "ip -d link show gt-site1 | grep -q 'state UNKNOWN\|state UP'"

check "gt-site2 is a GRETAP tunnel 10.100.1.10 -> 10.100.2.10 without PMTU discovery" \
  x site1 "ip -d link show gt-site2 | grep -q 'gretap remote 10.100.2.10 local 10.100.1.10 .*nopmtudisc.*ignore-df'"
check "gt-site2 has MTU 1500" x site1 "ip link show gt-site2 | grep -q 'mtu 1500'"
check "br0 has VLAN filtering on and STP off" \
  x site1 "ip -d link show br0 | grep -q 'stp_state 0 .*vlan_filtering 1'"
check "trunk0 and gt-site2 are ports of br0" \
  x site1 "[ \"\$(ip -j link show master br0 | jq -r '[.[].ifname] | sort | join(\" \")')\" = 'gt-site2 trunk0' ]"
check "gt-site2 is isolated (split horizon)" \
  x site1 "bridge -d link show dev gt-site2 | grep -q 'isolated on'"
expect_vlans site1 trunk0 "10-50"
expect_vlans site1 gt-site2 "10-50"
check "br0 itself has no VLANs" \
  x site1 "[ \"\$(bridge -j vlan show dev br0 | jq '[.[]?.vlans[]?] | length')\" = 0 ]"

client_vlan c-site1 20 10.20.0.1/24
client_vlan c-site2 20 10.20.0.2/24
expect_ping c-site1 10.20.0.2 "ping in VLAN 20 from site1's client to site2's client"

client_vlan c-site1 60 10.60.0.1/24
client_vlan c-site2 60 10.60.0.2/24
expect_no_ping c-site1 10.60.0.2 "VLAN 60 (outside 10-50) is not bridged"
client_vlan_del c-site1 60
client_vlan_del c-site2 60

client_native c-site1 10.99.0.1/24
client_native c-site2 10.99.0.2/24
expect_no_ping c-site1 10.99.0.2 "untagged traffic is not bridged"
client_native_del c-site1
client_native_del c-site2

# Idempotence: reloading the same configuration changes nothing. Wait for
# the first path MTU measurement first, it installs the MSS rules.
wait_for 60 "site1 clamps the MSS on gt-site2 after measuring the path MTU" \
  x site1 "nft list table bridge wanderlan 2>/dev/null | grep -q 'oifname \"gt-site2\" .*maxseg size set 1418'"
mark=$(daemon_log_lines site1)
x site1 "wl-daemon reload"
sleep 8
changes=$(daemon_log_since site1 "$mark" | grep -cE "$CHANGE_RE" || true)
if [ "$changes" = 0 ]; then
  pass "reloading the same configuration changes nothing"
else
  fail "reloading the same configuration made $changes changes"
  daemon_log_since site1 "$mark"
fi
