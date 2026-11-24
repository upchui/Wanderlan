# shellcheck shell=bash
# Loop guard: a layer-2 loop outside wanderlan (spoke1's and spoke2's LANs
# bridged together) makes MAC addresses flap at hub1, which shuts a port.

begin "Loop guard: an external loop between two spokes is cut"

expect_ping c-spoke1 10.20.0.12 "VLAN 20: spoke1 reaches spoke2 before the loop"
x looper "ip link set loopa master brl; ip link set loopb master brl; ip link set loopa up; ip link set loopb up"
# Some traffic so that addresses move.
x c-spoke1 "ping -c 5 -i 0.2 -W 1 10.20.0.12 >/dev/null 2>&1" || true
wait_for 30 "hub1 detects the loop and blocks a spoke port" \
  x hub1 "wanderlan status --json | jq -e '.blocked | length == 1'"
blocked=$(jqs hub1 '.blocked[0]')
info "blocked port: $blocked"
check "the blocked port is a spoke port of hub1" grep -qE '^gt-spoke[12]$' <<<"$blocked"
check "the blocked port is down" x hub1 "ip -j link show $blocked | jq -e '.[0].flags | index(\"UP\") | not'"
check "the event log explains the shutdown" x hub1 "wanderlan log | grep -q 'Loop detected: MAC .* shutting down $blocked'"
check "the status shows the port as blocked" \
  x hub1 "wanderlan status --json | jq -e '.peers[] | select(.port == \"$blocked\") | .port_state == \"blocked\"'"

# Remove the loop and re-enable the port.
x looper "ip link set loopa nomaster; ip link set loopb nomaster; ip link set loopa down; ip link set loopb down"
check "wanderlan unblock re-enables the port" x hub1 "wanderlan unblock $blocked"
wait_for 20 "the port is up again" x hub1 "ip -j link show $blocked | jq -e '.[0].flags | index(\"UP\")'"
expect_ping c-spoke1 10.20.0.12 "VLAN 20: spoke1 reaches spoke2 again"
expect_ping c-spoke2 10.20.0.13 "VLAN 20: spoke2 reaches spoke3 again"
