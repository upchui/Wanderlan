# shellcheck shell=bash
# Loop guard: the LANs of site3 and site4 are also connected outside
# wanderlan (the looper bridges them). Together with the tunnel between the
# two sites this is a layer-2 loop; MAC addresses keep moving between the
# trunk and the tunnel, and the tunnel between site3 and site4 is shut down.

begin "Loop guard: an external loop between two sites is cut"

expect_ping c-site3 10.20.0.12 "VLAN 20: site3 reaches site4 before the loop"
x looper "ip link set loopa master brl; ip link set loopb master brl; ip link set loopa up; ip link set loopb up"
# A loop only starts with a broadcast (or unknown unicast): known unicast
# follows the learned path and never enters the second one. Clear the ARP
# cache and ask for an unused address, so ARP broadcasts start circling.
x c-site3 "ip neigh flush dev trunk0.20; ping -c 3 -i 0.5 -W 1 10.20.0.250 >/dev/null 2>&1" || true

blocks() { x "$1" "wanderlan status --json | jq -e '.blocked | index(\"$2\")'"; }
cut() { blocks site3 gt-site4 || blocks site4 gt-site3; }
wait_for 30 "site3 or site4 shuts down the tunnel between them" cut
if blocks site3 gt-site4 >/dev/null 2>&1; then
  site=site3 port=gt-site4
else
  site=site4 port=gt-site3
fi
info "$site blocked $port"
check "$site: the blocked port is down" x "$site" "ip -j link show $port | jq -e '.[0].flags | index(\"UP\") | not'"
check "$site: the event log explains the shutdown" \
  x "$site" "wanderlan log | grep -q 'Loop detected: MAC .* shutting down $port'"
check "$site: the status shows the port as blocked" \
  x "$site" "wanderlan status --json | jq -e '.peers[] | select(.port == \"$port\") | .port_state == \"blocked\"'"
check "$site: the trunk is never shut down" \
  x "$site" "ip -j link show trunk0 | jq -e '.[0].flags | index(\"UP\")'"

# Remove the loop and re-enable every port the loop guard shut down.
x looper "ip link set loopa nomaster; ip link set loopb nomaster; ip link set loopa down; ip link set loopb down"
for s in site1 site2 site3 site4 site5; do
  for p in $(jqs "$s" '.blocked[]' 2>/dev/null); do
    check "$s: wanderlan unblock $p" x "$s" "wanderlan unblock $p"
  done
done
wait_for 20 "the tunnel is up again" x "$site" "ip -j link show $port | jq -e '.[0].flags | index(\"UP\")'"
expect_ping c-site3 10.20.0.12 "VLAN 20: site3 reaches site4 again"
expect_ping c-site4 10.20.0.13 "VLAN 20: site4 reaches site5 again"
