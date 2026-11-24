# shellcheck shell=bash
# Path MTU measurement, MSS clamping and GRE fragmentation.
# The underlay of site5 has MTU 1400 (see compose.yml).

begin "MTU: measured path MTU, MSS clamping and a large TCP transfer"

wait_for 90 "site5 measured a path MTU of 1400 to site2" \
  x site5 "grep -q 'Path MTU to \"site2\" is 1400 bytes (TCP MSS 1318)' /var/log/wanderland.log"
wait_for 90 "site2 measured a path MTU of 1400 to site5" \
  x site2 "grep -q 'Path MTU to \"site5\" is 1400 bytes' /var/log/wanderland.log"
wait_for 90 "site1 measured a path MTU of 1400 to site5 (direct tunnel)" \
  x site1 "grep -q 'Path MTU to \"site5\" is 1400 bytes' /var/log/wanderland.log"
wait_for 90 "site1 measured the full 1500-byte underlay MTU to site2" \
  x site1 "grep -q 'Path MTU to \"site2\" is 1500 bytes (TCP MSS 1418)' /var/log/wanderland.log"
wait_for 30 "site5 clamps the MSS to 1318 on gt-site2" \
  x site5 "nft list table bridge wanderlan | grep -q 'oifname \"gt-site2\" .*maxseg size set 1318'"
check "site5 clamps IPv6 TCP to 1298" \
  x site5 "nft list table bridge wanderlan | grep -q 'meta protocol ip6 oifname \"gt-site2\" .*maxseg size set 1298'"
check "site2 clamps SYNs from site5 to 1318 as well" \
  x site2 "nft list table bridge wanderlan | grep -q 'iifname \"gt-site5\" .*maxseg size set 1318'"

# Large TCP transfer between the untagged client at site5 (VLAN 20) and site1's client.
x c-site5 "head -c 100M /dev/urandom > /tmp/data && sha256sum < /tmp/data | cut -d' ' -f1 > /tmp/data.sha"
x c-site1 "rm -f /tmp/recv; (timeout 120 nc -l 5001 > /tmp/recv) >/dev/null 2>&1 &
          (timeout 60 tcpdump -nli trunk0.20 -c 1 'tcp[tcpflags] & tcp-syn != 0 and tcp[tcpflags] & tcp-ack == 0 and port 5001' > /tmp/syn.txt 2>/dev/null) &
          sleep 1"
if x c-site5 "timeout 120 nc -N 10.20.0.1 5001 < /tmp/data"; then
  pass "100 MB TCP transfer from site5 to site1 completed"
else
  fail "100 MB TCP transfer from site5 to site1 failed"
fi
sleep 1
sent=$(x c-site5 "cat /tmp/data.sha")
got=$(x c-site1 "sha256sum < /tmp/recv | cut -d' ' -f1")
if [ -n "$sent" ] && [ "$sent" = "$got" ]; then
  pass "the transferred data is intact (sha256 $got)"
else
  fail "the transferred data differs (sent $sent, received $got)"
fi
check "the SYN arrives at site1 with MSS 1318" x c-site1 "grep -q 'mss 1318' /tmp/syn.txt"

begin "Fragmentation: inner packets larger than the path still arrive"

frag_before=$(x site1 "nstat -az IpFragCreates | awk '/IpFragCreates/ {print \$2}'")
reasm_before=$(x site2 "nstat -az IpReasmOKs | awk '/IpReasmOKs/ {print \$2}'")
expect_ping c-site1 "-M do -s 1472 10.20.0.2" "1500-byte packet with DF set crosses the tunnel"
x c-site2 "rm -f /tmp/udp; (timeout 10 nc -u -l 5002 > /tmp/udp) >/dev/null 2>&1 & sleep 1"
x c-site1 "head -c 4000 /dev/zero | tr '\\0' 'x' | timeout 3 nc -u -w 1 10.20.0.2 5002" || true
sleep 1
size=$(x c-site2 "wc -c < /tmp/udp")
if [ "$size" = 4000 ]; then
  pass "a 4000-byte UDP datagram arrives complete"
else
  fail "the 4000-byte UDP datagram arrived with $size bytes"
fi
frag_after=$(x site1 "nstat -az IpFragCreates | awk '/IpFragCreates/ {print \$2}'")
reasm_after=$(x site2 "nstat -az IpReasmOKs | awk '/IpReasmOKs/ {print \$2}'")
if [ "${frag_after:-0}" -gt "${frag_before:-0}" ] && [ "${reasm_after:-0}" -gt "${reasm_before:-0}" ]; then
  pass "GRE packets were fragmented by site1 and reassembled by site2 ($((frag_after - frag_before)) fragments)"
else
  fail "no GRE fragmentation seen (FragCreates $frag_before -> $frag_after, ReasmOKs $reasm_before -> $reasm_after)"
fi
