# shellcheck shell=bash
# Five sites: two hubs, two spokes at hub1, one spoke at hub2.

begin "Hub and spoke: five sites, split horizon"

mesh_toml "10-50" hub1:hub hub2:hub spoke1:spoke:hub1 spoke2:spoke:hub1 spoke3:spoke:hub2 \
  >"$RUN_DIR/mesh-5.toml"
for s in hub1 hub2 spoke1 spoke2 spoke3; do
  configure "$s" "$RUN_DIR/mesh-5.toml"
  if x "$s" "wl-daemon status"; then x "$s" "wl-daemon reload"; else x "$s" "wl-daemon start"; fi
done

wait_for 30 "hub1 has tunnels to hub2, spoke1 and spoke2" \
  x hub1 "[ \"\$(ip -j link show master br0 | jq -r '[.[].ifname] | sort | join(\" \")')\" = 'gt-hub2 gt-spoke1 gt-spoke2 trunk0' ]"
wait_for 30 "hub2 has tunnels to hub1 and spoke3" \
  x hub2 "[ \"\$(ip -j link show master br0 | jq -r '[.[].ifname] | sort | join(\" \")')\" = 'gt-hub1 gt-spoke3 trunk0' ]"
for s in spoke1 spoke2; do
  wait_for 30 "$s has exactly one tunnel, to hub1" \
    x "$s" "[ \"\$(ip -j link show master br0 | jq -r '[.[].ifname] | sort | join(\" \")')\" = 'gt-hub1 trunk0' ]"
done
wait_for 30 "spoke3 has exactly one tunnel, to hub2" \
  x spoke3 "[ \"\$(ip -j link show master br0 | jq -r '[.[].ifname] | sort | join(\" \")')\" = 'gt-hub2 trunk0' ]"

check "hub1: port to hub2 is isolated" x hub1 "bridge -d link show dev gt-hub2 | grep -q 'isolated on'"
check "hub1: ports to spokes are not isolated" \
  x hub1 "bridge -d link show dev gt-spoke1 | grep -q 'isolated off' && bridge -d link show dev gt-spoke2 | grep -q 'isolated off'"
check "spoke1: port to its hub is not isolated" x spoke1 "bridge -d link show dev gt-hub1 | grep -q 'isolated off'"

for s in spoke1 spoke2 spoke3; do
  client_vlan "c-$s" 20 "10.20.0.${HOSTNUM[$s]}/24"
done

expect_ping c-spoke1 10.20.0.12 "VLAN 20: spoke1 reaches spoke2 (same hub)"
expect_ping c-spoke1 10.20.0.13 "VLAN 20: spoke1 reaches spoke3 (different hubs)"
expect_ping c-spoke3 10.20.0.1 "VLAN 20: spoke3 reaches hub1's client"
expect_ping c-hub2 10.20.0.11 "VLAN 20: hub2's client reaches spoke1"

# Split horizon: a broadcast must never come back to its sender. c-spoke1
# sends ARP requests for an unused address and watches its trunk for
# incoming frames with its own source MAC.
loop_copies=$(x c-spoke1 'mac=$(cat /sys/class/net/trunk0/address)
  timeout 4 tcpdump -Q in -nli trunk0 "ether src $mac" 2>/dev/null > /tmp/loop.txt &
  sleep 1
  ping -c 3 -i 0.3 -W 1 -I trunk0.20 10.20.0.250 >/dev/null 2>&1
  wait
  grep -c . /tmp/loop.txt')
if [ "${loop_copies:-1}" = 0 ]; then
  pass "spoke1's own broadcasts never come back (no loop)"
else
  fail "${loop_copies:-?} frames with spoke1's own source MAC came back"
fi

# Topology check: a spoke that names another spoke as its hub is rejected.
mesh_toml "10-50" hub1:hub hub2:hub spoke1:spoke:hub1 spoke2:spoke:spoke1 >"$RUN_DIR/mesh-bad.toml"
put hub1 /tmp/mesh-bad.toml <"$RUN_DIR/mesh-bad.toml"
check "wanderlan validate rejects a spoke whose hub is a spoke" \
  x hub1 "! wanderlan validate /tmp/mesh-bad.toml && wanderlan validate /tmp/mesh-bad.toml 2>&1 | grep -q 'is a spoke, not a hub'"

begin "Three hubs (full mesh triangle): split horizon prevents loops"

# Turn spoke3 into a third hub: hub1, hub2 and spoke3 form a triangle, which
# would loop without split horizon.
mesh_toml "10-50" hub1:hub hub2:hub spoke3:hub spoke1:spoke:hub1 spoke2:spoke:hub1 \
  >"$RUN_DIR/mesh-3hubs.toml"
for s in hub1 hub2 spoke1 spoke2 spoke3; do
  configure "$s" "$RUN_DIR/mesh-3hubs.toml"
  x "$s" "wl-daemon reload"
done
wait_for 30 "spoke3 (now a hub) has isolated tunnels to hub1 and hub2" \
  x spoke3 "bridge -d link show dev gt-hub1 | grep -q 'isolated on' && bridge -d link show dev gt-hub2 | grep -q 'isolated on'"
wait_for 30 "hub1 has a tunnel to spoke3" x hub1 "bridge -d link show dev gt-spoke3 | grep -q 'isolated on'"
expect_ping c-spoke1 10.20.0.13 "VLAN 20: spoke1 reaches the third hub's client"
for c in c-spoke1 c-hub2 c-spoke3; do
  copies=$(x "$c" 'mac=$(cat /sys/class/net/trunk0/address)
    timeout 4 tcpdump -Q in -nli trunk0 "ether src $mac" 2>/dev/null > /tmp/loop.txt &
    sleep 1
    ping -c 3 -i 0.3 -W 1 -I trunk0.20 10.20.0.251 >/dev/null 2>&1
    wait
    grep -c . /tmp/loop.txt')
  if [ "${copies:-1}" = 0 ]; then
    pass "$c: own broadcasts never come back"
  else
    fail "$c: ${copies:-?} frames with its own source MAC came back"
  fi
done

# Back to the five-site topology for the following cases.
renew "$RUN_DIR/mesh-5.toml"
for s in hub1 hub2 spoke1 spoke2 spoke3; do
  configure "$s" "$RUN_DIR/mesh-5.toml"
  x "$s" "wl-daemon reload"
done
wait_for 30 "spoke3 is a spoke of hub2 again" \
  x spoke3 "[ \"\$(ip -j link show master br0 | jq -r '[.[].ifname] | sort | join(\" \")')\" = 'gt-hub2 trunk0' ]"
wait_for 30 "hub1 dropped its tunnel to spoke3" x hub1 "! ip link show gt-spoke3"
