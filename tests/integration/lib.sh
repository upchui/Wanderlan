# Helpers for the integration tests. Sourced by run.sh.
# shellcheck shell=bash

COMPOSE=(docker compose -p "${PROJECT:-wanderlan-it}" -f "$IT_DIR/compose.yml")
RUN_DIR="$IT_DIR/.run"
FAILURES=0
CASE=""

# Site name -> transport address and VLAN host number.
declare -A ADDR=([site1]=10.100.1.10 [site2]=10.100.2.10 [site3]=10.100.11.10 [site4]=10.100.12.10 [site5]=10.100.13.10)
declare -A HOSTNUM=([site1]=1 [site2]=2 [site3]=11 [site4]=12 [site5]=13)

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
info() { printf '    %s\n' "$*"; }

begin() {
  CASE="$1"
  log "$CASE"
}

pass() { printf '    \033[32mok\033[0m %s\n' "$*"; }

fail() {
  printf '    \033[31mFAIL\033[0m %s\n' "$*"
  FAILURES=$((FAILURES + 1))
  echo "$CASE: $*" >>"$RUN_DIR/failures"
  # GitHub Actions annotations are visible without access to the job logs.
  if [ "${GITHUB_ACTIONS:-}" = true ]; then
    printf '::error title=%s::%s\n' "${CASE//,/;}" "$(printf '%s' "$*" | tr '\n' ' ' | cut -c1-900)"
  fi
}

# Runs a shell command in a container.
x() {
  local svc="$1"
  shift
  "${COMPOSE[@]}" exec -T "$svc" bash -c "$*"
}

# Same, but without output.
xq() { x "$@" >/dev/null 2>&1; }

# entry_of <site> <name>: the entry of site <name> in the network.toml of <site>.
entry_of() { x "$1" "site-entry $2"; }

# tunnel_of <box> <site>: the tunnel interface of <box> to <site> (named
# by the identity of the site, see the node helper `tun`).
tunnel_of() { x "$1" "tun $2"; }

# Writes stdin to a file in a container.
put() {
  local svc="$1" path="$2"
  "${COMPOSE[@]}" exec -T "$svc" sh -c "mkdir -p \"\$(dirname '$path')\" && cat > '$path'"
}

# wait_for <seconds> <description> <command...>: retries until success.
wait_for() {
  local timeout="$1" what="$2"
  shift 2
  local start=$SECONDS
  until "$@" >/dev/null 2>&1; do
    if ((SECONDS - start >= timeout)); then
      fail "timed out after ${timeout}s waiting for: $what"
      return 1
    fi
    sleep 1
  done
  pass "$what ($((SECONDS - start))s)"
}

# wait_eq <seconds> <description> <expected> <command...>: retries until the
# output of the command (run anew each time) equals the expected value.
wait_eq() {
  local timeout="$1" what="$2" want="$3"
  shift 3
  local start=$SECONDS got=""
  while true; do
    got=$("$@" 2>/dev/null)
    [ "$got" = "$want" ] && break
    if ((SECONDS - start >= timeout)); then
      fail "timed out after ${timeout}s waiting for: $what (got '$got', expected '$want')"
      return 1
    fi
    sleep 1
  done
  pass "$what ($((SECONDS - start))s)"
}

# check <description> <command...>
check() {
  local what="$1"
  shift
  if "$@" >/dev/null 2>&1; then
    pass "$what"
  else
    fail "$what"
  fi
}

# ---- configuration -------------------------------------------------------

# Revision of the generated networks. Every new configuration gets a higher
# revision so the sync between the daemons never prefers an older one.
NETWORK_REVISION=${NETWORK_REVISION:-100}

# Raises NETWORK_REVISION above the revision of every running site: the
# daemons raise it themselves too (a site claims its identity in the network
# at its first start), so a counter of our own would fall behind.
next_revision() {
  local s r
  for s in "${!ADDR[@]}"; do
    r=$(jqs "$s" .revision 2>/dev/null) || continue
    if [[ "$r" =~ ^[0-9]+$ ]] && [ "$r" -gt "$NETWORK_REVISION" ]; then
      NETWORK_REVISION=$r
    fi
  done
  NETWORK_REVISION=$((NETWORK_REVISION + 1))
}

# network_toml <vlans> <site[=vlans]>... prints a network.toml; `site=20,30`
# limits a site to some of the VLANs.
network_toml() {
  local vlans="$1"
  shift
  next_revision
  printf '[network]\nrevision = %s\nvlans = "%s"\nmtu_probe_interval = "%s"\ncommit_timeout = "%s"\n' \
    "${REVISION:-$NETWORK_REVISION}" "$vlans" "${PROBE_INTERVAL:-15s}" "${COMMIT_TIMEOUT:-60s}"
  local spec name site_vlans
  for spec in "$@"; do
    name="${spec%%=*}"
    site_vlans=""
    [[ "$spec" == *=* ]] && site_vlans="${spec#*=}"
    printf '\n[[site]]\nname = "%s"\naddress = "%s"\n' "$name" "${ADDR[$name]}"
    if [ -n "$site_vlans" ]; then
      printf 'vlans = "%s"\n' "$site_vlans"
    fi
  done
  if [ -n "${NETWORK_EXTRA:-}" ]; then
    printf '\n%s\n' "$NETWORK_EXTRA"
  fi
}

# keyed <site>...: waits until each site has confirmed the identity of all
# its peers (after a new network every site claims its identity; until the
# claims have spread, the tunnels get their keys one by one).
keyed() {
  local s
  for s in "$@"; do
    wait_eq 90 "$s confirmed the identities of all its peers" true \
      jqs "$s" '[.peers[] | .identity_state == "confirmed" and .port_state == "up"] | (length > 0 and all)'
  done
}

# renew <network-file>: gives a previously generated network a new, higher revision
# (reusing an old file would let the sync bring back a newer configuration).
renew() {
  next_revision
  sed -i "s/^revision = .*/revision = $NETWORK_REVISION/" "$1"
}

# configure <site> <network-file>: writes network.toml and local.toml of a site.
configure() {
  local site="$1" network="$2"
  put "$site" /etc/wanderlan/network.toml <"$network"
  printf 'self = "%s"\ntrunk = "trunk0"\n' "$site" | put "$site" /etc/wanderlan/local.toml
}

# ---- clients -------------------------------------------------------------

# client_vlan <client> <vid> <cidr>: adds a VLAN subinterface with an address.
client_vlan() {
  local c="$1" vid="$2" cidr="$3"
  x "$c" "ip link show trunk0.$vid >/dev/null 2>&1 || ip link add link trunk0 name trunk0.$vid type vlan id $vid;
          ip addr flush dev trunk0.$vid; ip addr add $cidr dev trunk0.$vid; ip link set trunk0.$vid up"
}

client_vlan_del() { x "$1" "ip link del trunk0.$2 2>/dev/null || true"; }

# client_native <client> <cidr>: sets an untagged address on the trunk.
client_native() { x "$1" "ip addr flush dev trunk0; ip addr add $2 dev trunk0"; }
client_native_del() { x "$1" "ip addr flush dev trunk0"; }

ping_once() { x "$1" "ping -c 2 -i 0.2 -W 1 $2"; }

expect_ping() {
  local c="$1" ip="$2" what="${3:-$1 reaches $2}"
  wait_for "${PING_TIMEOUT:-20}" "$what" ping_once "$c" "$ip"
}

expect_no_ping() {
  local c="$1" ip="$2" what="${3:-$1 does not reach $2}"
  if x "$c" "ping -c 3 -i 0.3 -W 1 $ip" >/dev/null 2>&1; then
    fail "$what"
  else
    pass "$what"
  fi
}

# ---- daemon --------------------------------------------------------------

daemon_log_lines() { x "$1" "wc -l < /var/log/wanderland.log 2>/dev/null || echo 0"; }

# Prints the daemon log of a site from line <start>.
daemon_log_since() { x "$1" "tail -n +$(($2 + 1)) /var/log/wanderland.log 2>/dev/null"; }

# Lines of the daemon log that describe system changes.
CHANGE_RE='Creating|Adding VLANs|Removing VLANs|Deleting|Attaching|Bringing|Isolating|Removing isolation|Setting|clsact|On [a-z0-9.]+: '

dump_logs() {
  local svc
  for svc in "$@"; do
    echo "----- $svc: wanderland.log -----"
    x "$svc" "tail -n 80 /var/log/wanderland.log 2>/dev/null" || true
  done
}

# vlan_summary <svc> <dev>: VLANs of a bridge port in compact form, e.g.
# "10-19,20:pu,21-50" (p = PVID, u = egress untagged).
vlan_summary() {
  x "$1" "bridge -j vlan show dev $2" | jq -r '
    [.[0].vlans[]? | {v: .vlan, e: (.vlanEnd // .vlan),
      f: ([(.flags // [])[] | if . == "PVID" then "p" elif . == "Egress Untagged" then "u" else . end] | sort | join(""))}]
    | reduce .[] as $x ([];
        if (length > 0) and (.[-1].f == "") and ($x.f == "") and (.[-1].e + 1 == $x.v)
        then .[-1].e = $x.e else . + [$x] end)
    | map((if .v == .e then "\(.v)" else "\(.v)-\(.e)" end) + (if .f != "" then ":" + .f else "" end))
    | join(",")'
}

# expect_vlans <svc> <dev|to:site> <summary>; `to:site2` is the tunnel to
# site2.
expect_vlans() {
  local got dev="$2" shown="$2"
  if [[ "$dev" == to:* ]]; then
    dev=$(tunnel_of "$1" "${dev#to:}")
    shown="the tunnel to ${2#to:}"
  fi
  got=$(vlan_summary "$1" "$dev")
  if [ "$got" = "$3" ]; then
    pass "$1: $shown carries VLANs $3"
  else
    fail "$1: $shown carries VLANs '$got', expected '$3'"
  fi
}

# status_json <site>: the daemon status as JSON.
status_json() { x "$1" "wanderlan status --json"; }

# jqs <site> <jq filter>: a value from the daemon status.
jqs() { status_json "$1" | jq -r "$2"; }
