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

begin "Upgrade from an older build: its configuration file is moved"

rev=$(jqs site4 .revision)
x site4 "wl-daemon stop"
# What an older build left behind: the file under its old name with the old
# table name, the confirmed copy likewise, and the package's new template.
x site4 "sed 's/^\[network\]\$/[mesh]/' /etc/wanderlan/network.toml > /etc/wanderlan/mesh.toml && cp /etc/wanderlan/mesh.toml /var/lib/wanderlan/confirmed/mesh.toml && rm /var/lib/wanderlan/confirmed/network.toml && printf '# This site is not configured yet.\n' > /etc/wanderlan/network.toml"
check "the old file has the old table name" x site4 "grep -qx '\[mesh\]' /etc/wanderlan/mesh.toml"
x site4 "wl-daemon start"
wait_for 30 "site4 runs again" x site4 "wanderlan status --json | jq -e '.state == \"running\"'"
check "network.toml holds the configuration with a [network] table" \
  x site4 "grep -qx '\[network\]' /etc/wanderlan/network.toml && grep -q 'name = \"site4\"' /etc/wanderlan/network.toml"
check "the old files are gone" \
  x site4 "[ ! -e /etc/wanderlan/mesh.toml ] && [ ! -e /var/lib/wanderlan/confirmed/mesh.toml ]"
check "the revision is the same ($rev)" test "$(jqs site4 .revision)" = "$rev"
check "the log says what was moved" x site4 "wanderlan log | grep -q 'to /etc/wanderlan/network.toml'"
expect_ping c-site4 10.20.0.13 "VLAN 20: site4 reaches site5"

begin "A second daemon started by mistake changes nothing"

mark=$(daemon_log_lines site4)
out=$(x site4 "timeout 10 wanderland; echo \"exit=\$?\"" 2>&1)
if grep -q 'already running' <<<"$out" && grep -q 'exit=1' <<<"$out"; then
  pass "a second wanderland refuses to start"
else
  fail "unexpected output of a second wanderland: $out"
fi
out=$(x site4 "script -qec 'timeout 10 wanderland' /dev/null; echo \"exit=\$?\"" 2>&1)
if grep -q 'The user interface is `wanderlan`' <<<"$out" && grep -q 'exit=2' <<<"$out"; then
  pass "started from a terminal, wanderland only explains what it is"
else
  fail "unexpected output of wanderland in a terminal: $out"
fi
check "the API socket is still there" x site4 "test -S /run/wanderlan/wanderlan.sock"
check "the tunnels are still there" x site4 "ip link show gt-site5 >/dev/null"
changes=$(daemon_log_since site4 "$mark" | grep -cE "$CHANGE_RE" || true)
check "the running daemon changed nothing ($changes changes)" test "$changes" = 0
expect_ping c-site4 10.20.0.13 "VLAN 20: site4 still reaches site5"

begin "A deleted API socket is created again"

x site4 "rm -f /run/wanderlan/wanderlan.sock"
wait_for 15 "the daemon created its socket again" \
  x site4 "wanderlan status --json | jq -e '.state == \"running\"'"
check "the log explains it" x site4 "grep -q 'was removed or replaced; creating it again' /var/log/wanderland.log"
