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

begin "Passing on over a server: a VLAN that reaches a site only through another"

# VLAN 20 only on site1 - site3, site2 - site3 and site3 - site4: a star
# around site3. site1 and site2 reach each other only through site3.
all_but_20="10-19,21-50"
for pair in "site1 site2" "site1 site4" "site2 site4"; do
  x site1 "wanderlan tunnel-vlans $pair $all_but_20 --confirm" >/dev/null 2>&1
done
wait_eq 30 "site2's tunnel to site1 has no VLAN 20" "$all_but_20" vlan_summary site2 gt-site1
wait_eq 30 "nor its tunnel to site4" "$all_but_20" vlan_summary site2 gt-site4
wait_for 30 "site3 passes VLAN 20 on with nftables rules" \
  x site3 "nft list table bridge wanderlan | grep -q 'wl split'"
check "site3's server tunnels are no longer isolated in the kernel" \
  x site3 "! bridge -d link show dev gt-site1 | grep -q 'isolated on'"
check "site1 needs no rules (one server tunnel with VLAN 20)" \
  x site1 "! nft list table bridge wanderlan 2>/dev/null | grep -q 'wl split'"
check "site1's server tunnels stay isolated" x site1 "bridge -d link show dev gt-site2 | grep -q 'isolated on'"
expect_ping c-site1 10.20.0.2 "VLAN 20: site1 reaches site2 through site3"
expect_ping c-site2 10.20.0.12 "VLAN 20: site2 reaches site4 through site3"
expect_ping c-site5 10.20.0.2 "VLAN 20: site5 (client of site1) reaches site2 through site1 and site3"
check "no port is blocked by the loop guard" \
  test "$(for s in site1 site2 site3 site4; do jqs $s '.blocked | length'; done | sort -u)" = 0

x site3 "nft flush table bridge wanderlan"
wait_for 30 "a firewall reload on site3 is repaired" \
  x site3 "nft list table bridge wanderlan | grep -q 'wl split'"
expect_ping c-site1 10.20.0.2 "VLAN 20: site1 reaches site2 again after the reload"

begin "Passing on: a ring is not passed on"

# VLAN 20 on site1 - site2 - site3 - site4 - site1 only.
x site1 "wanderlan tunnel-vlans site1 site2 all --confirm" >/dev/null 2>&1
x site1 "wanderlan tunnel-vlans site1 site4 all --confirm" >/dev/null 2>&1
x site1 "wanderlan tunnel-vlans site1 site3 $all_but_20 --confirm" >/dev/null 2>&1
wait_eq 30 "site3's tunnel to site1 has no VLAN 20" "$all_but_20" vlan_summary site3 gt-site1
wait_for 30 "nobody passes VLAN 20 on in a ring" \
  x site3 "! nft list table bridge wanderlan 2>/dev/null | grep -q 'wl split'"
check "site3's server tunnels are isolated again" x site3 "bridge -d link show dev gt-site1 | grep -q 'isolated on'"
check "wanderlan validate warns about the ring" \
  x site1 "wanderlan validate /etc/wanderlan/network.toml --local /etc/wanderlan/local.toml 2>&1 | grep -q 'do not reach each other'"
expect_ping c-site1 10.20.0.2 "VLAN 20: neighbours in the ring reach each other"
expect_no_ping c-site1 10.20.0.11 "VLAN 20: site1 and site3 do not (no direct tunnel, ring)"
check "no port is blocked by the loop guard" \
  test "$(for s in site1 site2 site3 site4; do jqs $s '.blocked | length'; done | sort -u)" = 0

for pair in "site1 site3" "site2 site4"; do
  x site1 "wanderlan tunnel-vlans $pair all --confirm" >/dev/null 2>&1
done
wait_for 30 "all limits are gone" x site2 "! grep -q '^\[\[tunnel\]\]' /etc/wanderlan/network.toml"
expect_ping c-site1 10.20.0.11 "VLAN 20: site1 reaches site3 directly again"
