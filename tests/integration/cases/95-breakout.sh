# shellcheck shell=bash
# Local breakout: VLAN 44 of site3 reaches "the internet" (203.0.113.1 and
# 2001:db8:ffff::1 on the router) through site3's own uplink, masqueraded;
# its gateway (c-site2, at site2) does not route at all. Exceptions go
# through the tunnel, a second uplink is used when chosen, and nothing is
# left once the breakout is off.

begin "Local breakout: VLAN 44 of site3 leaves through site3's uplink"

# The gateway of VLAN 44 is at site2 and routes nowhere.
client_vlan c-site2 44 10.44.0.1/24
x c-site2 "ip -6 addr add 2001:db8:44::1/64 dev trunk0.44 nodad 2>/dev/null || true"
client_vlan c-site3 44 10.44.0.3/24
x c-site3 "ip -6 addr add 2001:db8:44::3/64 dev trunk0.44 nodad 2>/dev/null || true
           ip route replace default via 10.44.0.1 dev trunk0.44
           ip -6 route replace default via 2001:db8:44::1 dev trunk0.44"
expect_ping c-site3 10.44.0.1 "VLAN 44: site3's client reaches its gateway at site2"
expect_no_ping c-site3 203.0.113.1 "without a breakout the internet is not reached (the gateway does not route)"

# What the internet sees: a counter per source address.
x router "nft -f - <<'NFT'
table inet bo {
  chain in {
    type filter hook input priority 0; policy accept;
    ip daddr 203.0.113.1 ip saddr 10.100.11.10 counter comment \"uplink\"
    ip daddr 203.0.113.1 ip saddr 10.100.111.10 counter comment \"second\"
    ip daddr 203.0.113.1 ip saddr 10.44.0.0/24 counter comment \"unmasked\"
  }
}
NFT"
seen() { # seen <comment>: packets the router counted for it
  x router "nft -j list chain inet bo in" |
    jq "[.nftables[].rule? | select(.comment == \"$1\") | .expr[] | .counter? | select(.) | .packets] | add // 0"
}

check "wanderlan breakout site3 44 on" x site3 "wanderlan breakout site3 44 on --confirm"
wait_eq 30 "site3: the breakout of VLAN 44 is in place" "true wlbo44" \
  jqs site3 '.breakout[0] | "\(.ready) \(.router)"'
wait_for 30 "site2 took the breakout of site3 over by sync" \
  x site2 "grep -q '^breakout = { 44 = {} }' /etc/wanderlan/network.toml"
expect_ping c-site3 203.0.113.1 "VLAN 44: IPv4 to the internet through site3's uplink"
check "the internet sees site3's uplink address, nothing unmasked" \
  eval '[ "$(seen uplink)" -gt 0 ] && [ "$(seen unmasked)" = 0 ]'
expect_ping c-site3 2001:db8:ffff::1 "VLAN 44: IPv6 to the internet through site3's uplink too"
expect_ping c-site3 10.44.0.1 "private destinations still go through the tunnel"

begin "Local breakout: exceptions and a second uplink"

check "an exception goes through the tunnel" \
  x site3 "wanderlan breakout site3 44 on --except 203.0.113.0/24 --confirm"
sleep 2
expect_no_ping c-site3 203.0.113.1 "203.0.113.1 goes to the gateway again (which does not route)"
wan2=$(x site3 "ip -o -4 addr show | awk '/ 10\\.100\\.111\\./ {print \$2}'")
check "VLAN 44 leaves through the second uplink $wan2" \
  x site3 "wanderlan breakout site3 44 on --except none --uplink $wan2 --confirm"
wait_eq 30 "site3: IPv4 of VLAN 44 leaves out of $wan2" "$wan2" jqs site3 '.breakout[0].ipv4.dev'
expect_ping c-site3 203.0.113.1 "VLAN 44: IPv4 to the internet through the second uplink"
check "the internet sees the address of the second uplink" eval '[ "$(seen second)" -gt 0 ]'

begin "Local breakout: off again, nothing left"

check "wanderlan breakout site3 44 off" x site3 "wanderlan breakout site3 44 off --confirm"
wait_for 30 "site3: no breakout interfaces" \
  x site3 "[ -z \"\$(ip -o link show | grep -E ' wlb[op][0-9]')\" ]"
check "site3: no breakout tables, rules or routes" \
  x site3 "! nft list table inet wanderlan-breakout 2>/dev/null && ! nft list table bridge wanderlan-breakout 2>/dev/null && ! ip rule show | grep -q 'proto 87' && ! ip -6 rule show | grep -q 'proto 87'"
expect_no_ping c-site3 203.0.113.1 "VLAN 44 goes through the tunnel again"
x router "nft delete table inet bo" || true
client_vlan_del c-site2 44
client_vlan_del c-site3 44
