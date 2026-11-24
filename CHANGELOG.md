# Changelog

All notable changes to wanderlan are documented in this file.

## [Unreleased]

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
