#!/usr/bin/env bash
# wanderland under systemd with the hardened unit of the package, driven
# through what users do: start a network, invite, join, type `wanderland` by
# mistake, delete the socket, reset, start a new network while the old one
# still lists the site, join again, crash, restart, stop, update, remove and
# install again. After every step
# both services must be active, restarted only as expected, and their
# journals free of internal errors.
#   SYSTEMDNODE_IMAGE  image (default wanderlan-systemdnode)
#   KEEP=1             keep the containers
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
COMPOSE=(docker compose -f "$here/compose.yml")
x() { local s="$1"; shift; "${COMPOSE[@]}" exec -T "$s" bash -c "$*"; }

failures=0
failed=()
begin() { printf '\n== %s\n' "$*"; }
pass() { echo "  ok    $*"; }
fail() { echo "  FAIL  $*"; failures=$((failures + 1)); failed+=("$*"); }
# On GitHub also as annotations, 25 lines each: the job log needs a login,
# the annotations of a check run are public.
annotate() { # annotate <title> <text> [error|notice]
  [ "${GITHUB_ACTIONS:-}" = true ] || return 0
  local -a lines
  local i text
  mapfile -t lines <<<"$2"
  for ((i = 0; i < ${#lines[@]}; i += 25)); do
    printf -v text '%s\n' "${lines[@]:i:25}"
    text=${text//'%'/'%25'}
    text=${text//$'\r'/'%0D'}
    text=${text//$'\n'/'%0A'}
    echo "::${3:-error} title=$1 ($((i / 25 + 1)))::$text"
  done
}
check() {
  local what="$1"
  shift
  if "$@" >/dev/null 2>&1; then pass "$what"; else fail "$what"; fi
}
wait_for() {
  local secs="$1" what="$2"
  shift 2
  for _ in $(seq 1 "$secs"); do
    if "$@" >/dev/null 2>&1; then
      pass "$what"
      return 0
    fi
    sleep 1
  done
  fail "$what (not within ${secs}s)"
  return 1
}
expect_output() { # expect_output <what> <pattern> <output>
  if grep -q -- "$2" <<<"$3"; then pass "$1"; else fail "$1: $3"; fi
}

declare -A RESTARTS=([alpha]=0 [beta]=0)
healthy() {
  local s
  for s in alpha beta; do
    check "$s: the service is active" test "$(x "$s" 'systemctl is-active wanderland')" = active
    check "$s: restarted by systemd ${RESTARTS[$s]} times" \
      test "$(x "$s" 'systemctl show -p NRestarts --value wanderland')" = "${RESTARTS[$s]}"
    check "$s: no internal error in the journal" \
      x "$s" "! journalctl -u wanderland -b --no-pager -o cat | grep -qE 'panicked|Internal error|internal error'"
  done
}

state() { x "$1" "wanderlan status --json | jq -r .state"; }
new_network() { # new_network <host> <site name> <address>: what the setup wizard does
  x "$1" "printf '[network]\n\n[[site]]\nname = \"$2\"\naddress = \"$3\"\n' > /tmp/network.toml"
  x "$1" "printf 'self = \"$2\"\ntrunk = \"trunk0\"\n' > /tmp/local.toml"
  x "$1" "wanderlan apply --network /tmp/network.toml --local /tmp/local.toml >/dev/null && wanderlan confirm >/dev/null"
}
invite_code() { x "$1" "wanderlan invite" | awk '$1 == "wanderlan" && $2 == "join" {print $4; exit}'; }
join() { # join <host> <code>
  local out
  out=$(x "$1" "wanderlan join 10.8.0.1 $2" 2>&1)
  expect_output "$1 joined alpha's network" 'is part of the network' "$out"
}
ping_ok() { x c-alpha "ping -c 2 -i 0.2 -W 1 10.20.0.2"; }
# Nothing of wanderlan is left on a host: no bridge, tunnel or segment, no
# nftables table, no tc filter on the trunk, no API socket.
nothing_left() {
  x "$1" "! ip link show br0 2>/dev/null && ! ip -o link show | grep -qE ' (vx|gt|wl)-' \
    && ! nft list table bridge wanderlan 2>/dev/null \
    && [ -z \"\$(tc filter show dev trunk0 ingress 2>/dev/null)\" ] \
    && ! test -e /run/wanderlan/wanderlan.sock"
}
br0_index() { x alpha "ip -j link show br0 | jq -r '.[0].ifindex'"; }
journal_cursor() { x "$1" "journalctl -n 1 --show-cursor --no-pager -o cat | sed -n 's/^-- cursor: //p'"; }
# The journal of a host after a cursor: the service's lines, systemd's about
# it, and those of short-lived processes such as `wanderland --cleanup`,
# which journald cannot always attribute to the unit.
journal_since() { # journal_since <host> <cursor>
  x "$1" "journalctl -b --no-pager -o short-monotonic --after-cursor='$2' \
    _SYSTEMD_UNIT=wanderland.service + UNIT=wanderland.service + SYSLOG_IDENTIFIER=wanderland"
}

cleanup() {
  if [ "$failures" -gt 0 ]; then
    annotate "failed checks" "$(printf '%s\n' "${failed[@]}")"
    for s in alpha beta; do
      echo "--- journal of $s"
      x "$s" "journalctl -u wanderland -b --no-pager -o short-monotonic | tail -n 60"
    done
  fi
  [ "${KEEP:-0}" = 1 ] || "${COMPOSE[@]}" down -v -t 1 >/dev/null 2>&1
}
trap cleanup EXIT
"${COMPOSE[@]}" down -v -t 1 >/dev/null 2>&1
"${COMPOSE[@]}" up -d --quiet-pull || exit 1

begin "Boot: the package's service starts, unconfigured"
for s in alpha beta; do
  wait_for 90 "$s booted" x "$s" "systemctl is-system-running | grep -qE '^(running|degraded)$'"
  wait_for 30 "$s: wanderland answers" x "$s" "wanderlan status --json"
  check "$s is unconfigured" test "$(state "$s")" = unconfigured
done
for c in c-alpha c-beta; do
  wait_for 30 "$c is ready" x "$c" "test -f /run/node-ready"
done
x c-alpha "ip link add link trunk0 name trunk0.20 type vlan id 20 && ip addr add 10.20.0.1/24 dev trunk0.20 && ip link set trunk0.20 up"
x c-beta "ip link add link trunk0 name trunk0.20 type vlan id 20 && ip addr add 10.20.0.2/24 dev trunk0.20 && ip link set trunk0.20 up"
healthy

begin "A new network, an invitation, a join"
check "alpha starts a new network" new_network alpha alpha 10.8.0.1
code=$(invite_code alpha)
check "alpha created an invitation ($code)" test -n "$code"
join beta "$code"
out=$(x alpha "wanderlan online beta --confirm" 2>&1)
expect_output "alpha takes beta online right after the join" 'beta is online' "$out"
wait_for 30 "VLAN 20: the clients reach each other" ping_ok
healthy

begin "wanderland typed by mistake"
out=$(x alpha "timeout 10 wanderland; echo exit=\$?" 2>&1)
expect_output "a second daemon refuses to start" 'already running' "$out"
expect_output "and exits with 1" 'exit=1' "$out"
out=$(x alpha "script -qec 'timeout 10 wanderland' /dev/null; echo exit=\$?" 2>&1)
expect_output "in a terminal it only explains what it is" 'The user interface is `wanderlan`' "$out"
expect_output "and exits with 2" 'exit=2' "$out"
check "the socket is still there" x alpha "test -S /run/wanderlan/wanderlan.sock"
check "the tunnel is still there" x alpha "ip link show vx-beta"
check "VLAN 20 still works" ping_ok
healthy

begin "A deleted socket is created again"
x alpha "rm -f /run/wanderlan/wanderlan.sock"
wait_for 15 "the CLI reaches the daemon again" x alpha "wanderlan status --json"
check "the journal explains it" \
  x alpha "journalctl -u wanderland -b --no-pager -o cat | grep -q 'was removed or replaced'"
healthy

begin "Reset under ProtectSystem=strict"
out=$(x alpha "wanderlan reset --yes" 2>&1)
expect_output "alpha is reset" 'Reset done' "$out"
check "alpha is unconfigured" test "$(state alpha)" = unconfigured
check "no bridge and no tunnels are left" \
  x alpha "! ip link show br0 2>/dev/null && ! ip -o link show | grep -q ' vx-'"
check "no nftables table is left" x alpha "! nft list table bridge wanderlan 2>/dev/null"
check "the state directory exists and is empty" \
  x alpha "[ -d /var/lib/wanderlan ] && [ -z \"\$(ls -A /var/lib/wanderlan)\" ]"
check "the configuration files are the package templates" \
  x alpha "grep -q 'not configured yet' /etc/wanderlan/network.toml && ! grep -q '^self' /etc/wanderlan/local.toml"
out=$(x alpha "wanderlan reset --yes" 2>&1)
expect_output "a second reset succeeds too" 'Reset done' "$out"
healthy

begin "A new network while the old one still lists this site"
check "beta still lists alpha (revision $(x beta 'wanderlan status --json | jq .revision'))" \
  x beta "wanderlan status --json | jq -e '.sites | any(.name == \"alpha\")'"
check "alpha starts a new network anyway" new_network alpha alpha 10.8.0.1
check "alpha runs its new network alone" \
  test "$(x alpha "wanderlan status --json | jq -r '\"\(.state) \(.sites | length) \(.peers | length)\"'")" = "running 1 0"
healthy

begin "beta leaves and joins the new network"
out=$(x beta "wanderlan reset --yes" 2>&1)
expect_output "beta is reset" 'Reset done' "$out"
code=$(invite_code alpha)
join beta "$code"
out=$(x alpha "wanderlan online beta --confirm" 2>&1)
expect_output "alpha takes beta online right after the join" 'beta is online' "$out"
wait_for 30 "VLAN 20: the clients reach each other again" ping_ok
healthy

begin "Local breakout under the hardened unit: sysctls written and put back"
uplink=$(x beta "ip -o -4 addr show | awk '/ 10\\.8\\.0\\.2\\// {print \$2}'")
forwarding() { x beta "cat /proc/sys/net/ipv4/conf/$uplink/forwarding"; }
x beta "ip route replace default via 10.8.0.254; sysctl -qw net.ipv4.conf.$uplink.forwarding=0"
out=$(x beta "wanderlan breakout beta 20 on --confirm" 2>&1)
expect_output "beta (restricted) may not break out by itself" 'may not' "$out"
check "alpha (its manager) lets beta break out VLAN 20" \
  x alpha "wanderlan breakout beta 20 on --confirm"
wait_for 30 "beta: the breakout of VLAN 20 is in place, out of $uplink" \
  x beta "wanderlan status --json | jq -e '.breakout[0].ready and .breakout[0].ipv4.dev == \"$uplink\"'"
check "beta: the daemon turned on forwarding on $uplink (ProtectKernelTunables, ReadWritePaths)" \
  test "$(forwarding)" = 1
check "beta: and wrote down the value before" x beta "grep -q 'ipv4/conf/$uplink/forwarding' /var/lib/wanderlan/breakout-sysctl"
check "beta: the nftables tables are there (the NAT modules load)" \
  x beta "nft list table inet wanderlan-breakout >/dev/null && nft list table bridge wanderlan-breakout >/dev/null"
x beta "systemctl stop wanderland"
check "beta: after a stop forwarding on $uplink is off again" test "$(forwarding)" = 0
check "beta: no breakout interface, table or rule is left" \
  x beta "! ip link show wlbo20 2>/dev/null && ! nft list table inet wanderlan-breakout 2>/dev/null && ! ip rule show | grep -q 'proto 87' && [ ! -e /var/lib/wanderlan/breakout-sysctl ]"
x beta "systemctl start wanderland"
wait_for 30 "beta: the breakout is back after the start" \
  x beta "wanderlan status --json | jq -e '.breakout[0].ready'"
check "alpha takes the breakout of beta back" x alpha "wanderlan breakout beta 20 off --confirm"
wait_for 30 "beta: forwarding on $uplink is off again" eval '[ "$(forwarding)" = 0 ]'
x beta "ip route del default via 10.8.0.254"
wait_for 30 "VLAN 20: the clients still reach each other" ping_ok
healthy

begin "A crash: the cleanup removes what is left, the restart builds it anew"
before=$(br0_index)
cursor=$(journal_cursor alpha)
failures_before=$failures
# systemd's debug messages show what became of ExecStopPost.
x alpha "systemd-analyze log-level debug"
x alpha "systemctl kill -s KILL wanderland"
RESTARTS[alpha]=1
wait_for 20 "systemd restarted wanderland" \
  x alpha "systemctl is-active wanderland | grep -qx active && wanderlan status --json | jq -e '.state == \"running\"'"
wait_for 10 "the cleanup after the crash removed the interfaces of the dead daemon" \
  eval 'journal_since alpha "$cursor" | grep "Cleanup: .* changes undone" >/dev/null'
wait_for 30 "the new daemon built br0 anew" eval '[ -n "$(br0_index)" ] && [ "$(br0_index)" != "'"$before"'" ]'
x alpha "systemd-analyze log-level info"
# Which hook cleaned up: ExecStopPost, or (where the kernel killed it at
# once) the ExecStartPre of the restart.
hooks=$(x alpha "journalctl -b --no-pager -o cat --after-cursor='$cursor' UNIT=wanderland.service \
  | grep -E 'Control process exited|stop-post|start-pre' | grep -v 'Got notification'")
echo "  info  the hooks after the kill:"
sed 's/^/          /' <<<"$hooks"
annotate "hooks after the kill" "$hooks" notice
if [ "$failures" -gt "$failures_before" ]; then
  report=$(
    echo "--- journal of alpha since the kill (all units, without debug messages)"
    x alpha "journalctl -b -p info --no-pager -o short-monotonic --after-cursor='$cursor' | grep -v 'modprobe: WARNING'"
    echo "--- links of alpha"
    x alpha "ip -br link"
    echo "--- nftables tables of alpha"
    x alpha "nft list tables"
  )
  printf '%s\n' "$report"
  annotate "crash cleanup" "$report"
fi
wait_for 30 "VLAN 20 works" ping_ok
healthy

begin "systemctl restart and stop/start"
x alpha "systemctl restart wanderland"
RESTARTS[alpha]=0 # an explicit start resets systemd's counter
wait_for 20 "alpha runs again after a restart" x alpha "wanderlan status --json | jq -e '.state == \"running\"'"
wait_for 30 "VLAN 20 works after the restart" ping_ok
x alpha "systemctl stop wanderland"
check "stopping removes everything: interfaces, nftables, tc, socket" nothing_left alpha
x alpha "systemctl start wanderland"
wait_for 20 "alpha runs again after a start" x alpha "wanderlan status --json | jq -e '.state == \"running\"'"
wait_for 30 "VLAN 20 works after the start" ping_ok
healthy

begin "An update restarts the daemon"
pid=$(x alpha "systemctl show -p MainPID --value wanderland")
x alpha "dpkg -i /var/cache/wanderlan-test/wanderlan.deb >/dev/null 2>&1"
RESTARTS[alpha]=0
wait_for 20 "a new daemon runs after the update" \
  x alpha "[ \"\$(systemctl show -p MainPID --value wanderland)\" != '$pid' ] && wanderlan status --json | jq -e '.state == \"running\"'"
check "the old one removed everything first" \
  x alpha "journalctl -u wanderland -b --no-pager -o cat | grep -q 'Stopping; removing everything wanderlan created'"
wait_for 30 "VLAN 20 works after the update" ping_ok
healthy

begin "Removing the package removes everything, installing it again brings it back"
x alpha "dpkg -r wanderlan >/dev/null 2>&1"
check "the package is removed" x alpha "! test -x /usr/bin/wanderland"
check "nothing of wanderlan is left" nothing_left alpha
check "the configuration is kept" x alpha "grep -qx '\[site.alpha\]' /etc/wanderlan/network.toml"
x alpha "dpkg -i /var/cache/wanderlan-test/wanderlan.deb >/dev/null 2>&1"
wait_for 20 "alpha runs again with its configuration" \
  x alpha "systemctl is-active wanderland | grep -qx active && wanderlan status --json | jq -e '.state == \"running\"'"
wait_for 30 "VLAN 20 works again" ping_ok
healthy

echo
if [ "$failures" -eq 0 ]; then
  echo "systemd test: all checks passed"
else
  echo "systemd test: $failures checks FAILED"
fi
[ "$failures" -eq 0 ]
