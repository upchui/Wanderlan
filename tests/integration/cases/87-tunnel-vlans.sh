# shellcheck shell=bash
# VLANs per tunnel: a tunnel carries at most the VLANs it is given, on top
# of the VLANs both of its sites take part in.

begin "VLANs per tunnel: a limit narrows what one tunnel carries"

before=$(vlan_summary site1 gt-site3)
check "site1 and site2 reach each other in VLAN 20 before" ping_once c-site1 10.20.0.2
out=$(x site1 "wanderlan tunnel-vlans site1 site2 10-19,21-50 --confirm" 2>&1)
if grep -q 'The tunnel between site1 and site2 carries VLANs 10-19,21-50 (limited to 10-19,21-50)' <<<"$out"; then
  pass "wanderlan tunnel-vlans site1 site2 10-19,21-50"
else
  fail "unexpected output of wanderlan tunnel-vlans: $out"
fi
wait_eq 30 "site1's tunnel to site2 carries 10-19,21-50" "10-19,21-50" vlan_summary site1 gt-site2
wait_eq 30 "site2's tunnel to site1 too" "10-19,21-50" vlan_summary site2 gt-site1
check "the tunnel site1-site3 is unchanged" test "$(vlan_summary site1 gt-site3)" = "$before"
wait_for 30 "network.toml on site2 has the limit" \
  x site2 "grep -A2 '^\[\[tunnel\]\]' /etc/wanderlan/network.toml | grep -q 'between = \[\"site1\", \"site2\"\]'"
check "the status shows what the tunnel carries" \
  test "$(jqs site1 '.peers[] | select(.name == "site2") | .vlans')" = "10-19,21-50"
expect_no_ping c-site1 10.20.0.2 "VLAN 20: site1 no longer reaches site2"
expect_ping c-site1 10.20.0.11 "VLAN 20: site1 still reaches site3"
expect_ping c-site2 10.20.0.11 "VLAN 20: site2 still reaches site3"
out=$(x site5 "wanderlan tunnel-vlans site2 site3 20" 2>&1)
if grep -q 'there is no site "site2" in the network of this site' <<<"$out"; then
  pass "site5 (a client) does not even see the tunnels of other sites"
else
  fail "unexpected output when site5 limits a foreign tunnel: $out"
fi
out=$(x site1 "wanderlan tunnel-vlans site1 site2 10-19,21-50" 2>&1)
if grep -q 'already has this limit' <<<"$out"; then
  pass "the same limit again changes nothing"
else
  fail "unexpected output for the same limit: $out"
fi
check "site1 lifts the limit" x site1 "wanderlan tunnel-vlans site1 site2 all --confirm"
wait_eq 30 "the tunnel carries everything both have again" "$before" vlan_summary site1 gt-site2
wait_for 30 "network.toml on site2 has no limit any more" \
  x site2 "! grep -q '^\[\[tunnel\]\]' /etc/wanderlan/network.toml"
expect_ping c-site1 10.20.0.2 "VLAN 20: site1 reaches site2 again"
