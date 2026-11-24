# shellcheck shell=bash
# Commit with rollback through the API (CLI).

begin "Commit with rollback: a change without confirmation is rolled back"

check "wanderlan status reports the site" \
  x hub1 "[ \"\$(wanderlan status --json | jq -r .site)\" = hub1 ]"
rev_before=$(jqs hub1 .revision)

# Same mesh with VLANs reduced to 10-40.
sed 's/^vlans = .*/vlans = "10-40"/' "$RUN_DIR/mesh-map.toml" | put hub1 /tmp/mesh-40.toml
out=$(x hub1 "wanderlan apply --mesh /tmp/mesh-40.toml --timeout 8s" 2>&1)
if grep -q 'Run `wanderlan confirm` within 8 seconds' <<<"$out" && grep -q '^+vlans = "10-40"' <<<"$out"; then
  pass "apply shows the diff and the confirmation deadline"
else
  fail "unexpected apply output: $out"
fi
check "the change is in effect while pending (VLANs 10-40 on trunk0)" \
  test "$(vlan_summary hub1 trunk0)" = "10-40"
check "status shows the pending change" x hub1 "wanderlan status --json | jq -e '.pending.remaining_secs > 0'"
check "a second change is refused while one is pending" \
  x hub1 "! wanderlan apply --mesh /tmp/mesh-40.toml 2>/dev/null"
wait_eq 20 "the change is rolled back after the deadline (VLANs 10-50 again)" "10-50" \
  vlan_summary hub1 trunk0
check "the rollback is in the event log" x hub1 "wanderlan log | grep -q 'Rolled back to the previous configuration: the change was not confirmed in time'"
check "the revision is unchanged" test "$(jqs hub1 .revision)" = "$rev_before"
check "mesh.toml on disk still has VLANs 10-50" x hub1 "grep -q '^vlans = \"10-50\"' /etc/wanderlan/mesh.toml"

begin "Commit with rollback: hand-edited files that are not confirmed are restored"

x spoke2 "cp /etc/wanderlan/mesh.toml /tmp/mesh.orig && sed -i 's/^vlans = .*/vlans = \"10-30\"/' /etc/wanderlan/mesh.toml"
check "wanderlan apply applies the edited files" x spoke2 "wanderlan apply --timeout 6s"
check "the edited VLANs are in effect" test "$(vlan_summary spoke2 trunk0)" = "10-30"
wait_eq 20 "the edit is rolled back" "10-50" vlan_summary spoke2 trunk0
check "the confirmed mesh.toml is restored" x spoke2 "cmp -s /etc/wanderlan/mesh.toml /tmp/mesh.orig"
check "the rejected file is kept as mesh.toml.rejected" x spoke2 "grep -q '^vlans = \"10-30\"' /etc/wanderlan/mesh.toml.rejected"

begin "Commit with rollback: invalid configurations and manual rollback"

sed 's/^vlans = .*/vlans = "10-5000"/' "$RUN_DIR/mesh-map.toml" | put hub1 /tmp/mesh-bad.toml
out=$(x hub1 "wanderlan apply --mesh /tmp/mesh-bad.toml" 2>&1)
if grep -q 'mesh.toml:3: error: mesh.vlans: VLAN ID 5000 is outside 1-4094' <<<"$out"; then
  pass "an invalid configuration is refused with file and line"
else
  fail "unexpected output for an invalid configuration: $out"
fi
x hub1 "wanderlan apply --mesh /tmp/mesh-40.toml --timeout 60s" >/dev/null
check "manual rollback works" x hub1 "wanderlan rollback"
check "VLANs 10-50 again after the manual rollback" test "$(vlan_summary hub1 trunk0)" = "10-50"
