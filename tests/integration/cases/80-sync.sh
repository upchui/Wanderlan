# shellcheck shell=bash
# Automatic configuration sync between the sites.

begin "Sync: a confirmed change at one site reaches all sites"

base=$(jqs site2 .revision)
sed 's/^vlans = .*/vlans = "10-60"/' "$RUN_DIR/network-map.toml" | put site2 /tmp/network-60.toml
x site2 "wanderlan apply --network /tmp/network-60.toml" >/dev/null
out=$(x site2 "wanderlan confirm")
want=$((base + 1))
if grep -q "network revision $want is in effect" <<<"$out"; then
  pass "confirming bumps the revision to $want"
else
  fail "unexpected confirm output: $out"
fi
fp=$(jqs site2 .fingerprint)
for s in site1 site3 site4 site5; do
  wait_eq 90 "$s has network revision $want" "$want" jqs "$s" .revision
  check "$s has the same configuration (fingerprint)" test "$(jqs "$s" .fingerprint)" = "$fp"
  wait_eq 30 "$s confirmed the synced change automatically" "null" jqs "$s" .pending
done
check "site3 applied the synced VLAN range (10-60 on trunk0)" test "$(vlan_summary site3 trunk0)" = "10-60"
check "site3 wrote the synced configuration to its network.toml" \
  x site3 "grep -q '^revision = $want' /etc/wanderlan/network.toml && grep -q '^vlans = \"10-60\"' /etc/wanderlan/network.toml"
check "site5 kept its own local.toml" x site5 "grep -q '^self = \".*\"  # site5\$' /etc/wanderlan/local.toml"
wait_for 30 "site1 reports all peers in sync" \
  x site1 "wanderlan status --json | jq -e '[.peers[].in_sync] | all'"

begin "Sync: changes based on an outdated revision are refused"

network_old=$(x site3 "wanderlan export" | sed 's/^vlans = .*/vlans = "10-55"/')
printf '%s\n' "$network_old" | put site3 /tmp/network-old.toml
api_reply=$(x site3 "jq -Rsc '{id: 1, method: \"apply\", params: {network: ., local: \"self = \\\"site3\\\"\\ntrunk = \\\"trunk0\\\"\\n\", base_revision: 1}}' /tmp/network-old.toml | timeout 3 nc -U -q 1 /run/wanderlan/wanderlan.sock")
if grep -q 'the configuration changed meanwhile' <<<"$api_reply"; then
  pass "an apply based on revision 1 is refused"
else
  fail "unexpected API reply: $api_reply"
fi

begin "Sync: a site with an outdated network fetches the whole network from a peer"

x site5 "wl-daemon stop"
printf '[network]\nrevision = 0\nvlans = "10-50"\n\n[[site]]\nname = "site5"\naddress = "10.100.13.10"\n\n[[site]]\nname = "site2"\naddress = "10.100.2.10"\n' \
  | put site5 /etc/wanderlan/network.toml
x site5 "wl-daemon start"
wait_eq 90 "site5 fetched the network from site2 (revision $want, 5 sites)" "$want 5" \
  jqs site5 '"\(.revision) \(.sites | length)"'
check "site5 has its mappings again (tc rules from the synced network)" \
  x site5 "tc filter show dev trunk0 ingress | grep -q 'vlan_id 130'"
expect_ping c-site5 10.30.0.1 "mapped VLAN 130 at site5 reaches VLAN 30 at site1 again"

# Back to VLANs 10-50 for the remaining cases.
sed 's/^vlans = .*/vlans = "10-50"/' "$RUN_DIR/network-map.toml" | put site1 /tmp/network-50.toml
x site1 "wanderlan apply --network /tmp/network-50.toml >/dev/null && wanderlan confirm >/dev/null"
wait_eq 90 "site5 is back at VLANs 10-50" "10-19,20:pu,21-50" vlan_summary site5 trunk0

begin "Settings: every site sets its own, nobody those of another"

client_vlan c-site3 20 10.20.0.11/24
client_vlan c-site4 20 10.20.0.12/24
site4_entry() { entry_of site1 site4; }
check "site4 moves its control port" x site4 "wanderlan settings control-port 52821 --confirm"
check "and its tunnel port" x site4 "wanderlan settings tunnel-port 52833 --confirm"
wait_for 30 "site1 learned the ports of site4 (from site4, at its new port)" \
  eval 'site4_entry | grep -q "^control_port = 52821" && site4_entry | grep -q "^tunnel_port = 52833"'
wait_for 30 "site1 sends to the new tunnel port of site4" \
  x site1 "bridge fdb show dev \$(tun site4) | grep -q 'port 52833'"
expect_ping c-site3 10.20.0.12 "VLAN 20: site3 reaches site4 at its new ports"
x site1 "sed 's/^control_port = 52821/control_port = 52999/' /etc/wanderlan/network.toml > /tmp/ports.toml"
check "site1 cannot change the ports of site4" \
  x site1 "! wanderlan apply --network /tmp/ports.toml 2>/tmp/ports.err && grep -q 'only \"site4\" itself' /tmp/ports.err"
check "site4 sets its own commit timeout" x site4 "wanderlan settings commit-timeout 2m --confirm"
check "site4 keeps it in its local.toml" x site4 "grep -q '^commit_timeout = \"2m\"' /etc/wanderlan/local.toml"
check "site1 keeps its own" x site1 "wanderlan settings | grep -q '^commit-timeout  1m  (network default)'"
# Its ports are its own, its VLANs not: they change elsewhere, the ports stay.
check "site1 changes the VLANs of site4, which has ports of its own" \
  x site1 "wanderlan site vlans site4 10-40 --confirm"
wait_for 90 "site4 has the VLANs 10-40 and kept its ports" \
  eval 'entry_of site4 site4 | grep -q "^vlans = \"10-40\"" && entry_of site4 site4 | grep -q "^control_port = 52821"'
check "and gives it those of the network again" x site1 "wanderlan site vlans site4 default --confirm"
wait_for 90 "site4 has the VLANs of the network again" eval '! entry_of site4 site4 | grep -q "^vlans ="'
for s in commit-timeout tunnel-port control-port; do
  x site4 "wanderlan settings $s default --confirm" >/dev/null
done
wait_for 90 "site1 has the default ports of site4 again" eval '! site4_entry | grep -q "_port ="'
expect_ping c-site3 10.20.0.12 "VLAN 20: site3 reaches site4 at the default ports again"

begin "CLI: sites, VLANs and mappings by hand"

check "site1 adds the site lab (a client of site1)" \
  x site1 "wanderlan site add lab 10.100.1.99 --vlans 10,20 --confirm"
# lab has no identity of its own (no box joined as it): a placeholder that
# follows its name, the file names every site by its identity all the same.
wait_for 90 "site3 knows lab (synced)" eval 'entry_of site3 lab | grep -Eq "^server = \".*\"  # site1\$"'
check "lab has a placeholder, not its name, as its key" \
  eval 'entry_of site3 lab | head -n 1 | grep -q "^\[site\.\".*\"\]  # no box has joined as this site yet\$"'
check "lab cannot be added twice" \
  x site1 "! wanderlan site add lab 10.100.1.98 2>/tmp/add.err && grep -q 'already a site' /tmp/add.err"
check "site1 maps source VLAN 20 to VLAN 120 at lab" x site1 "wanderlan map add lab 20 120 --confirm"
wait_for 90 "site3 has the mapping" eval 'entry_of site3 lab | grep -q "^map = { 20 = 120 }"'
check "site1 deletes it" x site1 "wanderlan map del lab 20 --confirm"
check "the mapping is gone" eval '! entry_of site1 lab | grep -q "^map"'
check "site1 removes lab" x site1 "wanderlan site remove lab --confirm"
wait_for 90 "site3 no longer knows lab" eval '! entry_of site3 lab | grep -q lab'
check "the network VLANs: nothing to change" x site1 "wanderlan vlans 10-50 | grep -q 'already'"
check "site1 sets the VLANs of the network" x site1 "wanderlan vlans 10-51 --confirm"
wait_for 90 "site3 has the VLANs 10-51" x site3 "grep -q '^vlans = \"10-51\"' /etc/wanderlan/network.toml"
x site1 "wanderlan vlans 10-50 --confirm" >/dev/null
wait_eq 90 "site5 is back at VLANs 10-50" "10-19,20:pu,21-50" vlan_summary site5 trunk0

begin "Sync: two servers change the network at the same time, both changes are kept"

want=$(jqs site1 .revision)
for s in site2 site3 site4 site5; do
  wait_eq 60 "$s has network revision $want" "$want" jqs "$s" .revision
done
# site1 hears nobody for a moment: site1 and site3 change the network from
# the same revision, neither knowing the other's change.
check "site1 is cut off from the control traffic" \
  x site1 "nft add table inet isolate && nft 'add chain inet isolate in { type filter hook input priority -10 ; }' && nft 'add chain inet isolate out { type filter hook output priority -10 ; }' && for c in in out; do for p in udp tcp; do nft add rule inet isolate \$c \$p dport 51821 drop || exit 1; done; done"
check "site1 sets the VLANs of the network" x site1 "wanderlan vlans 10-52 --confirm"
check "site3 sets the VLANs of site4 meanwhile" x site3 "wanderlan site vlans site4 10-40 --confirm"
check "site1 hears the others again" x site1 "nft delete table inet isolate"
both_changes() {
  x "$1" "grep -q '^vlans = \"10-52\"' /etc/wanderlan/network.toml" &&
    entry_of "$1" site4 | grep -q '^vlans = "10-40"'
}
for s in site1 site2 site3 site4 site5; do
  wait_for 90 "$s has both changes" both_changes "$s"
done
fp=$(jqs site1 .fingerprint)
for s in site2 site3 site4 site5; do
  wait_eq 60 "$s has the same network as site1" "$fp" jqs "$s" .fingerprint
done
merged_said() {
  x site1 "wanderlan log | grep -q 'at the same time; both changes are kept'" ||
    x site3 "wanderlan log | grep -q 'at the same time; both changes are kept'"
}
check "site1 or site3 tells that it put both together" merged_said
check "site1 takes the VLANs of the network back" x site1 "wanderlan vlans 10-50 --confirm"
check "and those of site4" x site1 "wanderlan site vlans site4 default --confirm"
wait_for 90 "site3 has the VLANs of the network for site4 again" eval '! entry_of site3 site4 | grep -q "^vlans ="'
wait_eq 90 "site5 is back at VLANs 10-50" "10-19,20:pu,21-50" vlan_summary site5 trunk0

begin "Sync: a server made a client while it was away fetches its view"

x site5 "wl-daemon stop"
check "site2 makes site5 a client of its own" x site2 "wanderlan role site5 client site2 --confirm"
rev=$(jqs site2 .revision)
for s in site1 site3 site4; do
  wait_eq 60 "$s has network revision $rev" "$rev" jqs "$s" .revision
done
# With a higher revision (written by hand meanwhile) site5 does not fetch the
# view of site2: it takes site2 for a server and sends it nothing.
x site5 "sed -i 's/^revision = .*/revision = $((rev + 5))/' /etc/wanderlan/network.toml"
x site5 "wl-daemon start"
wait_for 120 "site5 is a client of site2 now: its only tunnel leads there" \
  x site5 "[ \"\$(br-ports br0 tunnels)\" = site2 ]"
check "site2 counted its revision on, so that site5 fetched its view" \
  x site2 "wanderlan log | grep -q 'does not send it; this site counts its revision on'"
wait_eq 60 "site5 knows only site2 and itself" "site2 site5" jqs site5 '[.sites[].name] | sort | join(" ")'
# Nothing of its own was lost: a copy kept aside is only reported.
if x site5 "ls /etc/wanderlan/network.toml.lost-*" >/dev/null 2>&1; then
  info "site5 kept a copy aside: $(x site5 "wanderlan log | grep 'replaces a change made here' | tail -n 1")"
  x site5 "rm -f /etc/wanderlan/network.toml.lost-*"
fi
check "site2 makes site5 a server again" x site2 "wanderlan role site5 server --confirm"
wait_for 90 "site5 has its tunnels to the other servers again" \
  x site5 "for s in site1 site2 site3 site4; do [ -n \"\$(tun \$s)\" ] || exit 1; done"
wait_eq 90 "site5 is at VLANs 10-50" "10-19,20:pu,21-50" vlan_summary site5 trunk0
