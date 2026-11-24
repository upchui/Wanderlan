# shellcheck shell=bash
# Five sites: every site has a tunnel to every other site (one virtual
# switch); split horizon keeps it loop-free; VLANs per site.

begin "Five sites: full mesh, split horizon"

mesh_toml "10-50" site1 site2 site3 site4 site5 >"$RUN_DIR/mesh-5.toml"
for s in site1 site2 site3 site4 site5; do
  configure "$s" "$RUN_DIR/mesh-5.toml"
  if x "$s" "wl-daemon status"; then x "$s" "wl-daemon reload"; else x "$s" "wl-daemon start"; fi
done

ports_of() {
  x "$1" "ip -j link show master br0 | jq -r '[.[].ifname] | sort | join(\" \")'"
}
for s in site1 site2 site3 site4 site5; do
  want=$(for o in site1 site2 site3 site4 site5; do [ "$o" = "$s" ] || printf 'gt-%s ' "$o"; done)
  want="${want}trunk0"
  wait_for 30 "$s has tunnels to all four other sites" eval '[ "$(ports_of '"$s"')" = "'"$want"'" ]'
done
check "every tunnel port is isolated (split horizon)" \
  x site3 "for p in gt-site1 gt-site2 gt-site4 gt-site5; do bridge -d link show dev \$p | grep -q 'isolated on' || exit 1; done"
check "the trunk is not isolated" x site3 "bridge -d link show dev trunk0 | grep -q 'isolated off'"

for s in site3 site4 site5; do
  client_vlan "c-$s" 20 "10.20.0.${HOSTNUM[$s]}/24"
done
expect_ping c-site3 10.20.0.12 "VLAN 20: site3 reaches site4"
expect_ping c-site3 10.20.0.13 "VLAN 20: site3 reaches site5"
expect_ping c-site5 10.20.0.1 "VLAN 20: site5 reaches site1's client"
expect_ping c-site2 10.20.0.11 "VLAN 20: site2's client reaches site3"

# Split horizon: a broadcast must never come back to its sender, although
# every site forwards it to every other site.
for c in c-site1 c-site3 c-site5; do
  copies=$(x "$c" 'mac=$(cat /sys/class/net/trunk0/address)
    timeout 4 tcpdump -Q in -nli trunk0 "ether src $mac" 2>/dev/null > /tmp/loop.txt &
    sleep 1
    ping -c 3 -i 0.3 -W 1 -I trunk0.20 10.20.0.250 >/dev/null 2>&1
    wait
    grep -c . /tmp/loop.txt')
  if [ "${copies:-1}" = 0 ]; then
    pass "$c: own broadcasts never come back (no loop)"
  else
    fail "$c: ${copies:-?} frames with its own source MAC came back"
  fi
done

begin "VLANs per site: a site only takes part in its VLANs"

# site5 only takes part in VLAN 20: its tunnels carry VLAN 20 only, VLAN 30
# does not reach it, the other sites keep VLAN 30 among each other.
mesh_toml "10-50" site1 site2 site3 site4 site5=20 >"$RUN_DIR/mesh-5-vlans.toml"
for s in site1 site2 site3 site4 site5; do
  configure "$s" "$RUN_DIR/mesh-5-vlans.toml"
  x "$s" "wl-daemon reload"
done
wait_for 30 "site5's trunk carries VLAN 20 only" eval '[ "$(vlan_summary site5 trunk0)" = 20 ]'
expect_vlans site5 gt-site1 "20"
expect_vlans site1 gt-site5 "20"
expect_vlans site1 gt-site2 "10-50"
for s in site3 site5; do
  client_vlan "c-$s" 30 "10.30.0.${HOSTNUM[$s]}/24"
done
client_vlan c-site4 30 10.30.0.12/24
expect_ping c-site3 10.30.0.12 "VLAN 30: site3 reaches site4"
expect_no_ping c-site3 10.30.0.13 "VLAN 30 does not reach site5 (not one of its VLANs)"
expect_ping c-site3 10.20.0.13 "VLAN 20 still reaches site5"
for s in site3 site4 site5; do
  client_vlan_del "c-$s" 30
done

# A mapping onto a VLAN the site does not take part in is rejected.
MESH_EXTRA='[[map]]
site = "site5"
local = "native"
shared = 30' mesh_toml "10-50" site1 site2 site5=20 >"$RUN_DIR/mesh-bad.toml"
put site1 /tmp/mesh-bad.toml <"$RUN_DIR/mesh-bad.toml"
check "wanderlan validate rejects a mapping onto a VLAN the site does not have" \
  x site1 "! wanderlan validate /tmp/mesh-bad.toml && wanderlan validate /tmp/mesh-bad.toml 2>&1 | grep -q 'not one of the VLANs of site \"site5\"'"

# Back to all VLANs everywhere for the following cases.
renew "$RUN_DIR/mesh-5.toml"
for s in site1 site2 site3 site4 site5; do
  configure "$s" "$RUN_DIR/mesh-5.toml"
  x "$s" "wl-daemon reload"
done
wait_for 30 "site5's trunk carries VLANs 10-50 again" eval '[ "$(vlan_summary site5 trunk0)" = "10-50" ]'
