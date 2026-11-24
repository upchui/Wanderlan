# shellcheck shell=bash
# Restarts: a hub container restart and a daemon crash.

begin "Restart of a hub: spokes reconnect without intervention"

"${COMPOSE[@]}" restart -t 2 hub1 >/dev/null 2>&1
wait_for 30 "hub1 is back" x hub1 "test -f /run/node-ready"
wait_for 30 "hub1's daemon rebuilt its tunnels" \
  x hub1 "[ \"\$(ip -j link show master br0 | jq -r '[.[].ifname] | sort | join(\" \")')\" = 'gt-hub2 gt-spoke1 gt-spoke2 trunk0' ]"
expect_ping c-spoke1 10.20.0.12 "VLAN 20: spoke1 reaches spoke2 again"
expect_ping c-spoke1 10.20.0.13 "VLAN 20: spoke1 reaches spoke3 again"
expect_ping c-spoke3 10.20.0.11 "VLAN 20: spoke3 reaches spoke1 again"

begin "Daemon crash: the restarted daemon adopts the existing interfaces"

x spoke2 'kill -9 "$(cat /run/wanderland.pid)"; rm -f /run/wanderland.pid'
check "spoke2's interfaces survive the crash" x spoke2 "ip link show gt-hub1"
mark=$(daemon_log_lines spoke2)
x spoke2 "wl-daemon start"
sleep 6
changes=$(daemon_log_since spoke2 "$mark" | grep -cE "$CHANGE_RE" || true)
if [ "$changes" = 0 ]; then
  pass "the restarted daemon changed nothing"
else
  fail "the restarted daemon made $changes changes"
  daemon_log_since spoke2 "$mark"
fi
expect_ping c-spoke2 10.20.0.13 "VLAN 20: spoke2 reaches spoke3"
