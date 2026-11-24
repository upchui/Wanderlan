# shellcheck shell=bash
# VLAN mappings at site5: untagged -> shared 20, local 130 -> shared 30.

begin "VLAN mappings: native VLAN and VLAN translation"

MESH_EXTRA='[[map]]
site = "site5"
local = "native"
shared = 20

[[map]]
site = "site5"
local = 130
shared = 30' mesh_toml "10-50" site1 site2 site3 site4 site5 \
  >"$RUN_DIR/mesh-map.toml"
for s in site1 site2 site3 site4 site5; do
  configure "$s" "$RUN_DIR/mesh-map.toml"
  x "$s" "wl-daemon reload"
done

wait_eq 20 "site5: trunk0 has VLAN 20 as untagged PVID" "10-19,20:pu,21-50" \
  vlan_summary site5 trunk0
check "site5: ingress rule translates VLAN 130 to 30" \
  x site5 "tc filter show dev trunk0 ingress | grep -A8 'vlan_id 130' | grep -q 'modify id 30'"
check "site5: egress rule translates VLAN 30 back to 130" \
  x site5 "tc filter show dev trunk0 egress | grep -A8 'vlan_id 30' | grep -q 'modify id 130'"
check "site5: incoming frames tagged with VLAN 30 are dropped" \
  x site5 "tc filter show dev trunk0 ingress | grep -A8 'vlan_id 30' | grep -q 'gact action drop'"
check "site2 (no mappings) has no tc rules" x site2 "[ -z \"\$(tc filter show dev trunk0 ingress 2>/dev/null)\" ]"

# Native mapping: an untagged client at site5 is in VLAN 20 everywhere.
client_vlan_del c-site5 20
client_native c-site5 10.20.0.13/24
expect_ping c-site5 10.20.0.1 "untagged client at site5 reaches VLAN 20 at site1"
expect_ping c-site3 10.20.0.13 "VLAN 20 at site3 reaches the untagged client at site5"

# Translation: local VLAN 130 at site5 is shared VLAN 30.
client_vlan c-site5 130 10.30.0.13/24
client_vlan c-site1 30 10.30.0.1/24
client_vlan c-site4 30 10.30.0.12/24
expect_ping c-site5 10.30.0.1 "local VLAN 130 at site5 reaches VLAN 30 at site1"
expect_ping c-site4 10.30.0.13 "VLAN 30 at site4 reaches local VLAN 130 at site5"
check "site5's client sees VLAN 130 tagged frames back (translated on egress)" \
  x c-site5 "ip neigh show dev trunk0.130 | grep -q '10.30.0.1 .*lladdr'"

# Tagged VLAN 30 at site5 must not leak into shared VLAN 30.
client_vlan c-site5 30 10.30.0.113/24
x c-site5 "ip route del 10.30.0.0/24 dev trunk0.130 2>/dev/null; true"
expect_no_ping c-site5 10.30.0.1 "tagged VLAN 30 at site5 is dropped (it would merge with VLAN 130)"
client_vlan_del c-site5 30
client_vlan c-site5 130 10.30.0.13/24

# Conflicting mappings are rejected.
MESH_EXTRA='[[map]]
site = "site5"
local = "native"
shared = 20

[[map]]
site = "site5"
local = "native"
shared = 21' mesh_toml "10-50" site1 site2 site5 >"$RUN_DIR/mesh-conflict.toml"
put site1 /tmp/mesh-conflict.toml <"$RUN_DIR/mesh-conflict.toml"
check "wanderlan validate rejects two native mappings at one site" \
  x site1 "wanderlan validate /tmp/mesh-conflict.toml 2>&1 | grep -q 'more than one native mapping'"

# Idempotence with translation rules.
mark=$(daemon_log_lines site5)
x site5 "wl-daemon reload"
sleep 7
changes=$(daemon_log_since site5 "$mark" | grep -cE "$CHANGE_RE" || true)
if [ "$changes" = 0 ]; then
  pass "site5: reloading the configuration with mappings changes nothing"
else
  fail "site5: reloading the configuration made $changes changes"
  daemon_log_since site5 "$mark"
fi
