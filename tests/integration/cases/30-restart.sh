# shellcheck shell=bash
# Restarts: a site container restart and a daemon crash.

begin "Restart of a site: the others reconnect without intervention"

"${COMPOSE[@]}" restart -t 2 site1 >/dev/null 2>&1
wait_for 30 "site1 is back" x site1 "test -f /run/node-ready"
wait_for 30 "site1's daemon rebuilt its tunnels" \
  x site1 "[ \"\$(ip -j link show master br0 | jq -r '[.[].ifname] | sort | join(\" \")')\" = 'gt-site2 gt-site3 gt-site4 gt-site5 trunk0' ]"
expect_ping c-site3 10.20.0.12 "VLAN 20: site3 reaches site4 again"
expect_ping c-site3 10.20.0.13 "VLAN 20: site3 reaches site5 again"
expect_ping c-site5 10.20.0.11 "VLAN 20: site5 reaches site3 again"

begin "Daemon crash: the restarted daemon adopts the existing interfaces"

x site4 'kill -9 "$(cat /run/wanderland.pid)"; rm -f /run/wanderland.pid'
check "site4's interfaces survive the crash" x site4 "ip link show gt-site1"
mark=$(daemon_log_lines site4)
x site4 "wl-daemon start"
sleep 6
changes=$(daemon_log_since site4 "$mark" | grep -cE "$CHANGE_RE" || true)
if [ "$changes" = 0 ]; then
  pass "the restarted daemon changed nothing"
else
  fail "the restarted daemon made $changes changes"
  daemon_log_since site4 "$mark"
fi
expect_ping c-site4 10.20.0.13 "VLAN 20: site4 reaches site5"
