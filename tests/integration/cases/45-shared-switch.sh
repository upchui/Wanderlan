# shellcheck shell=bash
# The switch check, after case 40 (site5 maps local 130 to VLAN 30 and
# untagged to VLAN 20). A "switch" joins VLAN 30 of the trunk networks of
# site2 and site5, like one switch network both trunks hang in: site5 finds
# its trunk shared with a peer for VLAN 30 (not for 20), warns, and its DHCP
# requests from VLAN 130 ask for broadcast replies. Without the switch both
# go again.

begin "Switch check: both sites in one switch network"

PROJECT_NAME="${PROJECT:-wanderlan-it}"
SWITCH="$PROJECT_NAME-switch"
# Labelled as part of the project: `compose down --remove-orphans` removes
# it should this case not get to the end.
docker rm -f "$SWITCH" >/dev/null 2>&1
docker run -d --rm --name "$SWITCH" --cap-add NET_ADMIN \
  --label "com.docker.compose.project=$PROJECT_NAME" \
  --label com.docker.compose.service=switch --label com.docker.compose.oneoff=False \
  --sysctl net.ipv6.conf.all.disable_ipv6=1 --network "${PROJECT_NAME}_tr-site2" \
  --entrypoint sleep "${TESTNODE_IMAGE:-wanderlan-testnode}" infinity >/dev/null
docker network connect "${PROJECT_NAME}_tr-site5" "$SWITCH"
# Only VLAN 30 passes, tagged (no loop: site5 drops VLAN 30 as it is).
docker exec "$SWITCH" bash -c '
  set -e
  port() { ip -o -4 addr show | awk -v p="$1" "index(\$4, p) == 1 { print \$2; exit }"; }
  a=$(port 10.200.2.); b=$(port 10.200.13.)
  ip addr flush dev "$a"; ip addr flush dev "$b"
  ip link add sw0 type bridge vlan_filtering 1 vlan_default_pvid 0
  for p in "$a" "$b"; do ip link set "$p" master sw0; bridge vlan add dev "$p" vid 30; done
  ip link set sw0 up'

# "<trunk> <vid> <local>" of every trunk site5 finds shared.
shared_switch_is() {
  [ "$(jqs site5 '[.shared_switch[] | "\(.trunk) \(.vid) \(.local)"] | join(",")')" = "$1" ]
}
wait_for 60 "site5 finds trunk0 in one switch network with a peer for VLAN 30 (130 there), not for 20" \
  shared_switch_is "trunk0 30 130"
check "site5 warns in its status" \
  x site5 "wanderlan status | grep -q 'WARNING: VLAN 30 (VLAN 130 here) also arrives unmapped at trunk trunk0: this site and a peer are in one switch network'"
check "site5: DHCP requests from VLAN 130 ask for broadcast replies" \
  x site5 "nft list chain bridge wanderlan forward | grep -q 'iifname \"trunk0\" vlan id 30 meta protocol ip udp dport 67 @th,144,16 set 0x8000 udp checksum set 0'"

# A BOOTP request from VLAN 130 at site5 (its own address, so the switch
# learns no address of c-site5 in two places), seen leaving site2's trunk.
x c-site5 "ip link add link trunk0.130 name dh0 type macvlan mode bridge; ip addr add 10.99.0.2/24 dev dh0; ip link set dh0 up
           ip neigh replace 10.99.0.1 lladdr ff:ff:ff:ff:ff:ff dev dh0 nud permanent
           { printf '\x01\x01\x06\x00\x12\x34\x56\x78'; head -c 228 /dev/zero; printf '\x63\x82\x53\x63\xff'; head -c 59 /dev/zero; } > /tmp/bootp"
x site2 "rm -f /tmp/dhcp.txt; (timeout 20 tcpdump -nvvli trunk0 -c 1 'vlan 30 and udp dst port 67' > /tmp/dhcp.txt 2>/dev/null) & sleep 1"
x c-site5 "nc -u -w1 -p 68 -s 10.99.0.2 10.99.0.1 67 < /tmp/bootp" >/dev/null 2>&1
wait_for 20 "site2 gets the request with the broadcast flag (and without a UDP checksum)" \
  x site2 "grep -q 'Flags \[Broadcast\]' /tmp/dhcp.txt && grep -q 'no cksum' /tmp/dhcp.txt"
x c-site5 "ip link del dh0 2>/dev/null; rm -f /tmp/bootp"

docker rm -f "$SWITCH" >/dev/null 2>&1
wait_for 150 "without the switch, site5 is alone again (three checks without the marker)" \
  shared_switch_is ""
check "and its DHCP requests go as they are" \
  x site5 "! nft list chain bridge wanderlan forward | grep -q 'udp dport 67'"
