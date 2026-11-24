#!/usr/bin/env bash
# Acceptance test: two hosts install wanderlan from the APT repository with
# the README commands and run the example configuration (its two hubs
# "heim" and "buero"); a client at each site pings the other in VLAN 20.
#   REPO_URL  repository (default https://upchui.github.io/Wanderlan)
#   KEEP=1    keep the containers
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
COMPOSE=(docker compose -f "$here/compose.yml")
x() { local s="$1"; shift; "${COMPOSE[@]}" exec -T "$s" bash -c "$*"; }

docker build -f "$root/docker/Dockerfile" --target repoe2e -t wanderlan-repoe2e \
  --build-arg REPO_URL="${REPO_URL:-https://upchui.github.io/Wanderlan}" \
  --build-arg CACHE_BUST="$(date +%s)" ${BUILD_ARGS:-} "$root" || exit 1

cleanup() { [ "${KEEP:-0}" = 1 ] || "${COMPOSE[@]}" down -v -t 1 >/dev/null 2>&1; }
trap cleanup EXIT
"${COMPOSE[@]}" down -v -t 1 >/dev/null 2>&1
"${COMPOSE[@]}" up -d --quiet-pull || exit 1
for s in heim buero c-heim c-buero; do
  for _ in $(seq 1 30); do x "$s" "test -f /run/node-ready" && break; sleep 1; done
done

# The example mesh.toml, reduced to its two hubs (everything from the first
# spoke on is cut), and a local.toml per host.
example="$(x heim "cat /usr/share/doc/wanderlan/examples/mesh.toml 2>/dev/null || wanderlan init | sed -n '/^# ---- mesh.toml/,/^# ---- local.toml/p'")"
mesh="$(awk '/^\[\[site\]\]/ { block = $0; next } block != "" { if ($0 ~ /^name = "haus2"/) exit; print block; block = "" } { print }' <<<"$example")"
for s in heim buero; do
  printf '%s\n' "$mesh" | "${COMPOSE[@]}" exec -T "$s" sh -c 'cat > /etc/wanderlan/mesh.toml'
  printf 'self = "%s"\ntrunk = "trunk0"\n' "$s" | "${COMPOSE[@]}" exec -T "$s" sh -c 'cat > /etc/wanderlan/local.toml'
  x "$s" "wanderlan validate /etc/wanderlan/mesh.toml --local /etc/wanderlan/local.toml" || exit 1
  x "$s" "wl-daemon start"
done

ok=1
for _ in $(seq 1 30); do
  x heim "ip link show gt-buero up | grep -q gt-buero" && break
  sleep 1
done
x c-heim "ip link add link trunk0 name trunk0.20 type vlan id 20 && ip addr add 10.20.0.1/24 dev trunk0.20 && ip link set trunk0.20 up"
x c-buero "ip link add link trunk0 name trunk0.20 type vlan id 20 && ip addr add 10.20.0.2/24 dev trunk0.20 && ip link set trunk0.20 up"
for _ in $(seq 1 20); do
  x c-heim "ping -c 2 -i 0.2 -W 1 10.20.0.2" >/dev/null 2>&1 && { ok=0; break; }
  sleep 1
done
x heim "wanderlan status"
if [ "$ok" = 0 ]; then
  echo "OK: the example configuration connects heim and buero; VLAN 20 works."
else
  echo "FAILED: no ping in VLAN 20 between heim and buero" >&2
  x heim "tail -n 40 /var/log/wanderland.log"
fi
exit "$ok"
