# shellcheck shell=bash
# Invitations, joining, online/offline, who may change a site, reset: a
# site leaves (reset), joins again with an invitation that carries its rules,
# starts offline, is taken online by the inviting site; codes work once.

code_of() { awk '$1 == "wanderlan" && $2 == "join" {print $4; exit}'; }
raw_join() { # raw_join <from> <target> <code> <name>: one JOIN request
  x "$1" "printf 'JOIN {\"protocol\":1,\"code\":\"%s\",\"name\":\"%s\"}\n' '$3' '$4' | timeout 10 nc -N $2 51821"
}

begin "Invitations: created on any site, kept across restarts"

check "an invitation with a mapping onto the native VLAN is refused" \
  x site2 "! wanderlan invite --native 1 2>/dev/null && wanderlan invite --native 1 2>&1 | grep -q 'native VLAN'"
check "an invitation for a name that is taken is refused" \
  x site2 "wanderlan invite --name site1 2>&1 | grep -q 'already used'"
code1=$(x site2 "wanderlan invite --native 20 --map 130:30" | code_of)
check "site2 created an invitation with rules (code $code1)" grep -qE '^[0-9A-Z]{5}-[0-9A-Z]{5}$' <<<"$code1"
check "the invitation shows the command with site2's address" \
  x site2 "wanderlan invite list | grep -q 'wanderlan join 10.100.2.10 $code1'"
check "the invitation shows its rules" \
  x site2 "wanderlan invite list | grep -q 'rules: native → 20, 130 → 30'"
x site2 "wl-daemon stop; wl-daemon start"
wait_for 30 "site2 is running again" x site2 "wanderlan status --json | jq -e '.state == \"running\"'"
check "the invitation survives a daemon restart" x site2 "wanderlan invite list | grep -q '$code1'"
check "the invitations are stored readable for root only" \
  x site2 "[ \"\$(stat -c %a /var/lib/wanderlan/invites.json)\" = 600 ]"

begin "Reset: everything wanderlan set up is removed"

client_native_del c-site5
check "wanderlan reset needs a confirmation" x site5 "echo no | wanderlan reset | grep -q 'Nothing was changed'"
check "wanderlan reset --yes" x site5 "wanderlan reset --yes"
check "site5 is unconfigured" x site5 "wanderlan status --json | jq -e '.state == \"unconfigured\"'"
check "no bridge and no tunnels are left" x site5 "! ip link show br0 2>/dev/null && ! ip -o link show | grep -q ' gt-'"
check "no nftables table is left" x site5 "! nft list table bridge wanderlan 2>/dev/null"
check "no tc rules are left on the trunk" x site5 "[ -z \"\$(tc filter show dev trunk0 ingress 2>/dev/null)\" ]"
check "the configuration files are the package templates again" \
  x site5 "grep -q 'This site is not configured yet' /etc/wanderlan/mesh.toml && grep -q 'not configured yet' /etc/wanderlan/local.toml && ! grep -q '^self' /etc/wanderlan/local.toml"
check "the state is gone" x site5 "[ ! -e /var/lib/wanderlan/confirmed ]"
check "the state directory itself stays (systemd's StateDirectory)" x site5 "[ -d /var/lib/wanderlan ]"
check "a second reset changes nothing and succeeds" x site5 "wanderlan reset --yes"

begin "Reset: a new network while the old one still lists this site"

# The other sites still list site5 and probe it with a newer revision; that
# must not block a new network on site5 (it did: stale announcements).
x site5 "printf '[mesh]\n\n[[site]]\nname = \"solo\"\naddress = \"10.100.13.10\"\n' > /tmp/solo-mesh.toml"
x site5 "printf 'self = \"solo\"\ntrunk = \"trunk0\"\n' > /tmp/solo-local.toml"
check "site5 starts its own network" \
  x site5 "wanderlan apply --mesh /tmp/solo-mesh.toml --local /tmp/solo-local.toml >/dev/null && wanderlan confirm >/dev/null"
check "site5 runs its own network" \
  test "$(jqs site5 '"\(.state) \(.site) \(.sites | length)"')" = "running solo 1"
check "site5 ignores the probes of its old network" \
  test "$(jqs site5 '.peers | length')" = 0
check "and resets again" x site5 "wanderlan reset --yes"

# The other sites remove site5 (and its VLAN mappings).
mesh_toml "10-50" site1 site2 site3 site4 >"$RUN_DIR/mesh-4.toml"
put site1 /tmp/mesh-4.toml <"$RUN_DIR/mesh-4.toml"
x site1 "wanderlan apply --mesh /tmp/mesh-4.toml >/dev/null && wanderlan confirm >/dev/null"
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
check "site5 is still unconfigured" x site5 "wanderlan status --json | jq -e '.state == \"unconfigured\"'"

out=$(x site5 "wanderlan join 10.100.2.10 $code1" 2>&1)
if grep -q 'Done: "site5" is part of the network' <<<"$out" && grep -q 'trunk trunk0' <<<"$out"; then
  pass "site5 joined (name from the host name, trunk chosen automatically)"
else
  fail "unexpected join output: $out"
fi
rev=$(jqs site2 .revision)
for s in site1 site2 site3 site4; do
  wait_eq 60 "$s has 5 sites (revision $rev)" "$rev 5" jqs "$s" '"\(.revision) \(.sites | length)"'
done
wait_eq 60 "site5 knows only site2 and itself (revision $rev)" "$rev site2 site5" \
  jqs site5 '"\(.revision) \([.sites[].name] | sort | join(" "))"'
check "site5's mesh.toml names no other site" \
  x site5 "! grep -qE 'name = \"site(1|3|4)\"' /etc/wanderlan/mesh.toml && grep -q 'name = \"site2\"' /etc/wanderlan/mesh.toml"
check "site5 sees site2 as its server" \
  test "$(jqs site5 '.sites[] | select(.name == "site2") | .uplink')" = null
check "site2 confirmed the new site automatically" test "$(jqs site2 .pending)" = null
check "site5 has its address from the connection" \
  test "$(jqs site2 '.sites[] | select(.name == "site5") | .address')" = 10.100.13.10
check "the invitation is used up" x site2 "! wanderlan invite list | grep -q '$code1'"
check "site2's log tells who joined" x site2 "wanderlan log | grep -q 'Site \"site5\" joined the network'"
wait_for 30 "site5 is a client of site2: its only tunnel leads there" \
  x site5 "[ \"\$(ip -j link show master br0 | jq -r '[.[].ifname | select(startswith(\"gt-\"))] | join(\" \")')\" = gt-site2 ]"
check "the mesh says so (uplink)" \
  test "$(x site2 "grep -A8 'name = \"site5\"' /etc/wanderlan/mesh.toml | grep -c 'uplink = \"site2\"'")" = 1
check "site1 shows site5 as a client of site2, reached via site2" \
  test "$(jqs site1 '.sites[] | select(.name == "site5") | "\(.uplink) \(.via) \(.direct)"')" = "site2 site2 false"
check "site1 and site3 have no tunnel to site5" \
  x site1 "! ip link show gt-site5 2>/dev/null" && x site3 "! ip link show gt-site5 2>/dev/null"
check "the client port at site2 is not isolated, the server ports are" \
  x site2 "bridge -d link show dev gt-site5 | grep -q 'isolated off' && bridge -d link show dev gt-site1 | grep -q 'isolated on'"
check "site5 got the rules of the invitation (untagged is VLAN 20)" \
  test "$(vlan_summary site5 trunk0)" = "10-19,20:pu,21-50"
check "site5 is managed by site2" \
  test "$(x site2 "grep -A6 'name = \"site5\"' /etc/wanderlan/mesh.toml | grep -c 'managed_by = \"site2\"'")" = 1

begin "Online/offline: a new site starts offline until it is taken online"

check "site5 is offline" test "$(jqs site2 '.sites[] | select(.name == "site5") | .online')" = false
check "site2 shows the tunnel to site5 as offline" \
  test "$(jqs site2 '.peers[] | select(.name == "site5") | .port_state')" = offline
check "the tunnel to site5 is down at site2" \
  x site2 "ip -j link show gt-site5 | jq -e '.[0].flags | index(\"UP\") | not'"
check "site5 is reachable meanwhile (control channel)" \
  x site2 "wanderlan status --json | jq -e '.peers[] | select(.name == \"site5\") | .reachable'"
client_native c-site5 10.20.0.13/24
expect_no_ping c-site5 10.20.0.1 "no traffic while site5 is offline"
out=$(x site1 "wanderlan online site5 --confirm" 2>&1)
if grep -q 'may not change site "site5"' <<<"$out"; then
  pass "site1 may not take site5 online (managed by site2)"
else
  fail "unexpected output when site1 changes site5: $out"
fi
check "site2 takes site5 online" x site2 "wanderlan online site5 --confirm"
wait_for 30 "the tunnel to site5 is up at site2" \
  x site2 "ip -j link show gt-site5 | jq -e '.[0].flags | index(\"UP\")'"
expect_ping c-site5 10.20.0.1 "VLAN 20: the new site reaches site1's client (through site2)"
expect_ping c-site3 10.20.0.13 "VLAN 20: site3's client reaches the new site (through site2)"

# Only VLANs: a VLAN that site2 does not take part in does not pass it,
# even if site1 and site5 have it.
client_vlan c-site1 45 10.45.0.1/24
client_vlan c-site5 45 10.45.0.13/24
expect_ping c-site5 10.45.0.1 "VLAN 45: site5 reaches site1 while site2 has VLAN 45"
x site2 "sed '/^name = \"site2\"\$/a vlans = \"10-40\"' /etc/wanderlan/mesh.toml > /tmp/site2-vlans.toml"
check "site2 limits itself to VLANs 10-40" \
  x site2 "wanderlan apply --mesh /tmp/site2-vlans.toml >/dev/null && wanderlan confirm >/dev/null"
wait_eq 30 "the tunnel site2-site5 no longer carries VLAN 45" "10-40" vlan_summary site2 gt-site5
expect_no_ping c-site5 10.45.0.1 "VLAN 45: no way from site5 to site1 without site2"
# The daemon writes the file in canonical order (name, address, vlans, ...).
x site2 "sed '/^name = \"site2\"\$/,/^\[/{/^vlans = \"10-40\"\$/d;}' /etc/wanderlan/mesh.toml > /tmp/site2-all.toml"
check "site2 takes part in all VLANs again" \
  x site2 "wanderlan apply --mesh /tmp/site2-all.toml >/dev/null && wanderlan confirm >/dev/null"
expect_ping c-site5 10.45.0.1 "VLAN 45: site5 reaches site1 again"
client_vlan_del c-site1 45
client_vlan_del c-site5 45
x site2 "wl-daemon stop; wl-daemon start"
wait_for 30 "site2 is running again" x site2 "wanderlan status --json | jq -e '.state == \"running\"'"
check "site5 stays online across a restart" \
  test "$(jqs site2 '.sites[] | select(.name == "site5") | .online')" = true
expect_ping c-site5 10.20.0.2 "VLAN 20: site5 reaches site2's client after the restart"
check "site2 takes site5 offline again" x site2 "wanderlan offline site5 --confirm"
wait_for 30 "the tunnel to site5 is down at site2" \
  x site2 "ip -j link show gt-site5 | jq -e '.[0].flags | index(\"UP\") | not'"
check "and online again" x site2 "wanderlan online site5 --confirm"
expect_ping c-site5 10.20.0.1 "VLAN 20: traffic flows again"

begin "Permissions: a joined site changes no VLANs unless it is released"

rev=$(jqs site2 .revision)
wait_eq 60 "site5 has revision $rev" "$rev" jqs site5 .revision
x site5 "sed 's/^vlans = .*/vlans = \"10-40\"/' /etc/wanderlan/mesh.toml > /tmp/net-vlans.toml"
out=$(x site5 "wanderlan apply --mesh /tmp/net-vlans.toml" 2>&1)
if grep -q 'may not change the VLANs of the network' <<<"$out" && grep -q 'managed by "site2"' <<<"$out"; then
  pass "site5 may not change the network's VLANs"
else
  fail "unexpected output when site5 changes the network's VLANs: $out"
fi
x site5 "sed '/^name = \"site2\"\$/a vlans = \"20\"' /etc/wanderlan/mesh.toml > /tmp/site2-vlans.toml"
out=$(x site5 "wanderlan apply --mesh /tmp/site2-vlans.toml" 2>&1)
if grep -q 'is a client of "site2": it may only change itself and its own clients' <<<"$out"; then
  pass "site5 may not change its server site2"
else
  fail "unexpected output when site5 changes site2: $out"
fi
out=$(x site5 "wanderlan offline site5 --confirm" 2>&1)
if grep -q 'may not change site "site5"' <<<"$out"; then
  pass "site5 may not take itself offline"
else
  fail "unexpected output when site5 changes itself: $out"
fi
out=$(x site2 "wanderlan manage site5 --self-managed yes --confirm" 2>&1)
if grep -q '"site2" and "site5" itself may change it' <<<"$out"; then
  pass "site2 lets site5 manage itself"
else
  fail "unexpected output of wanderlan manage: $out"
fi
rev=$(jqs site2 .revision)
wait_eq 60 "site5 has revision $rev" "$rev" jqs site5 .revision
check "now site5 takes itself offline" x site5 "wanderlan offline site5 --confirm"
wait_eq 30 "site2 took over the change of its client" false \
  jqs site2 '.sites[] | select(.name == "site5") | .online'
wait_eq 30 "and site1 has it too" false \
  jqs site1 '.sites[] | select(.name == "site5") | .online'
wait_for 30 "the tunnel to site5 is down at site2" \
  x site2 "ip -j link show gt-site5 | jq -e '.[0].flags | index(\"UP\") | not'"
check "and online again" x site5 "wanderlan online site5 --confirm"
expect_ping c-site5 10.20.0.1 "VLAN 20: traffic flows again"
rev=$(jqs site5 .revision)
wait_eq 60 "site2 has revision $rev" "$rev" jqs site2 .revision
check "site2 withdraws the release" x site2 "wanderlan manage site5 --self-managed no --confirm"
reply=$(raw_join router 10.100.2.10 "$code1" ghost)
if grep -q 'unknown or already used invitation code' <<<"$reply"; then
  pass "a used code is refused"
else
  fail "unexpected answer for a used code: $reply"
fi

begin "Join: a site that never comes up is removed again"

code2=$(x site2 "wanderlan invite" | code_of)
reply=$(raw_join router 10.100.2.10 "$code2" ghost)
check "site2 accepts the request (the router never runs wanderland)" grep -q '"ok"' <<<"$reply"
check "site2 waits for the new site" \
  x site2 "wanderlan status --json | jq -e '.pending.source == {kind: \"join\", site: \"ghost\"}'"
check "the invitation is reserved meanwhile" x site2 "wanderlan invite list | grep '$code2' | grep -q 'is joining'"
wait_eq 90 "site2 rolls the new site back after the commit timeout" "null 5" \
  jqs site2 '"\(.pending) \(.sites | length)"'
check "the log explains the rollback" x site2 "wanderlan log | grep -q 'did not come up in time'"
check "the invitation can be used again" x site2 "wanderlan invite list | grep '$code2' | grep -qv 'is joining'"
check "wanderlan invite delete removes it" x site2 "wanderlan invite delete $code2"
check "no open invitations are left" x site2 "wanderlan invite list | grep -q 'No open invitations'"

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
  x site5 "[ \"\$(ip -j link show master br0 | jq -r '[.[].ifname | select(startswith(\"gt-\"))] | join(\" \")')\" = gt-site1 ]"
wait_for 30 "site2 has no tunnel to site5 any more" x site2 "! ip link show gt-site5 2>/dev/null"
wait_eq 30 "site5 now knows only site1 and itself" "site1 site5" \
  jqs site5 '[.sites[].name] | sort | join(" ")'
check "site1's invitation is used up" x site1 "wanderlan invite list | grep -q 'No open invitations'"
check "site5 is still online (the state is kept)" \
  test "$(jqs site1 '.sites[] | select(.name == "site5") | .online')" = true
expect_ping c-site5 10.20.0.1 "VLAN 20: site5 reaches site1's client again"
wait_eq 30 "site5 has its native mapping again" "10-19,20:pu,21-50" vlan_summary site5 trunk0
