# shellcheck shell=bash
# Throughput: the speed test between two sites, the diagnosis, and the
# tunnel packets of a server outside connection tracking. After case 50:
# the underlay of site5 has MTU 1400 (full frames are split there).

begin "Throughput: speed test, diagnosis, no connection tracking of tunnel packets"

# run_of <json> <frame|null> <up>: "<bit/s> <arrived> <error>" of one run.
run_of() {
  jq -r --argjson f "$2" --argjson up "$3" \
    '.runs[] | select(.frame == $f and .up == $up) | "\(.bits_per_second) \(.arrived) \(.error)"' <<<"$1"
}
# The values are read first, then checked (no `x` inside a pipe).
speed=$(x site1 "wanderlan speedtest site5 --seconds 2 --json" 2>&1)
for case in "null true TCP to site5" "null false TCP from site5" \
  "1514 true full frames to site5" "1514 false full frames from site5" \
  "1364 true frames that fit the path to site5" "1364 false frames that fit the path from site5"; do
  read -r frame up what <<<"$case"
  read -r bps arrived error <<<"$(run_of "$speed" "$frame" "$up")"
  if [ "${error:-x}" = null ] && [ "${bps:-0}" -gt 0 ] && [ "$arrived" != 0 ]; then
    pass "speedtest: $what ($((bps / 1000000)) Mbit/s)"
  else
    fail "speedtest: $what: '$bps $arrived $error' in $speed"
  fi
done
text=$(x site1 "wanderlan speedtest site5 --seconds 1" 2>&1)
check "the text names the split frames" grep -q "Path MTU 1400: full frames are split in two outside" <<<"$text"

# RPS is a setting of each site: by default off, and nothing is measured.
tune=$(x site1 "wanderlan tune site5" 2>&1)
check "tune: RPS is fixed by the setting (off by default)" grep -q 'RPS is fixed by the setting rps_transport = off' <<<"$tune"
# auto on both: /sys is read-only in these containers, so the calibration
# says why and changes nothing (the systemd test calibrates for real).
for s in site1 site5; do
  check "$s: rps-transport auto (a setting of this site)" x "$s" "wanderlan settings rps-transport auto --confirm"
done
check "the setting is in local.toml" x site1 "grep -qx 'rps_transport = \"auto\"' /etc/wanderlan/local.toml"
tune=$(x site1 "wanderlan tune site5" 2>&1)
check "tune: RPS cannot be changed here, and says why" grep -q 'RPS cannot be changed here (/sys cannot be written here' <<<"$tune"
check "the status keeps it for the peer (shown in the terminal UI)" \
  x site1 "wanderlan status --json | jq -e '.peers[] | select(.name == \"site5\") | .tuning.state == \"unchangeable\"'"
for s in site1 site5; do
  x "$s" "wanderlan settings rps-transport default --confirm" >/dev/null
done

perf=$(x site5 "wanderlan perf --seconds 1" 2>&1)
check "perf names the path MTU of site5" grep -q 'Path MTU to "site[0-9]" is 1400: packets over 1346 bytes are split in two outside' <<<"$perf"
check "perf looks at the receive queues of the trunk" grep -q 'trunk0: ' <<<"$perf"

# Connection tracking is loaded in the test kernel (the NAT box uses it):
# which tunnel packets skip it is a setting. site1 is a server without
# clients: by default (clients) it tracks them, with all it skips them both
# ways, with off again not.
if x site1 "test -e /proc/sys/net/netfilter/nf_conntrack_max"; then
  notrack() { x site1 "nft list table ip wanderlan 2>/dev/null | grep -c notrack"; }
  check "site1 (a server without clients) tracks its tunnel packets by default" eval '[ "$(notrack)" = 0 ]'
  check "skip-conntrack all" x site1 "wanderlan settings skip-conntrack all --confirm"
  wait_for 20 "site1 skips connection tracking for its tunnel packets, also those to other servers" \
    x site1 "nft list chain ip wanderlan out | grep -q 'udp dport 51822 notrack' && nft list chain ip wanderlan pre | grep -q 'udp dport 51822 fib daddr type local notrack'"
  check "skip-conntrack off" x site1 "wanderlan settings skip-conntrack off --confirm"
  wait_for 20 "site1 tracks them all again" eval '[ "$(notrack)" = 0 ]'
  x site1 "wanderlan settings skip-conntrack default --confirm" >/dev/null
else
  info "connection tracking not loaded here: no notrack rules expected"
fi
