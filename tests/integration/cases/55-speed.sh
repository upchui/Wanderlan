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

# /sys is read-only in these containers: the calibration says so and
# changes nothing (the systemd test calibrates for real).
tune=$(x site1 "wanderlan tune site5" 2>&1)
check "tune: RPS cannot be changed here, and says why" grep -q 'RPS cannot be changed here (/sys cannot be written here' <<<"$tune"

perf=$(x site5 "wanderlan perf --seconds 1" 2>&1)
check "perf names the path MTU of site5" grep -q 'Path MTU to "site[0-9]" is 1400: packets over 1346 bytes are split in two outside' <<<"$perf"
check "perf looks at the receive queues of the trunk" grep -q 'trunk0: ' <<<"$perf"

# Connection tracking is loaded in the test kernel (the NAT box uses it):
# a server skips it for its tunnel packets, both ways.
if x site1 "test -e /proc/sys/net/netfilter/nf_conntrack_max"; then
  check "site1 (a server) skips connection tracking for its tunnel packets, also those to other servers" \
    x site1 "nft list chain ip wanderlan out | grep -q 'udp dport 51822 notrack' && nft list chain ip wanderlan pre | grep -q 'udp dport 51822 fib daddr type local notrack'"
else
  info "connection tracking not loaded here: no notrack rules expected"
fi
