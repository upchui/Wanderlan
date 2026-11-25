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
  x site5 "! ip link show br0 2>/dev/null && ! ip -o link show | grep -q ' vx-' && ! nft list table bridge wanderlan 2>/dev/null"
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
if grep -q 'unknown or already used invitation code' <<<"$out"; then
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
  x site5 "[ \"\$(ip -j link show master br0 | jq -r '[.[].ifname | select(startswith(\"vx-\"))] | join(\" \")')\" = vx-site2 ]"
check "site1 and site3 have no tunnel to site5" \
  x site1 "! ip link show vx-site5 2>/dev/null" && x site3 "! ip link show vx-site5 2>/dev/null"
check "the client port at site2 is not isolated, the server ports are" \
  x site2 "bridge -d link show dev vx-site5 | grep -q 'isolated off' && bridge -d link show dev vx-site1 | grep -q 'isolated on'"
check "site5 got the rules of the invitation (untagged is VLAN 20)" \
  test "$(vlan_summary site5 trunk0)" = "10-19,20:pu,21-50"

begin "Online/offline: a new site starts offline until it is taken online"

check "site5 is offline, its tunnel at site2 is down" \
  x site2 "wanderlan status --json | jq -e '.peers[] | select(.name == \"site5\") | .port_state == \"offline\"' && ip -j link show vx-site5 | jq -e '.[0].flags | index(\"UP\") | not'"
client_native c-site5 10.20.0.13/24
expect_no_ping c-site5 10.20.0.1 "no traffic while site5 is offline"
check "site2 takes site5 online" x site2 "wanderlan online site5 --confirm"
wait_for 30 "the tunnel to site5 is up at site2" \
  x site2 "ip -j link show vx-site5 | jq -e '.[0].flags | index(\"UP\")'"
expect_ping c-site5 10.20.0.1 "VLAN 20: the new site reaches site1's client (through site2)"
expect_ping c-site3 10.20.0.13 "VLAN 20: site3's client reaches the new site (through site2)"

begin "Tunnel mode: a client runs nat (the default) or direct"

check "a new client runs nat: from the tunnel port, with keepalives" \
  x site5 "ip -d link show vx-site2 | grep -q 'srcport 51822 51823'"
check "site2 learned where site5's tunnel packets come from" \
  x site2 "bridge fdb show dev vx-site5 | grep -q '^00:00:00:00:00:00 dst 10.100.13.10 '"
check "site2 lets site5 run direct" x site2 "wanderlan tunnel-mode site5 direct --confirm"
wait_for 30 "site5 sends from a port per flow" \
  x site5 "ip -d link show vx-site2 | grep -q 'vxlan id' && ! ip -d link show vx-site2 | grep -q 'srcport 51822 51823'"
wait_eq 30 "site2 shows the tunnel as direct" "direct" \
  jqs site2 '.peers[] | select(.name == "site5") | .tunnel'
expect_ping c-site5 10.20.0.1 "VLAN 20: site5 reaches site1's client, direct"
check "and back to nat" x site2 "wanderlan tunnel-mode site5 nat --confirm"
wait_for 30 "site5 sends from the tunnel port again" \
  x site5 "ip -d link show vx-site2 | grep -q 'srcport 51822 51823'"
expect_ping c-site5 10.20.0.1 "VLAN 20: site5 reaches site1's client through nat again"

begin "Sync: a server refuses a change of its client"

rev=$(jqs site2 .revision)
wait_eq 60 "site5 has revision $rev, nothing pending" "$rev null" \
  jqs site5 '"\(.revision) \(.pending)"'
# site5 (a client of site2) adds a client of its own named like a site it
# does not see: valid on site5, but the name is taken in the network.
x site5 "printf '\n[site.site1]\naddress = \"10.100.99.10\"\nserver = \"site5\"\n' | cat /etc/wanderlan/network.toml - > /tmp/taken-name.toml"
if out=$(x site5 "wanderlan apply --network /tmp/taken-name.toml && wanderlan confirm" 2>&1); then
  pass "site5 adds a client named \"site1\""
else
  fail "site5 cannot add a client named \"site1\": $out"
fi
wait_for 15 "site2 marks the change of site5 as refused" \
  x site2 "wanderlan status --json | jq -e '.peers[] | select(.name == \"site5\") | .refused | test(\"already used\")'"
wait_eq 45 "site5 takes site2's configuration back" "site2 site5" \
  jqs site5 '[.sites[].name] | sort | join(" ")'
check "site5's status tells why" \
  x site5 "wanderlan status | grep -q 'WARNING: Your change was not taken over by \"site2\": .*already used'"
wait_eq 30 "site2 no longer marks site5" null \
  jqs site2 '.peers[] | select(.name == "site5") | .refused'
wait_for 30 "site5 has no tunnel to the made-up site" x site5 "! ip link show vx-site1 2>/dev/null"

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
  x site5 "[ \"\$(ip -j link show master br0 | jq -r '[.[].ifname | select(startswith(\"vx-\"))] | join(\" \")')\" = vx-site1 ]"
wait_for 30 "site2 has no tunnel to site5 any more" x site2 "! ip link show vx-site5 2>/dev/null"
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
