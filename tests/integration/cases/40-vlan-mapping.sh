# shellcheck shell=bash
# VLAN mappings at spoke3: untagged -> shared 20, local 130 -> shared 30.

begin "VLAN mappings: native VLAN and VLAN translation"

MESH_EXTRA='[[map]]
site = "spoke3"
local = "native"
shared = 20

[[map]]
site = "spoke3"
local = 130
shared = 30' mesh_toml "10-50" hub1:hub hub2:hub spoke1:spoke:hub1 spoke2:spoke:hub1 spoke3:spoke:hub2 \
  >"$RUN_DIR/mesh-map.toml"
for s in hub1 hub2 spoke1 spoke2 spoke3; do
  configure "$s" "$RUN_DIR/mesh-map.toml"
  x "$s" "wl-daemon reload"
done

wait_for 20 "spoke3: trunk0 has VLAN 20 as untagged PVID" \
  test "$(vlan_summary spoke3 trunk0)" = "10-19,20:pu,21-50"
check "spoke3: ingress rule translates VLAN 130 to 30" \
  x spoke3 "tc filter show dev trunk0 ingress | grep -A8 'vlan_id 130' | grep -q 'modify id 30'"
check "spoke3: egress rule translates VLAN 30 back to 130" \
  x spoke3 "tc filter show dev trunk0 egress | grep -A8 'vlan_id 30' | grep -q 'modify id 130'"
check "spoke3: incoming frames tagged with VLAN 30 are dropped" \
  x spoke3 "tc filter show dev trunk0 ingress | grep -A8 'vlan_id 30' | grep -q 'gact action drop'"
check "hub2 (no mappings) has no tc rules" x hub2 "[ -z \"\$(tc filter show dev trunk0 ingress 2>/dev/null)\" ]"

# Native mapping: an untagged client at spoke3 is in VLAN 20 everywhere.
client_vlan_del c-spoke3 20
client_native c-spoke3 10.20.0.13/24
expect_ping c-spoke3 10.20.0.1 "untagged client at spoke3 reaches VLAN 20 at hub1"
expect_ping c-spoke1 10.20.0.13 "VLAN 20 at spoke1 reaches the untagged client at spoke3"

# Translation: local VLAN 130 at spoke3 is shared VLAN 30.
client_vlan c-spoke3 130 10.30.0.13/24
client_vlan c-hub1 30 10.30.0.1/24
client_vlan c-spoke2 30 10.30.0.12/24
expect_ping c-spoke3 10.30.0.1 "local VLAN 130 at spoke3 reaches VLAN 30 at hub1"
expect_ping c-spoke2 10.30.0.13 "VLAN 30 at spoke2 reaches local VLAN 130 at spoke3"
check "spoke3's client sees VLAN 130 tagged frames back (translated on egress)" \
  x c-spoke3 "ip neigh show dev trunk0.130 | grep -q '10.30.0.1 .*lladdr'"

# Tagged VLAN 30 at spoke3 must not leak into shared VLAN 30.
client_vlan c-spoke3 30 10.30.0.113/24
x c-spoke3 "ip route del 10.30.0.0/24 dev trunk0.130 2>/dev/null; true"
expect_no_ping c-spoke3 10.30.0.1 "tagged VLAN 30 at spoke3 is dropped (it would merge with VLAN 130)"
client_vlan_del c-spoke3 30
client_vlan c-spoke3 130 10.30.0.13/24

# Conflicting mappings are rejected.
MESH_EXTRA='[[map]]
site = "spoke3"
local = "native"
shared = 20

[[map]]
site = "spoke3"
local = "native"
shared = 21' mesh_toml "10-50" hub1:hub hub2:hub spoke3:spoke:hub2 >"$RUN_DIR/mesh-conflict.toml"
put hub1 /tmp/mesh-conflict.toml <"$RUN_DIR/mesh-conflict.toml"
check "wanderlan validate rejects two native mappings at one site" \
  x hub1 "wanderlan validate /tmp/mesh-conflict.toml 2>&1 | grep -q 'more than one native mapping'"

# Idempotence with translation rules.
mark=$(daemon_log_lines spoke3)
x spoke3 "wl-daemon reload"
sleep 7
changes=$(daemon_log_since spoke3 "$mark" | grep -cE "$CHANGE_RE" || true)
if [ "$changes" = 0 ]; then
  pass "spoke3: reloading the configuration with mappings changes nothing"
else
  fail "spoke3: reloading the configuration made $changes changes"
  daemon_log_since spoke3 "$mark"
fi
