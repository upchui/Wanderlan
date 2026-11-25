# shellcheck shell=bash
# Two sites: one GRETAP tunnel, bridge with VLAN filtering, ping in VLAN 20.

begin "Two sites: tunnel, VLAN filtering and ping in VLAN 20"

network_toml "10-50" site1 site2 >"$RUN_DIR/network-2.toml"
for s in site1 site2; do
  configure "$s" "$RUN_DIR/network-2.toml"
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

# Identities: every site claimed one at its first start; the tunnel has the
# key of the two and came up once each side answered with its key.
for pair in "site1 site2" "site2 site1"; do
  set -- $pair
  wait_eq 30 "$1 confirmed the identity of $2 (tunnel with a key)" "confirmed gre_key" \
    jqs "$1" ".peers[] | select(.name == \"$2\") | \"\\(.identity_state) \\(.tunnel)\""
done
k1=$(x site1 "ip -d link show gt-site2 | grep -o 'ikey [0-9.]* okey [0-9.]*'")
k2=$(x site2 "ip -d link show gt-site1 | grep -o 'ikey [0-9.]* okey [0-9.]*'")
check "the tunnel has a GRE key, the same at both ends ($k1)" test -n "$k1" -a "$k1" = "$k2"
# Written once the synced change is confirmed (a moment after it applies).
wait_for 30 "site1's network.toml lists both identities" \
  eval '[ "$(x site1 "grep -c \"^id = \" /etc/wanderlan/network.toml")" = 2 ]'
check "the probes are authenticated (version 2)" \
  x site1 "timeout 8 tcpdump -c 1 -nli any -A 'udp port 51821' 2>/dev/null | grep -q WLP2"

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
  x site1 "nft list table bridge wanderlan 2>/dev/null | grep -q 'oifname \"gt-site2\" .*maxseg size set 1414'"
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

begin "All VLANs and untagged: like a trunk between two switches"

network_toml "" site1 site2 >"$RUN_DIR/network-2-all.toml"
for s in site1 site2; do
  configure "$s" "$RUN_DIR/network-2-all.toml"
  x "$s" "wl-daemon reload"
done
wait_eq 20 "site1: trunk0 carries every VLAN, untagged as native VLAN" "1:pu,2-4094" \
  vlan_summary site1 trunk0
expect_vlans site1 gt-site2 "1-4094"
check "status shows all VLANs" test "$(jqs site1 .vlans)" = all
for vid in 60 4000; do
  client_vlan c-site1 "$vid" "10.61.$((vid % 250)).1/24"
  client_vlan c-site2 "$vid" "10.61.$((vid % 250)).2/24"
  expect_ping c-site1 "10.61.$((vid % 250)).2" "VLAN $vid is bridged"
  client_vlan_del c-site1 "$vid"
  client_vlan_del c-site2 "$vid"
done
client_native c-site1 10.99.0.1/24
client_native c-site2 10.99.0.2/24
expect_ping c-site1 10.99.0.2 "untagged traffic is bridged (native)"
client_native_del c-site1
client_native_del c-site2

# Back to VLANs 10-50 for the following cases.
renew "$RUN_DIR/network-2.toml"
for s in site1 site2; do
  configure "$s" "$RUN_DIR/network-2.toml"
  x "$s" "wl-daemon reload"
done
wait_eq 20 "site1: trunk0 carries VLANs 10-50 again" "10-50" vlan_summary site1 trunk0
