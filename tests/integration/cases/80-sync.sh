# shellcheck shell=bash
# Automatic configuration sync between the sites.

begin "Sync: a confirmed change at one site reaches all sites"

base=$(jqs hub2 .revision)
sed 's/^vlans = .*/vlans = "10-60"/' "$RUN_DIR/mesh-map.toml" | put hub2 /tmp/mesh-60.toml
x hub2 "wanderlan apply --mesh /tmp/mesh-60.toml" >/dev/null
out=$(x hub2 "wanderlan confirm")
want=$((base + 1))
if grep -q "mesh revision $want is in effect" <<<"$out"; then
  pass "confirming bumps the revision to $want"
else
  fail "unexpected confirm output: $out"
fi
fp=$(jqs hub2 .fingerprint)
for s in hub1 spoke1 spoke2 spoke3; do
  wait_eq 90 "$s has mesh revision $want" "$want" jqs "$s" .revision
  check "$s has the same configuration (fingerprint)" test "$(jqs "$s" .fingerprint)" = "$fp"
  wait_eq 30 "$s confirmed the synced change automatically" "null" jqs "$s" .pending
done
check "spoke1 applied the synced VLAN range (10-60 on trunk0)" test "$(vlan_summary spoke1 trunk0)" = "10-60"
check "spoke1 wrote the synced configuration to its mesh.toml" \
  x spoke1 "grep -q '^revision = $want' /etc/wanderlan/mesh.toml && grep -q '^vlans = \"10-60\"' /etc/wanderlan/mesh.toml"
check "spoke3 kept its own local.toml" x spoke3 "grep -q '^self = \"spoke3\"' /etc/wanderlan/local.toml"
wait_for 30 "hub1 reports all peers in sync" \
  x hub1 "wanderlan status --json | jq -e '[.peers[].in_sync] | all'"

begin "Sync: changes based on an outdated revision are refused"

mesh_old=$(x spoke1 "wanderlan export" | sed 's/^vlans = .*/vlans = "10-55"/')
printf '%s\n' "$mesh_old" | put spoke1 /tmp/mesh-old.toml
api_reply=$(x spoke1 "jq -Rsc '{id: 1, method: \"apply\", params: {mesh: ., local: \"self = \\\"spoke1\\\"\\ntrunk = \\\"trunk0\\\"\\n\", base_revision: 1}}' /tmp/mesh-old.toml | timeout 3 nc -U -q 1 /run/wanderlan/wanderlan.sock")
if grep -q 'the configuration changed meanwhile' <<<"$api_reply"; then
  pass "an apply based on revision 1 is refused"
else
  fail "unexpected API reply: $api_reply"
fi

begin "Sync: a new site fetches the whole mesh from its hub"

x spoke3 "wl-daemon stop"
printf '[mesh]\nrevision = 0\nvlans = "10-50"\n\n[[site]]\nname = "spoke3"\nrole = "spoke"\nhub = "hub2"\naddress = "10.100.13.10"\n\n[[site]]\nname = "hub2"\nrole = "hub"\naddress = "10.100.2.10"\n' \
  | put spoke3 /etc/wanderlan/mesh.toml
x spoke3 "wl-daemon start"
wait_eq 90 "spoke3 fetched the mesh from hub2 (revision $want, 5 sites)" "$want 5" \
  jqs spoke3 '"\(.revision) \(.sites | length)"'
check "spoke3 has its mappings again (tc rules from the synced mesh)" \
  x spoke3 "tc filter show dev trunk0 ingress | grep -q 'vlan_id 130'"
expect_ping c-spoke3 10.30.0.1 "local VLAN 130 at spoke3 reaches VLAN 30 at hub1 again"

# Back to VLANs 10-50 for the remaining cases.
sed 's/^vlans = .*/vlans = "10-50"/' "$RUN_DIR/mesh-map.toml" | put hub1 /tmp/mesh-50.toml
x hub1 "wanderlan apply --mesh /tmp/mesh-50.toml >/dev/null && wanderlan confirm >/dev/null"
wait_eq 90 "spoke3 is back at VLANs 10-50" "10-19,20:pu,21-50" vlan_summary spoke3 trunk0
