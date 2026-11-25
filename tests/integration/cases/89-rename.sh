# shellcheck shell=bash
# Renaming sites: a client renames itself (also a restricted one), its
# manager (managed_by) renames it, other sites may not, a server renames
# itself. Every site follows: the network, local.toml (self, the trunk per
# tunnel), the tunnels, and the sync settles without loops.

names() { jqs "$1" '[.sites[].name] | sort | join(" ")'; }
revisions() { for s in site1 site2 site3 site4 site5; do jqs "$s" .revision; done | sort -u | wc -l; }

begin "Rename: a restricted client renames itself"

# site5 is a client of site1; it joined first through site2, which still
# manages it.
manager=$(x site1 "sed -n '/^\[site.site5\]/,/^\[/s/^managed_by = \"\(.*\)\"\$/\1/p' /etc/wanderlan/network.toml")
check "site5 is a client of site1, managed by another site ($manager)" \
  test -n "$manager" -a "$manager" != site5
other=site3
[ "$manager" = site3 ] && other=site4
mark1=$(daemon_log_lines site1)
mark5=$(daemon_log_lines site5)
out=$(x site5 "wanderlan rename site5 lab5 --confirm" 2>&1)
if grep -q 'Site "site5" is called "lab5"' <<<"$out"; then
  pass "wanderlan rename site5 lab5 on site5"
else
  fail "unexpected output of wanderlan rename: $out"
fi
check "site5's local.toml says self = lab5" x site5 "grep -qx 'self = \"lab5\"' /etc/wanderlan/local.toml"
for s in site1 site2 site3 site4; do
  wait_eq 30 "$s knows lab5 instead of site5" "lab5 site1 site2 site3 site4" names "$s"
done
wait_for 30 "site1's tunnel to it is gt-lab5" \
  x site1 "ip link show gt-lab5 >/dev/null && ! ip link show gt-site5 2>/dev/null"
check "site1's log says so" x site1 "wanderlan log | grep -q 'Site \"site5\" is now called \"lab5\"'"
wait_eq 30 "all sites have the same revision" 1 revisions
check "site1 refuses nothing" test "$(jqs site1 '[.peers[] | select(.refused != null)] | length')" = 0
sleep 5
fetches=$(daemon_log_since site5 "$mark5" | grep -c 'fetching it' || true)
refusals=$(daemon_log_since site1 "$mark1" | grep -cE 'Not taking over|Not using' || true)
if [ "$fetches" -le 2 ] && [ "$refusals" = 0 ]; then
  pass "the sync settles ($fetches fetches on lab5, no refusals on site1)"
else
  fail "the sync loops: $fetches fetches on lab5, $refusals refusals on site1"
  daemon_log_since site5 "$mark5" | tail -n 20
fi
expect_ping c-site5 10.20.0.1 "VLAN 20: lab5 reaches site1's client"

begin "Rename: its manager renames it, other sites may not"

out=$(x "$other" "wanderlan rename lab5 lab7 --confirm" 2>&1)
if grep -q "\"lab5\" itself and its manager \"$manager\" may" <<<"$out"; then
  pass "$other (not its manager) may not rename lab5"
else
  fail "unexpected output when $other renames lab5: $out"
fi
check "lab5 is still lab5" test "$(names site1)" = "lab5 site1 site2 site3 site4"
out=$(x "$manager" "wanderlan rename lab5 site5 --confirm" 2>&1)
if grep -q 'Site "lab5" is called "site5"' <<<"$out"; then
  pass "its manager $manager renames lab5 back to site5"
else
  fail "unexpected output when $manager renames lab5: $out"
fi
wait_for 30 "site5 follows: self = site5 in its local.toml" \
  x site5 "grep -qx 'self = \"site5\"' /etc/wanderlan/local.toml"
wait_eq 30 "site5 knows itself as site5" "site1 site5" names site5
wait_eq 30 "all sites have the same revision" 1 revisions
expect_ping c-site5 10.20.0.1 "VLAN 20: site5 reaches site1's client again"

begin "Rename: a server renames itself; the others and their trunks follow"

check "site1 gives the tunnel to site3 a trunk of its own" \
  x site1 "wanderlan trunk site3 trunk1 --confirm >/dev/null"
out=$(x site3 "wanderlan rename site3 hub3 --confirm" 2>&1)
if grep -q 'Site "site3" is called "hub3"' <<<"$out"; then
  pass "wanderlan rename site3 hub3 on site3"
else
  fail "unexpected output of wanderlan rename: $out"
fi
for s in site1 site2 site4; do
  wait_eq 30 "$s knows hub3 instead of site3" "hub3 site1 site2 site4 site5" names "$s"
done
# local.toml is written when the change is confirmed (automatically).
wait_for 30 "site1's trunk of the tunnel follows the name" \
  x site1 "grep -A1 '^\[tunnels.hub3\]' /etc/wanderlan/local.toml | grep -q 'trunk = \"trunk1\"'"
wait_for 30 "site1's tunnel to it is gt-hub3, on trunk1's bridge" \
  x site1 "ip -j link show gt-hub3 | jq -e '.[0].master == \"wl-trunk1\"' >/dev/null && ! ip link show gt-site3 2>/dev/null"
wait_eq 30 "all sites have the same revision" 1 revisions
check "site3 renames itself back" x site3 "wanderlan rename hub3 site3 --confirm >/dev/null"
wait_eq 30 "site1 knows site3 again" "site1 site2 site3 site4 site5" names site1
wait_for 30 "site1's trunk of the tunnel follows back" \
  x site1 "grep -q '^\[tunnels.site3\]' /etc/wanderlan/local.toml"
check "site1 gives the tunnel to site3 the default trunk again" \
  x site1 "wanderlan trunk site3 default --confirm >/dev/null"
wait_for 20 "the bridge wl-trunk1 is gone" x site1 "! ip link show wl-trunk1 2>/dev/null"
wait_eq 30 "all sites have the same revision" 1 revisions
expect_ping c-site1 10.20.0.11 "VLAN 20: site1 reaches site3 again"
