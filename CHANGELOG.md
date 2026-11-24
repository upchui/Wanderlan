# Changelog

All notable changes to wanderlan are documented in this file.

## [0.1.0] - 2026-11-24

First release. Everything below this line went into it.

### Fixes

- Loop guard: the FDB watch socket has a 4 MiB buffer and NETLINK_NO_ENOBUFS,
  so a broadcast storm cannot overflow it, and the watch is set up again if
  it ever ends.
- Integration test of the loop guard: the loop is started with ARP
  broadcasts; before, known unicast never entered the second path and the
  test only passed by chance.

### Milestone 7 – TUI and CLI

- Interactive terminal UI (`wanderlan` without arguments, ratatui +
  crossterm, keyboard first, `--mouse` optional, works in 80×24 and over
  SSH): overview with live peer status, sites with forms and in-field
  validation, mesh and local settings, VLAN range and mapping table with
  immediate conflict marking, MAC table (filter, sort), live log (filter by
  level and peer), setup wizard on first start.
- Changes are collected in a draft; `a` shows the diff against the running
  configuration and applies it; a countdown banner shows pending changes
  (also those from the sync), `c` confirms, `r` rolls back; `?` help.
- Without a daemon the TUI explains why and reconnects every 2 s.
- CLI: `status [--json]`, `validate`, `apply`, `confirm`, `rollback`, `init`,
  `export`, `unblock`, `log [-f]`, `probe`.
- Tests: state machine unit tests, snapshot tests of every screen at 80×24,
  and an integration test that drives the TUI in tmux.

### Milestone 6 – Daemon API, commit with rollback, loop guard, sync

- Unix socket API `/run/wanderlan/wanderlan.sock` (JSON lines, mode 0660,
  group `wanderlan`): status, event subscription, get/validate/apply
  configuration, apply edited files, confirm, rollback, interfaces, MAC
  table, unblock, MTU probe.
- Commit with rollback: every change must be confirmed within
  `commit_timeout`, otherwise the previous state is restored; edited files
  that are not confirmed are restored and kept as `*.rejected`, also across
  a restart. Confirming a local change raises the mesh revision.
- Loop guard: MAC addresses flapping between GRETAP ports shut down the
  offending port (spoke ports first) until `wanderlan unblock`.
- Automatic sync of `mesh.toml`: probes announce revision and fingerprint,
  sites fetch a newer mesh from a direct peer over TCP, apply it with the
  commit timer and confirm it automatically once a peer is reachable;
  rejected versions are not fetched again, removed peers may fetch for 5
  minutes, changes based on an outdated revision are refused.
- Event log (ring buffer, also to journald), throughput per port.
- Netlink dumps on one socket are serialised (concurrent dumps fail with
  EBUSY); a restarted daemon keeps the existing MSS rules.
- Integration tests: rollback after the deadline, rollback of edited files,
  invalid configurations with line numbers, an external layer-2 loop cut by
  the loop guard, sync of a change to all sites, refusal of outdated
  changes, a new site fetching the mesh from its hub.

### Milestone 5 – MTU and MSS

- Control probes between the daemons (UDP on `<address>:control_port`, only
  from direct peers): reachability and RTT every 5 s, path MTU by binary
  search with the DF bit set (`IP_PMTUDISC_PROBE`) at start, every
  `mtu_probe_interval` and whenever a peer becomes reachable again.
- MSS = path MTU − 82, clamped with nftables in table `bridge wanderlan`
  for SYNs entering and leaving each tunnel port (IPv6: −20); ports without
  a measurement use the smallest measured value; `mss` in the configuration
  replaces the measurement. The table is replaced atomically and recognised
  by rule comments.
- Works around swapped `<`/`>` operators in the `nftables` crate 0.6.3.
- GRETAP fragmentation stays on as safety net.
- Integration tests: spoke3 with a 1400-byte underlay measures 1400 and
  clamps to 1318, a 100 MB TCP transfer arrives intact with MSS 1318 in the
  SYN, 1500-byte inner packets with DF and 4000-byte UDP datagrams arrive
  (fragment counters prove the GRE fragmentation).

### Milestone 4 – VLAN mappings

- Native mapping: untagged traffic on the trunk is mapped to a shared VLAN
  (`pvid untagged` on the trunk).
- VLAN translation with tc `clsact`, `flower` and `action vlan modify` via
  netlink (local → shared on ingress, shared → local on egress). Frames that
  arrive tagged with a translation target are dropped (`gact drop`) so the
  local VLAN and the translated VLAN never merge; chained translations keep
  their targets on the trunk.
- wanderlan recognises its tc rules by an action cookie (`wl:i130>30`) and
  never touches foreign filters; the clsact qdisc is removed only if no
  other filters use it.
- Integration tests: untagged client reaches VLAN 20 at other sites, local
  VLAN 130 reaches VLAN 30, tagged VLAN 30 at the translating site does not
  leak, conflicting mappings are rejected, reload is idempotent.

### Milestone 3 – Hubs and spokes

- Five-site integration topology: two hubs, two spokes at the first hub, one
  spoke at the second hub, each site with its own underlay network behind a
  router (standing in for the existing transport network/VPN).
- Tests: VLAN 20 between spokes at the same hub and at different hubs, split
  horizon (`isolated` on hub-to-hub ports, own broadcasts never return, also
  with three fully meshed hubs), topology check via `wanderlan validate`,
  hub container restart, daemon crash with adoption of existing interfaces.
- After a topology change the daemon flushes the learned MAC addresses of
  the bridge, immediately and again after 10 s and 30 s, so that entries
  learned while the sites switch one after another do not blackhole traffic.

### Milestone 2 – Two hubs

- System access behind the `NetOps` trait: an rtnetlink backend for links,
  bridge options, GRETAP, bridge port isolation and bridge VLANs, and an
  in-memory fake kernel for tests without root.
- Desired state derived from the configuration and a reconciler that only
  changes what differs; applying the same configuration twice changes
  nothing. On stop the daemon removes every interface it created (marked
  with the alias `wanderlan`) and leaves foreign interfaces alone.
- Daemon `wanderland`: periodic reconciliation, reload on SIGHUP, cleanup on
  SIGTERM, every system change logged at info level, journald under systemd.
- Docker stages `build` (static musl binaries and .deb for amd64/arm64),
  `artifacts`, `testnode` (the .deb installed with apt) and `modloader`.
- .deb with systemd unit (CAP_NET_ADMIN only, hardened), conffiles in
  `/etc/wanderlan`, group `wanderlan`, examples in `/usr/share/doc`.
- Integration tests with Docker Compose: two hubs, ping in VLAN 20, VLANs
  outside the range and untagged traffic are not bridged, idempotent
  reload, cleanup on stop. CI runs them with the installed package.

### Milestone 1 – Skeleton and configuration

- Cargo workspace with the library `wanderlan-core`, the daemon `wanderland`
  and the TUI/CLI `wanderlan`; clippy denies `unwrap`/`expect` outside tests.
- Docker build with the stages `base` and `check` (fmt, clippy, tests);
  `scripts/check.sh` runs it locally, `ci.yml` on every push and pull request.
- Loading and validation of `mesh.toml` and `local.toml` with line numbers:
  unique names and addresses, hub/spoke references, topology check, VLAN
  ranges and mappings, durations, MSS, unknown keys.
- `wanderlan validate <file> [--local <file>]` and `wanderlan init`.
- Scope: wanderlan builds no VPN. An existing transport network (usually a
  VPN set up beforehand) connects the sites; wanderlan only handles layer 2
  and VLANs on top of it.
