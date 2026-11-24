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

# check_not <description> <command...>
check_not() {
  local what="$1"
  shift
  if "$@" >/dev/null 2>&1; then
    fail "$what"
  else
    pass "$what"
  fi
}

# ---- configuration -------------------------------------------------------

# Revision of the generated meshes. Every new configuration gets a higher
# revision so the sync between the daemons never prefers an older one.
MESH_REVISION=${MESH_REVISION:-100}

# mesh_toml <vlans> <site[=vlans]>... prints a mesh.toml; `site=20,30`
# limits a site to some of the VLANs.
mesh_toml() {
  local vlans="$1"
  shift
  MESH_REVISION=$((MESH_REVISION + 1))
  printf '[mesh]\nrevision = %s\nvlans = "%s"\nmtu_probe_interval = "%s"\ncommit_timeout = "%s"\n' \
    "${REVISION:-$MESH_REVISION}" "$vlans" "${PROBE_INTERVAL:-15s}" "${COMMIT_TIMEOUT:-60s}"
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
  if [ -n "${MESH_EXTRA:-}" ]; then
    printf '\n%s\n' "$MESH_EXTRA"
  fi
}

# renew <mesh-file>: gives a previously generated mesh a new, higher revision
# (reusing an old file would let the sync bring back a newer configuration).
renew() {
  MESH_REVISION=$((MESH_REVISION + 1))
  sed -i "s/^revision = .*/revision = $MESH_REVISION/" "$1"
}

# configure <site> <mesh-file>: writes mesh.toml and local.toml of a site.
configure() {
  local site="$1" mesh="$2"
  put "$site" /etc/wanderlan/mesh.toml <"$mesh"
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

# expect_vlans <svc> <dev> <summary>
expect_vlans() {
  local got
  got=$(vlan_summary "$1" "$2")
  if [ "$got" = "$3" ]; then
    pass "$1: $2 carries VLANs $3"
  else
    fail "$1: $2 carries VLANs '$got', expected '$3'"
  fi
}

# status_json <site>: the daemon status as JSON.
status_json() { x "$1" "wanderlan status --json"; }

# jqs <site> <jq filter>: a value from the daemon status.
jqs() { status_json "$1" | jq -r "$2"; }
