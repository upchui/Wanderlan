# Changelog

All notable changes to wanderlan are documented in this file.

## [1.0.0] - 2026-11-25

Simpler: the sites behave like one switch, and a new site joins with a
single command. Update all sites; 0.1 and 1.0 do not sync with each other.

### Several trunk ports

- `trunk = ["ens19", "ens22"]` in `local.toml`: several interfaces form the
  trunk, bridged like the ports of a switch (the first is the main port).
  The same for a tunnel with a trunk of its own (`[tunnels.<site>] trunk =
  [...]`), so a server says for each site exactly which ports carry it and
  a client which ports are its trunk. A port belongs to one segment.
- Settings (7) and Sites `l` list the interfaces of the host to choose the
  trunk ports from, several at once (`Space`, `m` for the main port, a
  name that is not there yet in the last row). Ports of a bridge of
  someone else are grey with the reason. A port of another segment (the
  default trunk, the trunk of another tunnel) moves here when chosen; that
  segment keeps the rest, and a tunnel left without ports uses the default
  trunk. The window, like `wanderlan trunk <site> ens20,ens21`, says what
  changes elsewhere.
- An interface the tunnels run over can be a trunk port again, marked with
  a warning ("tunnels to marcel, site2"). Which interface that is, the
  daemon asks the kernel the way the tunnel packets are routed, with the
  policy rules: behind a VPN such as Tailscale or WireGuard the default
  route's interface is no longer taken for it. `wanderlan trunk` warns
  instead of refusing, and with `--confirm` leaves such a change
  unconfirmed, so the rollback can still save the site.
- A further port that is missing (unplugged) is shown and left out instead
  of stopping the site; a loop between two trunk ports shuts the further
  one down (BLOCKED, `wanderlan unblock <port>`), never the main port.
- Fix: an interface that is no longer a trunk port is taken off the bridge;
  before, it stayed bridged after the trunk was changed.

### The system stays clean

- Whenever the service ends (stop, update, crash, kill), systemd runs
  `wanderland --cleanup`: bridges, tunnels, tc rules and the nftables
  table go, also what a crashed daemon left, and the kernel's `gre0`,
  `gretap0` and `erspan0` when nothing else needs `ip_gre`. A start
  builds everything anew; before, a crash left the network running.
- The cleanup also runs before every start, for when the one after the
  end could not run: after `systemctl kill`, some kernels kill a process
  started into the service's cgroup at once.
- A step of the teardown that fails no longer stops the others: the
  bridge and the tunnels go anyway, the error lists what failed
  (`Cleanup failed: ...` in the journal).
- An internal error removes everything before the daemon exits.
- The kernel modules are loaded when the service starts, not at every
  boot (no `gre0` while wanderlan does not run).
- `apt remove` runs the cleanup also when the daemon was not running.
- An update restarts the daemon (as before; now tested).

### Faster and more robust

- Forwarding: every bridged frame of the host passes one nftables rule;
  only TCP SYNs go on to the MSS rules (before: four rules per tunnel for
  every frame). All VLAN translations of a direction share one tc
  priority: one lookup per frame instead of one per translation. Both
  are rebuilt once after the update.
- MSS clamping from the start (from the MTU of the interface towards the
  peer until the path is measured), kept when a re-measurement fails.
- New VLANs reach the tunnel ports only after the split horizon covers
  them; teardown removes the tunnels before the split horizon; without
  nftables only the MSS clamping is missing.
- Loop guard: blocks of further trunk ports survive configuration
  changes and automatic confirmations; it works after a failed reconcile
  too; threshold 20 moves per 10 s (roaming devices and duplicated MAC
  addresses no longer cut a tunnel); bounded memory in a broadcast storm.
- The daemon restarts itself (through systemd) when its netlink
  connection dies; the sync server and the API have limits (connections,
  time, size); timeouts and revisions from the network are range-checked;
  a rolled back client change is not fetched again at once; the status
  is computed again only after a change; files are synced to disk.
- TUI: a request while the daemon is away no longer hangs ("Applying"
  forever); the tunnel search can be changed when it matches nothing; a
  delete question removes exactly the site or mapping it named; the trunk
  port window reloads after a reconnect and refuses typed ports of a
  foreign bridge; several small fixes (VLAN window navigation, scrolling,
  the second status line, Space in the key bar).
- Configuration: the same ports in another order for two segments, and an
  empty trunk list for a tunnel, are errors; a warning for an own VLAN
  that is also the local side of a mapping; tunnel trunks are checked
  against the real tunnels; `wanderlan vlan native`; `wanderlan status`
  shows all trunk ports; `wanderlan trunk` drops duplicate ports.

### Live traffic per VLAN

- The peer details (Overview, `Enter`) list the VLANs of the tunnel with
  their traffic now, the busiest first; `Enter` opens one: every port of
  this site with that VLAN, in and out now, bytes, packets, the MAC
  addresses behind each port, a graph, the mapping at this site, the
  tunnels that carry it and where it is passed on. `m` opens its MAC table
  (filter word `vlan:20`). Left/Right go to the next peer (was Up/Down).
- `wanderlan vlan <vid> [--json]` prints the same.
- The bridges count per VLAN and port (`vlan_stats_per_port`). The kernel
  takes that only from a new bridge: after the update the bridge is built
  anew once, a short interruption. A kernel without the counters keeps its
  bridge and shows no traffic per VLAN.

### Version numbers

- wanderlan is 1.0.0, and every commit on `main` raises the last number:
  1.0.1, 1.0.2, ... (`scripts/version.sh` counts the commits since the
  version in `Cargo.toml` was set; CI, packages and `--version` show it).
  The APT repository keeps the newest 20 and every tagged release.
- Packages are named exactly like the version (`1.0.2`, no `-1`), and
  `wanderlan --version`, the daemon's log and status, the CI job names and
  summaries name the commit too: `wanderlan 1.0.2 (c463aa1)`.

### network.toml

- wanderlan is no mesh, so nothing is called that any more: the shared
  configuration is `/etc/wanderlan/network.toml` with a `[network]` table,
  `wanderlan apply --network`, "network revision" everywhere.
- Upgrading moves an existing `mesh.toml` (also the confirmed copy and
  `*.rejected`) to `network.toml` at the first start and renames its `[mesh]`
  table; files and sites with the old table name are still read. Sites with
  an older version cannot read `[network]`: update all sites, until then
  changes do not reach the old ones (their tunnels keep running).

### Like a switch: no hubs and spokes

- Every site has a tunnel to every other site; all tunnel ports are isolated
  (split horizon), so the network stays loop-free. All sites must reach each
  other in the transport network.
- VLANs per site (`[[site]] vlans = "20,30"`), like the allowed VLANs of a
  switch port; a tunnel carries the VLANs both of its sites take part in.
  `network.vlans` is the default. Mappings (native → VLAN, VLAN → VLAN) as
  before, checked against the VLANs of their site.
- `role` and `hub` in existing configurations are ignored with a warning.
- The loop guard also detects MAC addresses moving between the trunk and a
  tunnel (a loop through the LANs of two sites) and always shuts down the
  tunnel, never the trunk.

### Invitations and joining

- `wanderlan invite` on any site creates a one-time code and prints the
  command for the new site: `wanderlan join <address> <code>`. Codes do not
  expire; they are used up or deleted (`wanderlan invite list`, `wanderlan
  invite delete`). They survive restarts (`/var/lib/wanderlan/invites.json`,
  mode 0600).
- `wanderlan join <address> <code>`: the new site takes its name from the
  host name and picks the trunk itself (the interface that does not lead to
  the inviting site); the inviting site adds it with the source address of
  the request and answers with the configuration. Both confirm
  automatically once they reach each other; a site that never comes up is
  removed again after `commit_timeout` and its code becomes usable again.
  Wrong codes are rate-limited.
- The setup wizard offers "Start a new network" (name, trunk and address
  suggested; afterwards it creates the first invitation) and "Join an
  existing network". The overview shows the open invitation; `i` creates one
  (Sites and Setup views).

### Reset

- `wanderlan reset` (and `R` in the Setup view) removes the bridge, the
  tunnels, all VLAN and MSS rules, the configuration and the invitations; the
  site is unconfigured again, like after the installation.

### VLANs like a switch trunk

- Without `vlans` (or empty, or `"all"`) every VLAN and the untagged traffic
  pass. This is the new default of the setup wizard.
- `native` in a VLAN list is the untagged traffic (carried as VLAN 1 between
  the sites, like the default native VLAN of switches), e.g.
  `vlans = "native,10-50"`. A site's own native mapping takes precedence.

### Invitations with rules, online/offline, managed sites

- An invitation carries rules for the new site: name, VLANs, VLAN for its
  untagged traffic, translations (`wanderlan invite --name … --vlans …
  --native … --map L:S`; TUI: Sites, `i`). Open invitations can be listed
  and deleted in the TUI (Sites, `I`).
- New sites start offline (`[[site]] online = false`): their tunnels stay
  down until a user takes them online (`wanderlan online <site>`, TUI: `o`).
  The state is part of the shared configuration and survives restarts.
- The inviting site manages the new site (`managed_by`): only it may change
  the new site's VLANs, mappings and online state, unless it allowed the site
  to manage itself (`--self-managed`).

### Robust operation

- Reset under systemd: the unit protects `/var/lib` (`ProtectSystem=strict`),
  so the reset failed with "Read-only file system"; it now empties
  `/var/lib/wanderlan` instead of removing it. Every step of the reset runs
  even if an earlier one fails, and the daemon checks the end state and
  reports what is left. As root and outside containers the reset also
  unloads the kernel module `ip_gre` if no other GRE tunnel uses it, so the
  kernel's devices `gre0`, `gretap0` and `erspan0` disappear.
- Starting a new network after a reset was refused ("peer … has a newer network
  configuration") because announcements of the old network were kept; only
  reachable peers of the new configuration count now.
- Only one daemon runs: a second `wanderland` (e.g. typed instead of
  `wanderlan`) stops without touching anything; it used to take over the API
  socket and tear down the network when it was stopped. Started from a shell
  without `--foreground`, `wanderland` only explains what it is. A deleted
  API socket is created again within seconds.
- `wanderlan join` returns once the inviting site has confirmed the new
  site too, so `wanderlan online <site>` works right after it; `apply`,
  `online`, `offline` and `manage` wait for a pending join or synchronised
  change instead of failing.
- The daemon exits for a restart by systemd if one of its services stops;
  panics are logged. The unit restarts it always (also after a kill) and
  never gives up.
- TUI: when the daemon is not reachable it shows the state of the service
  and its last log lines; after a reset (also a failed one) everything is
  reloaded; interface lists are always current and never offer the kernel's
  fallback devices (`gre0`, …).
- Tests: `scripts/systemd-test.sh` runs the package's hardened unit under
  systemd through the steps users take (also in CI).

### Who may change what

- A site that joined with an invitation and was not released is restricted:
  it cannot change any VLANs, mappings or online state, neither its own nor
  those of other sites, nor the VLANs of the network (before it could change
  the network's VLANs and those of the first site); only those of sites it
  invited itself. Released (`self_managed`), it has the same rights as the
  site that invited it. Timeouts, MTU check, MSS, port and invitations stay
  open to every site.
- Visible and adjustable: the Sites view has a column MANAGED BY and the site
  form a field "Changes" (only the manager / the manager and the site /
  any site); Settings shows who manages this site and which sites it
  manages; a restricted site shows it in the overview and the VLAN view, and
  the edit keys explain why they refuse. CLI: `wanderlan manage <site>
  [--by S | --anyone] [--self-managed yes|no]`.
- After "Start a new network" the setup opens the invitation form (rules and
  who may change them) instead of creating an invitation without rules.
- Sites added by hand are managed by the site that added them (field
  "Changes").
- A site managed by itself ("only alex may change alex") is not
  restricted: it changes the network's VLANs and its mappings as before
  (it was shown read-only and its edits were refused).

### Servers and clients

- A site that joins with an invitation is a client of the site that invited
  it (`[[site]] uplink = "alex"`): its only tunnel leads there, so it only
  has to reach that site. Sites without uplink are servers, connected with
  each other as before (split horizon). Clients have no tunnel to each other;
  every tunnel carries the VLANs both ends take part in, so two clients meet
  only in a VLAN they and their server share.
- Configurations without `uplink`: a site with a manager (`managed_by`)
  becomes a client of it. Update all sites: older 0.2 builds refuse the new
  key.
- Visible everywhere: column ROLE (`server`, `client of alex`) in Sites and
  in the overview, the own role in the overview header and in Settings
  (with its clients), `via alex` for sites without a tunnel from here, also
  in `wanderlan status`. Change it in the site form ("Connected to") or with
  `wanderlan role <site> server|client <site>`.
- A client that joins again through another site becomes a client of that
  site.
- Clients never see the clients of other servers: a client's configuration
  (TUI, status, `network.toml`) contains only itself, its server and its own
  clients; servers keep the whole network. Each peer gets its view in the
  sync; a client's changes are merged by its server (only its own entry,
  its own clients and the network settings, checked against its rights).
  A client may only change itself and its own clients.
- VLAN mappings (table and form) show whether a site is a server or a client:
  `marcel (client)`.
- Sync: a higher revision with the same content (e.g. a site taken offline
  and online again quickly) is adopted, so the revisions agree everywhere;
  a local change is no longer refused as outdated because a peer only has a
  higher revision number with the same content.
- The log names why a tunnel is down: blocked by the loop guard, this site
  offline, or the peer offline (before it always said loop guard).

### Trunk per tunnel

- A tunnel can have a trunk of its own (`local.toml`: `[tunnels.<site>]
  trunk = "ens20"`): it becomes a segment of its own at this site, with a
  bridge `wl-<trunk>` that carries the same VLANs and mappings as the
  default trunk. Only that network reaches the site at the other end through
  this site. The default trunk stays in `br0`, so existing installations do
  not change.
- TUI: Settings lists the trunk of every tunnel ("Trunk per tunnel"),
  Enter changes it; the overview shows the traffic of every trunk, the peer
  details the trunk of the tunnel. CLI: `wanderlan trunk <site>
  <interface|default>`. The interface that leads to the other site is
  refused.
- The loop guard watches every bridge of wanderlan; the MAC table, the
  flushes after topology changes and the cleanup cover all of them.
- Renaming a site in the TUI also renames it where it manages other sites
  (`managed_by`) and in `[tunnels.<site>]`.

### Own VLANs

- A site can have own VLANs (`[[site]] own_vlans = "50"`): on its trunk and
  towards its own clients, never towards its server. The site itself may
  always set them, also when its server manages everything else (a
  restricted client). TUI: VLANs, `o`, and the site form; CLI:
  `wanderlan own-vlans <site> <vlans|none>`. The Sites view shows them as
  `13 +50`.

### Documentation

- README rewritten around what wanderlan is today: concepts (servers and
  clients, VLANs like a switch trunk, mappings, who knows and who may
  change what), a quick start that follows the wizard, everyday tasks, a
  complete configuration reference, how it works, and the project
  decisions. The TUI help and the open questions follow suit.

### VLANs per tunnel

- A tunnel can carry less than the VLANs both of its sites take part in:
  `[[tunnel]] between = ["dc", "lab"] vlans = "30-40"` (or `"none"`). So
  `dc – hq` can carry 10-20 and `dc – lab` 30-40. With a trunk of its own
  for a tunnel, the same VLAN IDs on another interface reach another site.
- VLANs (`3`), `t`: every tunnel with what it carries and its limit
  (`Enter` limits, `d` lifts); `wanderlan tunnel-vlans <site> <site>
  <vlans|all|none>`; the peer details mark a limited tunnel.
- Whoever may change one of the two sites may limit their tunnel; a client
  sees and changes only its own tunnels.
- Passing on over servers: a server passes a VLAN on between two other
  servers when it is the only way between them for that VLAN (e.g. VLAN 30
  only on `hq – dc` and `dc – lab`: `hq` and `lab` meet through `dc`). Per
  VLAN, tunnels in the same block (every pair with its own tunnel, or a
  ring) keep the split horizon, tunnels of different blocks pass on: no
  loops. A ring without every direct tunnel is not passed on; `validate`
  and the TUI warn. Such a site keeps the split horizon with nftables
  (`wl split` rules, read back on every reconcile, set before `isolated` is
  dropped); all others keep the kernel's `isolated` flag as before.

### Faster, and sync problems in plain sight

- A server that cannot take over a change of its client says so: `refused`
  in its overview (CONFIG column), the reason in the peer details and in
  `wanderlan status`. The client learns the reason with its next fetch,
  takes the server's configuration back after 15 s and shows "Your change
  was not taken over by …" in its overview and status. Before, the client
  kept its change silently and never got in sync again.
- A refused configuration is not fetched again for 10 minutes, then it gets
  another chance; before, it was skipped until the next restart.
- A newer revision with the same content is taken over without fetching,
  applying or a countdown (clients saw "applied (0 system changes)" for
  every change elsewhere in the network).
- The status reuses the forwarding table for 5 s instead of reading it on
  every request; the MSS rules are read from `nft` at most every 2 minutes
  (written ones are remembered), so a firewall reload is still repaired.
  The expected version of each client is computed once per change instead
  of every second.
- Sites (2), the network list of the overview and `wanderlan status` show
  each server with its clients indented below (`└ shop`); Up/Down follow
  that order.
- Log: a site that joined again through another site and did not come up in
  time "stays a client of" its old server; the message no longer claims it
  was removed.

### A better terminal UI

- The bottom bar always shows `? help` and `q quit` and drops the least
  important keys when it is narrow; missing keys are in it.
- Messages that do not fit get a second line; `M` shows the last 20. The
  status line of every view names problems of this site (an error, blocked
  ports, refused changes, a change not taken over). The draft says how many
  changes it holds; new and changed mappings and tunnel limits are marked.
- Below 80×24 the TUI says the terminal is too small instead of cutting.
- Every list keeps its scroll position, shows a scrollbar when it overflows
  and moves with PgUp/PgDn/Home/End. The overview shares its space between
  the neighbours and the sites, with one cursor through both; it shows all
  sites now. Sites lost the boxes that repeated Settings (7). Forms wrap
  their hints and scroll; wide terminals show VLANs and trunk per tunnel.
- `c` and `r` work while a window is open; `r` asks, deleting an invitation
  asks, Esc in a form with input asks; Enter means no where a wrong yes
  loses something.
- `/` searches in Sites, the VLAN mappings, the tunnels and the log. The log
  shows local time (from `TZ` or `/etc/localtime`; `t` switches to UTC).
- Site details (Sites, Enter, or Enter on a site in the overview): role,
  VLANs, every tunnel with what it carries, mappings, rights; Up/Down goes
  to the next site, as in the peer details. Edit is `e`.
- With `--mouse` a click selects a row (a second click opens it), the
  wheel scrolls the list under the pointer instead of moving the focus of a
  form, and the keys of the bottom bar can be clicked.

### More to see

- Settings view (7): every setting with an explanation.
- Peer details (Overview, Enter): traffic graph, round-trip history,
  min/avg/max round trip, jitter, probe loss, estimated latency, byte,
  packet, error and drop counters. The overview shows the total traffic of
  the tunnels and the trunk.

## [0.1.0] - 2026-11-24

First release. Everything below this line went into it.

### Continuous delivery

- Every commit on `main` whose CI run is green is published to the APT
  repository automatically (`publish.yml`), as version
  `<version>+<CI run>.g<commit>-1`; the binaries report the same version.
  The newest 20 commit builds per architecture stay in the repository,
  tagged releases forever; a published file never changes.
- After every publication `verify-apt.yml` installs from the repository on
  Debian 13, Debian 12 and Ubuntu 24.04 (README commands, exactly this
  version, downgrade) and runs two hosts with the example configuration.
- CI and Release build through the same workflow (`build.yml`): native
  amd64 and arm64 builds with a smoke test of the package
  (`scripts/smoke-test.sh`).
- Fix: the release smoke test validated the examples under
  `/usr/share/doc`, which slim Debian images do not install; it now checks
  the unpacked package and the installation separately.
- Workflows use the Node 24 versions of the Docker and artifact actions,
  without provenance attestation and build records.

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
  validation, network and local settings, VLAN range and mapping table with
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
  a restart. Confirming a local change raises the network revision.
- Loop guard: MAC addresses flapping between GRETAP ports shut down the
  offending port (spoke ports first) until `wanderlan unblock`.
- Automatic sync of `network.toml`: probes announce revision and fingerprint,
  sites fetch a newer network from a direct peer over TCP, apply it with the
  commit timer and confirm it automatically once a peer is reachable;
  rejected versions are not fetched again, removed peers may fetch for 5
  minutes, changes based on an outdated revision are refused.
- Event log (ring buffer, also to journald), throughput per port.
- Netlink dumps on one socket are serialised (concurrent dumps fail with
  EBUSY); a restarted daemon keeps the existing MSS rules.
- Integration tests: rollback after the deadline, rollback of edited files,
  invalid configurations with line numbers, an external layer-2 loop cut by
  the loop guard, sync of a change to all sites, refusal of outdated
  changes, a new site fetching the network from its hub.

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
  with three hubs, all connected with each other), topology check via `wanderlan validate`,
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
- Loading and validation of `network.toml` and `local.toml` with line numbers:
  unique names and addresses, hub/spoke references, topology check, VLAN
  ranges and mappings, durations, MSS, unknown keys.
- `wanderlan validate <file> [--local <file>]` and `wanderlan init`.
- Scope: wanderlan builds no VPN. An existing transport network (usually a
  VPN set up beforehand) connects the sites; wanderlan only handles layer 2
  and VLANs on top of it.
