# shellcheck shell=bash
# Several networks on one box: site1 and site2 are also in "lab" (VLANs
# 60-70, on the trunk of the default network: its tunnel joins br0) and in
# "iso" (VLAN 20 like the default network, on a trunk of its own). Each
# network has its own identity, tunnels (another tag, another VNI) and
# ports (chosen by the daemon); what the default network has stays its own;
# a restart brings every network back; removing them leaves the default
# network as it was.

# jqn <site> <network> <filter>: the status of a network of a site.
jqn() { x "$1" "wanderlan --net $2 status --json" | jq -r "$3"; }
# The command an invitation prints (`wanderlan --net <name> join ...`).
join_cmd() { awk '$1 == "wanderlan" && $4 == "join" {print; exit}'; }
# tun_up <site> <alias>: the tunnel with that alias is there and up.
tun_up() { x "$1" "t=\$(tun $2) && [ -n \"\$t\" ] && ip -o link show dev \"\$t\" | grep -qE '[<,]UP[,>]'"; }

client_vlan c-site1 20 10.20.0.1/24
client_vlan c-site2 20 10.20.0.2/24

begin "Networks: site1 starts network lab, site2 joins it on the same trunk"

before=$(x site1 "ip -o link show | grep -c ' vx'")
check "site1 adds network lab" x site1 "wanderlan networks add lab | grep -q 'Network lab added'"
check "the box lists both networks" \
  test "$(x site1 "wanderlan networks list" | awk '{print $1}' | tr '\n' ' ')" = "default lab "
check "site1 starts lab (wanderlan new)" \
  x site1 "wanderlan --net lab new --name site1 --address 10.100.1.10 --trunk trunk0 --vlans 60-70 --confirm | grep -q 'Network started: site \"site1\" at 10.100.1.10, trunk trunk0, VLANs 60-70'"
# The default network has 51821 (and 51822 for its tunnels).
check "lab took other ports than the default network" \
  x site1 "wanderlan --net lab export | grep -q '^control_port = 51831$'"
check "lab has an identity of its own" \
  test "$(jqn site1 lab .site_id)" != "$(jqs site1 .site_id)"
cmd=$(x site1 "wanderlan --net lab invite" | join_cmd)
check "the invitation names the network ($cmd)" grep -q '^ *wanderlan --net lab join 10.100.1.10:51831 ' <<<"$cmd"
out=$(x site2 "$cmd --trunk trunk0" 2>&1)
if grep -q 'Network lab added' <<<"$out" && grep -q 'is part of the network' <<<"$out"; then
  pass "site2 joined lab (the network was added on the way)"
else
  fail "unexpected join output: $out"
fi
wait_eq 60 "site1 sees site2 in lab" "site2" jqn site1 lab '[.peers[].name] | join(" ")'
wait_eq 90 "site2 confirmed its configuration of lab" "running true" \
  jqn site2 lab '"\(.state) \(.pending == null)"'
check "site2 took other ports in lab too" \
  x site2 "wanderlan --net lab export | grep -q '^control_port = 51831$'"
check "site1 takes site2 online in lab" x site1 "wanderlan --net lab online site2 --confirm >/dev/null"
tag=$(x site1 "wanderlan networks list" | awk '$1 == "lab" {print $3}')
wait_for 60 "site1: the tunnel of lab to site2 is up" tun_up site1 lab/site2
labtun=$(x site1 "tun lab/site2")
deftun=$(x site1 "tun site2")
check "its interface carries the tag of lab ($labtun)" grep -q "^v$tag" <<<"$labtun"
check "it is in br0, with the default network's tunnels" \
  test "$(x site1 "ip -j link show $labtun | jq -r '.[0].master'")" = br0
vni() { x site1 "ip -d -j link show $1 | jq -r '.[0].linkinfo.info_data.id'"; }
check "it has a VNI of its own" test "$(vni "$labtun")" != "$(vni "$deftun")"
check "the default network still has its tunnels" \
  test "$(x site1 "ip -o link show | grep -c ' vx'")" = "$before"

client_vlan c-site1 60 10.60.0.1/24
client_vlan c-site2 60 10.60.0.2/24
wait_for 30 "VLAN 60 (lab): c-site1 reaches c-site2" ping_once c-site1 10.60.0.2
check "VLAN 20 (default network) still works" ping_once c-site1 10.20.0.2

out=$(x site1 "wanderlan --net lab vlans 20,60-70 --confirm" 2>&1)
if grep -q 'another network of this box has it' <<<"$out" && grep -q 'VLANs 20' <<<"$out"; then
  pass "lab cannot take VLAN 20 of the default network on trunk0"
else
  fail "VLAN 20 for lab: unexpected output: $out"
fi

begin "Networks: iso on a trunk of its own, with the VLAN of the default network"

for s in site1 site2; do
  x "$s" "ip link add trunk1 type veth peer name trunk1p && ip link set trunk1 up && ip link set trunk1p up"
  x "$s" "wanderlan networks add iso >/dev/null"
done
x site1 "ip link add link trunk1p name trunk1p.20 type vlan id 20 && ip addr add 10.21.0.1/24 dev trunk1p.20 && ip link set trunk1p.20 up"
x site2 "ip link add link trunk1p name trunk1p.20 type vlan id 20 && ip addr add 10.21.0.2/24 dev trunk1p.20 && ip link set trunk1p.20 up"
printf '[network]\nrevision = 1\nvlans = "20"\n\n[site.site1]\naddress = "10.100.1.10"\n' |
  put site1 /tmp/iso.toml
printf 'self = "site1"\ntrunk = "trunk1"\n' | put site1 /tmp/iso-local.toml
check "site1 configures iso" \
  x site1 "wanderlan --net iso apply --network /tmp/iso.toml --local /tmp/iso-local.toml >/dev/null && wanderlan --net iso confirm >/dev/null"
check "iso took the next free ports" \
  x site1 "wanderlan --net iso export | grep -q '^control_port = 51841$'"
cmd=$(x site1 "wanderlan --net iso invite" | join_cmd)
check "site2 joins iso" x site2 "$cmd --trunk trunk1 >/dev/null"
wait_eq 90 "site2 confirmed its configuration of iso" "running true" \
  jqn site2 iso '"\(.state) \(.pending == null)"'
check "site1 takes site2 online in iso" x site1 "wanderlan --net iso online site2 --confirm >/dev/null"
wait_for 60 "site1: the tunnel of iso to site2 is up" tun_up site1 iso/site2
check "it is in the bridge of trunk1" \
  test "$(x site1 "ip -j link show \$(tun iso/site2) | jq -r '.[0].master'")" = wl-trunk1
wait_for 60 "VLAN 20 (iso): site1 reaches site2 through trunk1" \
  x site1 "ping -c 2 -i 0.2 -W 1 10.21.0.2"
check "VLAN 20 of the default network is another one" ping_once c-site1 10.20.0.2
check "the same site names in every network" \
  test "$(jqn site1 iso '[.sites[].name] | sort | join(" ")')" = "site1 site2"

begin "Networks: a restart brings every network back"

x site1 "wl-daemon stop; wl-daemon start"
wait_eq 90 "site1: lab is running again" "running" jqn site1 lab .state
wait_for 60 "site1: the tunnel of lab to site2 is up again" tun_up site1 lab/site2
wait_for 60 "VLAN 60 (lab): c-site1 reaches c-site2 again" ping_once c-site1 10.60.0.2
wait_for 60 "VLAN 20 (iso): site1 reaches site2 again" x site1 "ping -c 2 -i 0.2 -W 1 10.21.0.2"
wait_for 60 "VLAN 20 (default network) works again" ping_once c-site1 10.20.0.2

begin "Networks: what goes wrong in one network stays there"

# A first configuration that is not confirmed is rolled back: only what
# that network built goes.
before=$(x site1 "ip -o link show | grep -c ' vx'")
check "site1 adds network tmp" x site1 "wanderlan networks add tmp >/dev/null"
printf '[network]\nrevision = 1\nvlans = "80"\n\n[site.site1]\naddress = "10.100.1.10"\n' |
  put site1 /tmp/tmp.toml
printf 'self = "site1"\ntrunk = "trunk0"\n' | put site1 /tmp/tmp-local.toml
check "site1 configures tmp without confirming it" \
  x site1 "wanderlan --net tmp apply --network /tmp/tmp.toml --local /tmp/tmp-local.toml --timeout 5s >/dev/null"
wait_eq 30 "tmp is rolled back" "unconfigured" jqn site1 tmp .state
check "the default network kept its tunnels and br0" \
  x site1 "[ \"\$(ip -o link show | grep -c ' vx')\" = $before ] && ip link show br0 >/dev/null"
wait_for 30 "VLAN 20 (default network) still works" ping_once c-site1 10.20.0.2
check "site1 removes tmp" x site1 "wanderlan networks remove tmp --yes >/dev/null"

# The trunk port of iso is gone (an unplugged adapter): iso misses it, the
# default network goes on.
x site1 "ip link del trunk1"
wait_for 30 "iso misses its trunk port" \
  eval "jqn site1 iso '.missing_ports | join(\" \")' | grep -qx trunk1"
check "the default network still applies (no error)" \
  test "$(jqs site1 .state)" = running
check "a change of the default network still works" \
  x site1 "wanderlan vlans 10-51 --confirm >/dev/null && wanderlan vlans 10-50 --confirm >/dev/null"
wait_for 30 "VLAN 20 (default network) still works" ping_once c-site1 10.20.0.2

begin "Networks: removed again, the default network is as before"

for s in site1 site2; do
  check "$s removes lab and iso" \
    x "$s" "wanderlan networks remove lab --yes >/dev/null && wanderlan networks remove iso --yes >/dev/null"
  check "$s has the default network only" test "$(x "$s" "wanderlan networks list" | awk '{print $1}')" = default
  check "$s has no tunnels of other networks, no bridge of trunk1" \
    x "$s" "! ip -o link show | grep -qE ' v[a-wyz0-9][a-z2-7]{13}[@:]' && ! ip link show wl-trunk1 2>/dev/null"
  check "$s has no files of other networks" \
    x "$s" "[ ! -e /etc/wanderlan/networks/lab ] && [ ! -e /var/lib/wanderlan/networks/lab ]"
  x "$s" "ip link del trunk1 2>/dev/null || true"
done
check "the default network has its tunnels" \
  test "$(x site1 "ip -o link show | grep -c ' vx'")" = "$before"
wait_for 30 "VLAN 20 (default network) still works" ping_once c-site1 10.20.0.2
client_vlan_del c-site1 60
client_vlan_del c-site2 60
