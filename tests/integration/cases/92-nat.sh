# shellcheck shell=bash
# Clients behind NAT: natc1 and natc2 sit behind one NAT (masquerading with
# random ports) and share its address. They open every flow; site1 answers
# where their packets come from.

nat_code() { awk '$1 == "wanderlan" && $2 == "join" {print $4; exit}'; }
peer_of() { # peer_of <site> <peer> <jq expression on the peer>
  jqs "$1" ".peers[] | select(.name == \"$2\") | $3"
}

begin "NAT: two boxes behind one NAT join and get their tunnels"

check "site1 cannot reach the boxes behind the NAT" x site1 "! ping -c 1 -W 1 192.168.77.11 >/dev/null"
for c in natc1 natc2; do
  x "$c" "wl-daemon start"
  wait_for 30 "$c: the daemon runs (unconfigured)" \
    x "$c" "wanderlan status --json | jq -e '.state == \"unconfigured\"'"
done
for c in natc1 natc2; do
  code=$(x site1 "wanderlan invite" | nat_code)
  out=$(x "$c" "wanderlan join 10.100.1.10 $code" 2>&1)
  if grep -q "Done: \"$c\" is part of the network" <<<"$out"; then
    pass "$c joined through the NAT"
  else
    fail "unexpected join output of $c: $out"
  fi
done
check "both have the address of the NAT at site1" \
  test "$(jqs site1 '[.sites[] | select(.name == "natc1" or .name == "natc2") | .address] | join(" ")')" = "10.100.20.10 10.100.20.10"
for c in natc1 natc2; do
  check "site1 takes $c online" x site1 "wanderlan online $c --confirm"
done
for c in natc1 natc2; do
  wait_eq 60 "site1: the tunnel to $c runs through NAT, $c proved its identity" "confirmed nat up" \
    peer_of site1 "$c" '"\(.identity_state) \(.tunnel) \(.port_state)"'
  wait_eq 30 "$c: its tunnel to site1 is up" "confirmed nat up" \
    peer_of "$c" site1 '"\(.identity_state) \(.tunnel) \(.port_state)"'
  check "site1 knows where the tunnel packets of $c come from (the NAT)" \
    grep -q '^10\.100\.20\.10:[0-9]*$' <<<"$(peer_of site1 "$c" .endpoint)"
done
for c in natc1 natc2; do
  wait_eq 30 "site1 sent $c a test frame, it did not come through the NAT: nat" "nat: not reachable" \
    eval "peer_of site1 $c .tunnel_reason | cut -d' ' -f1-3"
done
check "the NAT gave the two boxes different ports" \
  test "$(peer_of site1 natc1 .endpoint)" != "$(peer_of site1 natc2 .endpoint)"
check "site1 learned them from the keepalives (table ip wanderlan, a set per tunnel)" \
  x site1 "nft list set ip wanderlan ep-\$(tun natc1) | grep -q '10.100.20.10 . '"
check "a client tunnel is VXLAN from and to port 51822" \
  x natc1 "ip -d link show \$(tun site1) | grep -q 'vxlan id [0-9]* .*srcport 51822 51823 dstport 51822'"
check "site1 sends to the NAT, where natc1's packets come from" \
  x site1 "bridge fdb show dev \$(tun natc1) | grep -q '^00:00:00:00:00:00 dst 10.100.20.10 '"

client_vlan c-site1 20 10.20.0.1/24
client_vlan c-natc1 20 10.20.0.31/24
client_vlan c-natc2 20 10.20.0.32/24
expect_ping c-natc1 10.20.0.1 "VLAN 20: the first box behind the NAT reaches site1's client"
expect_ping c-natc2 10.20.0.1 "VLAN 20: the second one too"
expect_ping c-natc1 10.20.0.32 "VLAN 20: the two reach each other through site1"
check "site1 never opened a flow towards the boxes behind the NAT" \
  x nat "nft list chain ip watch pre | grep -q 'counter packets 0 bytes 0'"

begin "NAT: a new mapping (the NAT forgot the flows) is followed"

before=$(peer_of site1 natc1 .endpoint)
index=$(x site1 "cat /sys/class/net/\$(tun natc1)/ifindex")
x nat "conntrack -F >/dev/null 2>&1"
wait_for 60 "site1 follows the new port of natc1 (was $before)" \
  eval "[ \"\$(peer_of site1 natc1 .endpoint)\" != '$before' ]"
wait_for 60 "VLAN 20: natc1 reaches site1's client again" ping_once c-natc1 10.20.0.1
check "the tunnel was changed in place (same interface)" \
  test "$(x site1 "cat /sys/class/net/\$(tun natc1)/ifindex")" = "$index"
wait_for 60 "VLAN 20: natc2 reaches site1's client again" ping_once c-natc2 10.20.0.1

begin "NAT: a restart of the server is followed"

x site1 "wl-daemon stop; wl-daemon start"
for c in natc1 natc2; do
  wait_eq 90 "site1: the tunnel to $c is up again" "confirmed nat up" \
    peer_of site1 "$c" '"\(.identity_state) \(.tunnel) \(.port_state)"'
done
wait_for 60 "VLAN 20: natc1 reaches site1's client" ping_once c-natc1 10.20.0.1
wait_for 60 "VLAN 20: natc2 reaches natc1 through site1" ping_once c-natc2 10.20.0.31

begin "Names: two clients with the same name, told apart by their identities"

fp2=$(jqs natc2 .site_id | cut -c1-8)
tunnel2=$(tunnel_of site1 natc2)
check "site1 calls natc2 natc1 too (it manages both)" x site1 "wanderlan rename natc2 natc1 --confirm"
wait_eq 60 "site1 has two sites called natc1" 2 \
  jqs site1 '[.sites[] | select(.name | startswith("natc1#"))] | length'
check "the tunnel to the renamed one stays, named by its identity" \
  test "$(x site1 "ip -j link show $tunnel2 | jq -r '.[0].ifalias'")" = "wanderlan natc1#$fp2"
for c in natc1 natc2; do
  wait_eq 60 "$c: its tunnel to site1 is still up" "up" peer_of "$c" site1 .port_state
done
wait_for 60 "VLAN 20: natc2 (now also natc1) reaches natc1 through site1" ping_once c-natc2 10.20.0.31
check "a name two sites have is refused, with both" \
  x site1 "! wanderlan online natc1 2>/tmp/ambiguous.err && grep -q 'names 2 sites' /tmp/ambiguous.err"
check "the name with the start of its identity names one" \
  x site1 "wanderlan online natc1#$fp2 | grep -q 'already online'"
check "site1 calls it natc2 again" x site1 "wanderlan rename natc1#$fp2 natc2 --confirm"
wait_eq 60 "site1 has natc1 and natc2 again" "natc1 natc2" \
  jqs site1 '[.sites[] | select(.name | startswith("natc")) | .name] | sort | join(" ")'
# The next change is made elsewhere: it must build on this one (two changes
# with the same revision at two servers keep only one of them).
want=$(jqs site1 .revision)
for s in site2 site3 site4; do
  wait_eq 60 "$s has network revision $want" "$want" jqs "$s" .revision
done
client_vlan_del c-natc1 20
client_vlan_del c-natc2 20
