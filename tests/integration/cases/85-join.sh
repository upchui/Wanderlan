# shellcheck shell=bash
# Reset, invitations and joining: site5 leaves (reset), joins again with an
# invitation that carries its rules, starts offline and is taken online by
# the inviting site; its server refuses a change it may not make; a reset
# site joins via another site; a copy of a running machine is refused.

code_of() { awk '$1 == "wanderlan" && $2 == "join" {print $4; exit}'; }

begin "Reset: everything wanderlan set up is removed"

code1=$(x site2 "wanderlan invite --native 20 --map 130:30" | code_of)
check "site2 created an invitation with rules (code $code1)" grep -qE '^[0-9A-Z]{5}-[0-9A-Z]{5}$' <<<"$code1"
client_native_del c-site5
old_id=$(jqs site5 .site_id)
check "site5 has an identity" grep -qE '^[A-Za-z0-9+/]{43}=$' <<<"$old_id"
check "wanderlan reset --yes" x site5 "wanderlan reset --yes"
check "site5 is unconfigured" x site5 "wanderlan status --json | jq -e '.state == \"unconfigured\"'"
check "no bridge, no tunnels and no nftables table are left" \
  x site5 "! ip link show br0 2>/dev/null && ! ip -o link show | grep -q ' vx' && ! nft list table bridge wanderlan 2>/dev/null"
check "no tc rules are left on the trunk" x site5 "[ -z \"\$(tc filter show dev trunk0 ingress 2>/dev/null)\" ]"
check "the identity is gone (the box is a new box now)" x site5 "[ ! -e /var/lib/wanderlan/identity ]"

# The other sites remove site5 (and its VLAN mappings).
network_toml "10-50" site1 site2 site3 site4 >"$RUN_DIR/network-4.toml"
put site1 /tmp/network-4.toml <"$RUN_DIR/network-4.toml"
x site1 "wanderlan apply --network /tmp/network-4.toml >/dev/null && wanderlan confirm >/dev/null"
rev=$(jqs site1 .revision)
for s in site2 site3 site4; do
  wait_eq 60 "$s has 4 sites (revision $rev)" "$rev 4" jqs "$s" '"\(.revision) \(.sites | length)"'
done

begin "Join: a new site joins with nothing but the address and the code"

out=$(x site5 "wanderlan join 10.100.2.10 AAAAA-AAAAA" 2>&1)
if grep -q 'unknown, expired or already used invitation code' <<<"$out"; then
  pass "a wrong code is refused"
else
  fail "unexpected output for a wrong code: $out"
fi
out=$(x site5 "wanderlan join 10.100.2.10 $code1" 2>&1)
if grep -q 'Done: "site5" is part of the network' <<<"$out" && grep -q 'trunk trunk0' <<<"$out"; then
  pass "site5 joined (name from the host name, trunk chosen automatically)"
else
  fail "unexpected join output: $out"
fi
rev=$(jqs site2 .revision)
for s in site1 site3 site4; do
  wait_eq 60 "$s has 5 sites (revision $rev)" "$rev 5" jqs "$s" '"\(.revision) \(.sites | length)"'
done
wait_eq 60 "site5 knows only site2 and itself (revision $rev)" "$rev site2 site5" \
  jqs site5 '"\(.revision) \([.sites[].name] | sort | join(" "))"'
new_id=$(jqs site5 .site_id)
check "site5 has a new identity" test -n "$new_id" -a "$new_id" != "$old_id"
check "site2 knows site5 by its new identity" \
  test "$(jqs site2 '.sites[] | select(.name == "site5") | .id')" = "${new_id:0:8}"
wait_for 30 "site5 is a client of site2: its only tunnel leads there" \
  x site5 "[ \"\$(br-ports br0 tunnels)\" = site2 ]"
check "site1 and site3 have no tunnel to site5" \
  x site1 "[ -z \"\$(tun site5)\" ]" && x site3 "[ -z \"\$(tun site5)\" ]"
check "the client port at site2 is not isolated, the server ports are" \
  x site2 "bridge -d link show dev \$(tun site5) | grep -q 'isolated off' && bridge -d link show dev \$(tun site1) | grep -q 'isolated on'"
check "site5 got the rules of the invitation (untagged is VLAN 20)" \
  test "$(vlan_summary site5 trunk0)" = "10-19,20:pu,21-50"

begin "Online/offline: a new site starts offline until it is taken online"

check "site5 is offline, its tunnel at site2 is down" \
  x site2 "wanderlan status --json | jq -e '.peers[] | select(.name == \"site5\") | .port_state == \"offline\"' && ip -j link show \$(tun site5) | jq -e '.[0].flags | index(\"UP\") | not'"
client_native c-site5 10.20.0.13/24
expect_no_ping c-site5 10.20.0.1 "no traffic while site5 is offline"
check "site2 takes site5 online" x site2 "wanderlan online site5 --confirm"
wait_for 30 "the tunnel to site5 is up at site2" \
  x site2 "ip -j link show \$(tun site5) | jq -e '.[0].flags | index(\"UP\")'"
expect_ping c-site5 10.20.0.1 "VLAN 20: the new site reaches site1's client (through site2)"
expect_ping c-site3 10.20.0.13 "VLAN 20: site3's client reaches the new site (through site2)"

begin "Tunnel mode: a client its server reaches directly runs direct by itself"

# site2 reaches site5 at its tunnel port (no NAT in between): the test
# finds that out, both ends run the tunnel direct.
wait_eq 60 "site2 runs the tunnel to site5 direct, reached at its address" \
  "direct: reached at 10.100.13.10:51822" \
  jqs site2 '.peers[] | select(.name == "site5") | .tunnel_reason'
wait_eq 30 "site5 runs it direct too" "direct" \
  jqs site5 '.peers[] | select(.name == "site2") | .tunnel'
wait_for 30 "site5 sends from a port per flow" \
  x site5 "ip -d link show \$(tun site2) | grep -q 'vxlan id' && ! ip -d link show \$(tun site2) | grep -q 'srcport 51822 51823'"
check "site2 sends to site5's address" \
  x site2 "bridge fdb show dev \$(tun site5) | grep -q '^00:00:00:00:00:00 dst 10.100.13.10 '"
check "the status tells how the tunnel runs" \
  x site5 "wanderlan status | grep -q 'Tunnel to site2: VXLAN direct: \"site2\" reaches this site at UDP 51822'"
expect_ping c-site5 10.20.0.1 "VLAN 20: site5 reaches site1's client, direct"

begin "Sync: a server refuses a change of its client"

rev=$(jqs site2 .revision)
wait_eq 60 "site5 has revision $rev, nothing pending" "$rev null" \
  jqs site5 '"\(.revision) \(.pending)"'
# site5 (a client of site2) adds a client of its own named like a site it
# does not see: valid on site5, but in the network a site without an
# identity of its own needs a name of its own.
x site5 "printf '\n[site.site1]\naddress = \"10.100.99.10\"\nserver = \"site5\"\n' | cat /etc/wanderlan/network.toml - > /tmp/taken-name.toml"
if out=$(x site5 "wanderlan apply --network /tmp/taken-name.toml && wanderlan confirm" 2>&1); then
  pass "site5 adds a client named \"site1\""
else
  fail "site5 cannot add a client named \"site1\": $out"
fi
wait_for 15 "site2 marks the change of site5 as refused" \
  x site2 "wanderlan status --json | jq -e '.peers[] | select(.name == \"site5\") | .refused | test(\"defined twice\")'"
wait_eq 45 "site5 takes site2's configuration back" "site2 site5" \
  jqs site5 '[.sites[].name] | sort | join(" ")'
check "site5's status tells why" \
  x site5 "wanderlan status | grep -q 'WARNING: Your change was not taken over by \"site2\": .*defined twice'"
wait_eq 30 "site2 no longer marks site5" null \
  jqs site2 '.peers[] | select(.name == "site5") | .refused'
wait_for 30 "site5 has no tunnel to the made-up site" x site5 "[ -z \"\$(tun site1)\" ]"

begin "Join: a reset site joins again via another site"

rev=$(jqs site1 .revision)
check "reset site5" x site5 "wanderlan reset --yes"
code3=$(x site1 "wanderlan invite" | code_of)
out=$(x site5 "wanderlan join 10.100.1.10 $code3" 2>&1)
if grep -q 'Done: "site5" is part of the network' <<<"$out"; then
  pass "site5 joined again via site1"
else
  fail "unexpected join output: $out"
fi
check "site5 is a client of site1 now (it joined through site1)" \
  test "$(jqs site1 '"\(.revision) \(.sites[] | select(.name == "site5") | .uplink)"')" = "$((rev + 1)) site1"
wait_for 30 "site5's only tunnel leads to site1" \
  x site5 "[ \"\$(br-ports br0 tunnels)\" = site1 ]"
wait_for 30 "site2 has no tunnel to site5 any more" x site2 "[ -z \"\$(tun site5)\" ]"
check "site5 is still online (the state is kept)" \
  test "$(jqs site1 '.sites[] | select(.name == "site5") | .online')" = true
expect_ping c-site5 10.20.0.1 "VLAN 20: site5 reaches site1's client again"
wait_eq 30 "site5 has its native mapping again" "10-19,20:pu,21-50" vlan_summary site5 trunk0

begin "Join: a copy of a running machine is refused"

# site5 gets the key of site4, which runs (a cloned VM).
check "reset site5" x site5 "wanderlan reset --yes"
x site4 "cat /var/lib/wanderlan/identity" | put site5 /var/lib/wanderlan/identity
x site5 "chmod 600 /var/lib/wanderlan/identity"
code4=$(x site1 "wanderlan invite" | code_of)
out=$(x site5 "wanderlan join 10.100.1.10 $code4" 2>&1)
if grep -q 'has the identity of site "site4", which is still running' <<<"$out"; then
  pass "a box with the identity of the running site4 is refused"
else
  fail "unexpected join output: $out"
fi
check "site5 is still unconfigured" x site5 "wanderlan status --json | jq -e '.state == \"unconfigured\"'"
check "a reset gives site5 its own identity" x site5 "wanderlan reset --yes"
out=$(x site5 "wanderlan join 10.100.1.10 $code4" 2>&1)
if grep -q 'Done: "site5" is part of the network' <<<"$out"; then
  pass "with its own identity site5 joins again (the code still works)"
else
  fail "unexpected join output: $out"
fi
check "site1 says that a new box took over site5" \
  x site1 "wanderlan log | grep -q 'takes over site \"site5\"'"
wait_for 30 "site5's tunnel to site1 is up again" \
  x site5 "wanderlan status --json | jq -e '.peers[] | select(.name == \"site1\") | .identity_state == \"confirmed\" and .port_state == \"up\"'"
expect_ping c-site5 10.20.0.1 "VLAN 20: site5 reaches site1's client again"

begin "Client + server: site5 (a client of site1) has a client of its own"

only_tunnel() { x "$1" "[ \"\$(br-ports br0 tunnels)\" = $2 ]"; }
check "site1 makes site4 a client of site5" x site1 "wanderlan role site4 client site5 --confirm"
wait_for 60 "site4's only tunnel leads to site5" only_tunnel site4 site5
wait_for 30 "site5 has a tunnel to its client site4" x site5 "[ -n \"\$(tun site4)\" ]"
keyed site4 site5
check "site1 shows site5 as client + server" \
  x site1 "wanderlan status | grep -q 'site5 .*client of site1 + server'"
wait_eq 30 "site4 knows only site5 (its server) and itself" "site4 site5" \
  jqs site4 '[.sites[].name] | sort | join(" ")'
expect_ping c-site4 10.20.0.1 "VLAN 20: site4's client reaches site1's client (through site5)"
# A change only site4 itself may make reaches site1 through site5.
check "site4 moves its control port" x site4 "wanderlan settings control-port 52841 --confirm"
wait_for 90 "site1 learned it through site5" eval 'entry_of site1 site4 | grep -q "^control_port = 52841"'
expect_ping c-site4 10.20.0.1 "VLAN 20: site4's client still reaches site1's client"
check "site4 takes the default port again" x site4 "wanderlan settings control-port default --confirm"
wait_for 90 "site1 has the default port of site4 again" eval '! entry_of site1 site4 | grep -q "_port ="'
# Where a client is plugged in only its own server decides (site5 for
# site4), and away from a client + server only when that one knows where it
# went.
out=$(x site1 "wanderlan role site4 server --confirm" 2>&1)
if grep -q 'that is for its server "site5"' <<<"$out"; then
  pass "site1 does not move site4: that is for its server site5"
else
  fail "unexpected role output: $out"
fi
out=$(x site5 "wanderlan role site4 server --confirm" 2>&1)
if grep -q 'hangs off "site5", a client' <<<"$out"; then
  pass "site4 is not moved away from under site5 (site5 would lose it)"
else
  fail "unexpected role output: $out"
fi
# site5 is a client of site1: site1 makes it a server, then site5 (a server
# now) site4.
uplink_at() { jqs "$1" ".sites[] | select(.name == \"$2\") | .uplink"; }
check "site1 makes site5 a server" x site1 "wanderlan role site5 server --confirm"
wait_for 60 "site5 has its tunnels to the servers" x site5 "[ -n \"\$(tun site2)\" ] && [ -n \"\$(tun site3)\" ]"
wait_eq 30 "site1 knows site5 as a server" null uplink_at site1 site5
wait_eq 30 "site5 has the network of site1" "$(jqs site1 .revision)" jqs site5 .revision
check "then site5 makes site4 a server" x site5 "wanderlan role site4 server --confirm"
wait_for 60 "site4 has its tunnels to the servers again" \
  x site4 "[ -n \"\$(tun site1)\" ] && [ -n \"\$(tun site2)\" ] && [ -n \"\$(tun site3)\" ]"
wait_eq 30 "site2 knows site4 as a server" null uplink_at site2 site4
# A server: every server may change it. (With the change of site5 first:
# site1 may get it after site2.)
fp=$(jqs site5 .fingerprint)
for s in site1 site2; do
  wait_eq 30 "$s has the network of site5" "$fp" jqs "$s" .fingerprint
done
check "and site2 makes site5 a client of site1 again" x site2 "wanderlan role site5 client site1 --confirm"
wait_for 60 "site5's only tunnel leads to site1 again" only_tunnel site5 site1
keyed site4 site5
expect_ping c-site4 10.20.0.1 "VLAN 20: site4's client reaches site1's client directly again"
expect_ping c-site5 10.20.0.1 "VLAN 20: site5 reaches site1's client again"
