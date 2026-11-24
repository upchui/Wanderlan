#!/usr/bin/env bash
# Runs the integration tests against the testnode image.
#   TESTNODE_IMAGE  image to use (default wanderlan-testnode)
#   KEEP=1          keep the containers running afterwards
#   CASES="10 20"   run only the cases with these number prefixes
set -uo pipefail
IT_DIR="$(cd "$(dirname "$0")" && pwd)"
export IT_DIR
# shellcheck source=lib.sh
source "$IT_DIR/lib.sh"

rm -rf "$RUN_DIR"
mkdir -p "$RUN_DIR"

cleanup() {
  if [ "${KEEP:-0}" != 1 ]; then
    "${COMPOSE[@]}" down -v --remove-orphans -t 2 >/dev/null 2>&1
  fi
}
trap cleanup EXIT

log "Starting the test topology"
"${COMPOSE[@]}" down -v --remove-orphans -t 2 >/dev/null 2>&1
if ! "${COMPOSE[@]}" up -d --quiet-pull; then
  echo "cannot start the test topology" >&2
  exit 1
fi
SERVICES=$("${COMPOSE[@]}" config --services)
for svc in $SERVICES; do
  wait_for 30 "$svc ready" x "$svc" "test -f /run/node-ready" || {
    "${COMPOSE[@]}" logs "$svc"
    exit 1
  }
done

for file in "$IT_DIR"/cases/*.sh; do
  num=$(basename "$file" | cut -d- -f1)
  if [ -n "${CASES:-}" ] && ! grep -qw "$num" <<<"$CASES"; then
    continue
  fi
  # shellcheck source=/dev/null
  source "$file"
done

echo
if ((FAILURES > 0)); then
  printf '\033[31m%d check(s) failed:\033[0m\n' "$FAILURES"
  cat "$RUN_DIR/failures"
  dump_logs hub1 hub2 spoke1 spoke2 spoke3
  exit 1
fi
printf '\033[32mAll integration tests passed.\033[0m\n'
