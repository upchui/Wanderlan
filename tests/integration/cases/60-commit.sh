# shellcheck shell=bash
# Commit with rollback through the API (CLI).

begin "Commit with rollback: a change without confirmation is rolled back"

check "wanderlan status reports the site" \
  x site1 "[ \"\$(wanderlan status --json | jq -r .site)\" = site1 ]"
rev_before=$(jqs site1 .revision)

# Same network with VLANs reduced to 10-40.
sed 's/^vlans = .*/vlans = "10-40"/' "$RUN_DIR/network-map.toml" | put site1 /tmp/network-40.toml
out=$(x site1 "wanderlan apply --network /tmp/network-40.toml --timeout 8s" 2>&1)
if grep -q 'Run `wanderlan confirm` within 8 seconds' <<<"$out" && grep -q '^+vlans = "10-40"' <<<"$out"; then
  pass "apply shows the diff and the confirmation deadline"
else
  fail "unexpected apply output: $out"
fi
check "the change is in effect while pending (VLANs 10-40 on trunk0)" \
  test "$(vlan_summary site1 trunk0)" = "10-40"
check "status shows the pending change" x site1 "wanderlan status --json | jq -e '.pending.remaining_secs > 0'"
check "a second change is refused while one is pending" \
  x site1 "! wanderlan apply --network /tmp/network-40.toml 2>/dev/null"
wait_eq 20 "the change is rolled back after the deadline (VLANs 10-50 again)" "10-50" \
  vlan_summary site1 trunk0
check "the rollback is in the event log" x site1 "wanderlan log | grep -q 'Rolled back to the previous configuration: the change was not confirmed in time'"
check "the revision is unchanged" test "$(jqs site1 .revision)" = "$rev_before"
check "network.toml on disk still has VLANs 10-50" x site1 "grep -q '^vlans = \"10-50\"' /etc/wanderlan/network.toml"

begin "Commit with rollback: hand-edited files that are not confirmed are restored"

x site4 "cp /etc/wanderlan/network.toml /tmp/network.orig && sed -i 's/^vlans = .*/vlans = \"10-30\"/' /etc/wanderlan/network.toml"
check "wanderlan apply applies the edited files" x site4 "wanderlan apply --timeout 6s"
check "the edited VLANs are in effect" test "$(vlan_summary site4 trunk0)" = "10-30"
wait_eq 20 "the edit is rolled back" "10-50" vlan_summary site4 trunk0
check "the confirmed network.toml is restored" x site4 "cmp -s /etc/wanderlan/network.toml /tmp/network.orig"
check "the rejected file is kept as network.toml.rejected" x site4 "grep -q '^vlans = \"10-30\"' /etc/wanderlan/network.toml.rejected"

begin "Commit with rollback: invalid configurations and manual rollback"

sed 's/^vlans = .*/vlans = "10-5000"/' "$RUN_DIR/network-map.toml" | put site1 /tmp/network-bad.toml
out=$(x site1 "wanderlan apply --network /tmp/network-bad.toml" 2>&1)
if grep -q 'network.toml:3: error: network.vlans: VLAN ID 5000 is outside 1-4094' <<<"$out"; then
  pass "an invalid configuration is refused with file and line"
else
  fail "unexpected output for an invalid configuration: $out"
fi
x site1 "wanderlan apply --network /tmp/network-40.toml --timeout 60s" >/dev/null
check "manual rollback works" x site1 "wanderlan rollback"
check "VLANs 10-50 again after the manual rollback" test "$(vlan_summary site1 trunk0)" = "10-50"
