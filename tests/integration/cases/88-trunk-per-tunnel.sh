# shellcheck shell=bash
# Trunk per tunnel: the tunnel to site2 uses site1's second trunk (trunk1),
# which makes it a segment of its own at site1: only trunk1's network reaches
# site2 through site1, trunk0's network reaches the other sites.

begin "Trunk per tunnel: the tunnel to site2 gets its own trunk at site1"

client_vlan c-site1b 20 10.20.0.31/24
uplink=$(x site1 "ip -o route get 10.100.2.10 | sed -n 's/.* dev \([^ ]*\).*/\1/p'")
out=$(x site1 "wanderlan trunk site2 $uplink" 2>&1)
if grep -q 'would cut the connection' <<<"$out"; then
  pass "the interface towards site2 ($uplink) is refused as its trunk"
else
  fail "unexpected output for the transport interface: $out"
fi
out=$(x site1 "wanderlan trunk site2 trunk1 --confirm" 2>&1)
if grep -q 'The tunnel to site2 uses its own trunk trunk1' <<<"$out"; then
  pass "wanderlan trunk site2 trunk1"
else
  fail "unexpected output of wanderlan trunk: $out"
fi
wait_for 20 "site1 bridges trunk1 and gt-site2 in wl-trunk1" \
  x site1 "[ \"\$(ip -j link show master wl-trunk1 | jq -r '[.[].ifname] | sort | join(\" \")')\" = 'gt-site2 trunk1' ]"
check "br0 keeps trunk0 and the other tunnels" \
  x site1 "[ \"\$(ip -j link show master br0 | jq -r '[.[].ifname] | sort | join(\" \")')\" = 'gt-site3 gt-site4 gt-site5 trunk0' ]"
check "trunk1 carries the same VLANs as trunk0" \
  test "$(vlan_summary site1 trunk1)" = "$(vlan_summary site1 trunk0)"
check "local.toml names the trunk of the tunnel" \
  x site1 "grep -A1 '^\[tunnels.site2\]' /etc/wanderlan/local.toml | grep -q 'trunk = \"trunk1\"'"
check "the status shows the trunk of the tunnel" \
  test "$(jqs site1 '.peers[] | select(.name == "site2") | .trunk')" = trunk1
check "the status lists both trunks" test "$(jqs site1 '[.trunks[].name] | join(" ")')" = "trunk0 trunk1"
check "the mesh revision did not change (a local setting)" \
  test "$(jqs site1 .revision)" = "$(jqs site2 .revision)"
expect_ping c-site1b 10.20.0.2 "VLAN 20: the client at trunk1 reaches site2"
expect_no_ping c-site1b 10.20.0.11 "VLAN 20: the client at trunk1 does not reach site3"
expect_no_ping c-site1 10.20.0.2 "VLAN 20: the client at trunk0 does not reach site2 through site1"
expect_ping c-site1 10.20.0.11 "VLAN 20: the client at trunk0 still reaches site3"
expect_ping c-site2 10.20.0.11 "VLAN 20: site2 still reaches site3 directly"

begin "Trunk per tunnel: back to the default trunk"

out=$(x site1 "wanderlan trunk site2 default --confirm" 2>&1)
if grep -q 'uses the default trunk trunk0' <<<"$out"; then
  pass "wanderlan trunk site2 default"
else
  fail "unexpected output of wanderlan trunk: $out"
fi
wait_for 20 "the bridge wl-trunk1 is gone" x site1 "! ip link show wl-trunk1 2>/dev/null"
check "trunk1 is free again" x site1 "! ip -j link show trunk1 | jq -e '.[0].master'"
expect_ping c-site1 10.20.0.2 "VLAN 20: the client at trunk0 reaches site2 again"
expect_no_ping c-site1b 10.20.0.2 "VLAN 20: the client at trunk1 is cut off again"
client_vlan_del c-site1b 20
