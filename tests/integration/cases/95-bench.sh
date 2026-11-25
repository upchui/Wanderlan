# shellcheck shell=bash
# Throughput through the tunnels (only with BENCH=1): GRE with a key between
# two servers, GRE in UDP between a client behind NAT and its server, each
# with one and with four TCP streams, and the CPU it costs. All containers
# share the host's kernel and CPUs: the numbers compare the kinds of tunnel
# on this machine, they are no absolute measure.

if [ "${BENCH:-0}" = 1 ]; then
  begin "Benchmark: throughput and CPU per Gbit/s through the tunnels"

  # Busy and total jiffies of the host (user, nice, system, irq, softirq).
  cpu_jiffies() {
    x "$1" "awk '/^cpu / {print \$2+\$3+\$4+\$7+\$8, \$2+\$3+\$4+\$5+\$6+\$7+\$8}' /proc/stat"
  }
  # bench <label> <client> <server ip> <streams>: prints Gbit/s and the CPU.
  bench() {
    local label="$1" client="$2" ip="$3" streams="$4" before after json gbit
    before=$(cpu_jiffies "$client")
    json=$(x "$client" "iperf3 -c $ip -t 5 -P $streams -J 2>/dev/null")
    after=$(cpu_jiffies "$client")
    gbit=$(jq -r '.end.sum_received.bits_per_second // 0 | . / 1e9' <<<"$json")
    read -r b0 t0 <<<"$before"
    read -r b1 t1 <<<"$after"
    local cores
    cores=$(awk -v b="$((b1 - b0))" -v t="$((t1 - t0))" -v n="$(nproc)" 'BEGIN { if (t > 0) printf "%.2f", n * b / t; else print 0 }')
    local per
    per=$(awk -v c="$cores" -v g="$gbit" 'BEGIN { if (g > 0) printf "%.2f", c / g; else print "-" }')
    # The result on stderr (shown), the rate on stdout (for the checks).
    info "$(LC_ALL=C printf '%-26s %d stream(s): %6.2f Gbit/s, %5s CPU cores busy, %5s cores per Gbit/s' "$label" "$streams" "$gbit" "$cores" "$per")" >&2
    printf '%s' "$gbit"
  }

  client_vlan c-site1 20 10.20.0.1/24
  client_vlan c-site2 20 10.20.0.2/24
  client_vlan c-natc1 20 10.20.0.31/24
  x c-site1 "pkill iperf3; iperf3 -s -D"
  sleep 1
  expect_ping c-site2 10.20.0.1 "VLAN 20: site2's client reaches site1's client"
  expect_ping c-natc1 10.20.0.1 "VLAN 20: natc1's client reaches site1's client"
  gre1=$(bench "GRE with a key (servers)" c-site2 10.20.0.1 1 | tail -n1)
  bench "GRE with a key (servers)" c-site2 10.20.0.1 4 >/dev/null
  udp1=$(bench "GRE in UDP (through NAT)" c-natc1 10.20.0.1 1 | tail -n1)
  bench "GRE in UDP (through NAT)" c-natc1 10.20.0.1 4 >/dev/null
  x c-site1 "pkill iperf3" || true
  check "GRE with a key carries traffic ($gre1 Gbit/s)" awk -v g="$gre1" 'BEGIN { exit !(g > 0.05) }'
  check "GRE in UDP reaches half of it at least ($udp1 Gbit/s)" \
    awk -v u="$udp1" -v g="$gre1" 'BEGIN { exit !(u >= 0.5 * g) }'
  client_vlan_del c-natc1 20
fi
