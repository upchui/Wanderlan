#!/usr/bin/env bash
# Acceptance test: two hosts install wanderlan from the APT repository with
# the README commands. "heim" starts a new network, "buero" joins it with an
# invitation (`wanderlan invite` / `wanderlan join`); a client at each site
# pings the other in VLAN 20.
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

# heim starts a new network, with the files the setup wizard writes.
printf '[network]\nvlans = "10-50"\n\n[[site]]\nname = "heim"\naddress = "10.8.0.1"\n' |
  "${COMPOSE[@]}" exec -T heim sh -c 'cat > /etc/wanderlan/network.toml'
printf 'self = "heim"\ntrunk = "trunk0"\n' |
  "${COMPOSE[@]}" exec -T heim sh -c 'cat > /etc/wanderlan/local.toml'
x heim "wanderlan validate /etc/wanderlan/network.toml --local /etc/wanderlan/local.toml" || exit 1
x heim "wl-daemon start"
for _ in $(seq 1 30); do
  x heim "wanderlan status --json | jq -e '.state == \"running\"'" >/dev/null 2>&1 && break
  sleep 1
done

# buero joins with nothing but the address of heim and an invitation code.
code=$(x heim "wanderlan invite" | awk '$1 == "wanderlan" && $2 == "join" {print $4; exit}')
echo "invitation: $code"
x buero "wl-daemon start"
x buero "wanderlan join 10.8.0.1 $code" || exit 1
# A new site starts offline; the inviting site takes it online.
x heim "wanderlan online buero --confirm" || exit 1

ok=1
for _ in $(seq 1 30); do
  x heim "ip link show vx-buero up | grep -q vx-buero" && break
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
  echo "OK: buero joined heim's network with an invitation; VLAN 20 works."
else
  echo "FAILED: no ping in VLAN 20 between heim and buero" >&2
  x heim "tail -n 40 /var/log/wanderland.log"
fi
exit "$ok"
