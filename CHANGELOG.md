# Changelog

All notable changes to wanderlan are documented in this file.

## [1.0.0] - 2026-11-25

Simpler: the sites behave like one switch, and a new site joins with a
single command. Update all sites; 0.1 and 1.0 do not sync with each other.

### Throughput: settings, perf and speedtest

- Spreading the receive work over the cores (RPS) is a setting of each
  site, no longer switched on by itself: `rps_transport` (`off`, `on`,
  `auto`) for the interfaces the tunnel packets come in by, `rps_trunk`
  (`off`, `on`) for the trunk ports, both `off` by default. Switched on
  automatically (as for a short while) it doubled the throughput on bare
  metal but cost a third in VMs with two vCPUs. `auto` measures it with each
  peer when its tunnel comes up (about 30 s, each tunnel at most every 10
  minutes; `wanderlan tune <site>` or `t` in the terminal UI by hand): each
  box keeps per network card what all its tunnels speak for (at least 10 %
  more in total, no tunnel more than 10 % less). The old values come back
  when wanderlan stops or resets, or the setting goes back to `off`.
- `skip_conntrack` (`clients`, `all`, `off`): which tunnel packets skip
  connection tracking; `all` spares it on a server for every tunnel, both
  ways (before: only for the tunnels of its clients, still the default).
- `wanderlan perf` names what limits the throughput of the box (the load of
  the cores, receive queues and the RPS settings, split packets, connection
  tracking) and what helps; `wanderlan speedtest <site>` measures TCP over
  the transport and frames through the tunnel (in 64 flows, like real
  traffic), both ways.
- The terminal UI shows the optimization of a tunnel while it runs (a line
  above the peers, `tuning…` in its row, the status line of every view) and
  in the peer details what was measured and kept, or the setting.

### Both sites in one switch network

- A site with VLAN mappings checks every 30 s whether its trunk and the
  trunk of a peer are in one switch network: a marker frame in the source
  VLAN through its tunnels that comes back at the trunk. Harmless as such
  (the status notes it), but a switch that binds devices to ports (DHCP
  snooping, UniFi "Rogue DHCP Server Detection", port security) then sends
  DHCP replies to the wrong port, and devices lose their address one by
  one. While a trunk is shared, wanderlan watches the DHCP of the mapped
  VLAN: requests of a device that go unanswered 3 times in a row make the
  requests from there ask for broadcast replies (nftables rule
  `wl dhcp-bcast`); if they stay unanswered, the status, the TUI and the
  log warn.

### Fixes, fourth review

- A mapping that comes while the tunnels run has its rules before its VLAN
  reaches the trunk (and the VLAN leaves before them): the trunk never
  takes the source VLAN as it is, not even for a moment. Where both sites
  are in one network (a test with two boxes on one switch), that moment
  was a loop through the tunnel, after which switches and bridges had
  devices at the wrong port.
- A copy of a client's probe sent again from elsewhere no longer counts:
  what a request of a client says (the test of its tunnel, its ports)
  counts only from the address where it answered a challenge. Before, a
  copy sent from another address could point the client's tunnel there.
- The direct test of a client tunnel: a test the server did not answer
  (the line was down) no longer counts as failed, and the pause between
  tests, which doubles after each fall back to NAT, is short again once
  the tunnel ran direct for as long as that pause (it grew up to a day).
- The old control port after a port change sends its requests with the
  nonce in use (a peer took every one of them for a restart), and closes
  when another network of the box takes its port.
- Several networks: if the first network cannot be applied on its own
  (its trunk port is missing), the others are applied without it instead
  of not at all. A network whose ports another network took, or that gets
  them back, closes or opens its sockets at once (not only after the next
  change). The VLAN window and the MAC table of a network no longer show
  the tunnels of another network in a bridge both share as trunk ports.
- Stop and reset: a trunk port the loop guard shut comes up again; the
  breakout is removed also when its state cannot be read (the sysctls of
  the uplink are restored), and an error reading nftables is reported
  instead of leaving its table behind silently. A breakout being removed
  whose state cannot be read is reported instead of waiting silently.
- Clients behind NAT that serve clients of their own: all packets of
  their uplink tunnel skip connection tracking (some were still tracked).
- Several networks: sites without an identity of their own (placeholders,
  sites of older versions) in a further network got the same tunnel
  interface name; each has its own now.
- A server can make a client that changes its own tunnel ("only the
  client") a server, and remove it also when its tunnel has a VLAN limit
  (both were refused on either side).
- A client that changed an older state of the network gets back the
  merged one with a revision above its own (before, it kept what it sent
  until the next change).
- A box that was reset and joins through a client again stays below the
  client it hung off where moving it would be refused by the server
  (the join seemed to work, then the site was out).
- Invitations: one used through a client whose server already took the
  new site is used up at once, also if the client restarts before the
  new site is up (before, it could be used a second time). A client
  waits at most 20 s for its server to take a joining site.
- Hand edits: a crash or restart while edited files are applied starts
  from the confirmed files again (before, the edit counted as confirmed
  and was passed on). Edits not applied yet when another change is
  confirmed are kept as `network.toml.edited` / `local.toml.edited` with a
  warning (they were overwritten). `wanderlan apply` refuses a
  `network.toml` with a revision below the current one (an editor that
  saved an older state would undo the changes since).
- `systemctl reload` while a change waits for confirmation (one from
  another site, confirmed in a moment) is no longer ignored: it waits for
  that change, and the edit is put onto it (kept as `*.edited` only where
  both changed the same).
- Sync: a client no longer takes back its uplink's older network right
  after its uplink took its change (the change was undone for a moment,
  and a refusal could go unnoticed). A fetch that ends while a change made
  here waits for confirmation is left for later (it reported that it
  replaced that change).
- Validation: a breakout on a VLAN number that a mapping takes at the
  site's trunk does nothing, which is now warned about; a tunnel listed
  twice in `local.toml` (by name and by identity) is an error.
- `wanderlan new`, `wanderlan join` and the setup wizard no longer choose
  the interface of the default route as the trunk when the address is that
  of a VPN running over it (the only network port was bridged and the box
  cut off).
- The commands the CLI suggests next (`wanderlan confirm`, `online ...
  --confirm`, `invite delete`, `status`, ...) carry `--net <name>` when
  another network was chosen; before, following them acted on the default
  network (and confirmed or missed the wrong change).
- The CLI no longer waits forever when the daemon cannot read a request
  (an older version that does not know it): the error is shown.
- `wanderlan breakout <site> on` with the VLANs missing is an error instead
  of showing the breakouts and succeeding; `init --dir` writes neither file
  if one of them exists; `reset` names the clients also when names repeat.
- Invitations of older versions that let the new site change itself keep
  that when the invitations are saved again, and `invite list` shows it.
- Terminal UI, site form:
  - The fields follow the rights in the running network, not edits in the
    draft. The name and the own VLANs are greyed out like the others where
    this site may not change them.
  - Fields that were not touched keep what a sync brought in the meantime.
    Before, saving put back the values from when the form was opened.
  - A rename there no longer gives two sites known only by their name the
    same identity.
  - `e` in a site's details window opens the form where Sites does.
- Terminal UI, elsewhere:
  - Editing a mapping or a breakout offers only sites this site may change
    (for a breakout only clients).
  - A sync merged into the draft keeps this site's own ports as they run.
  - The details window from the overview finds a site renamed in the
    draft.
  - Messages name sites, not their identities.
  - The wizard takes a pasted join line with `sudo` and `--net`.
  - The event log shows the oldest events at the top.
  - Changed mappings are marked as changed, not as new.
  - The VLAN window lists the tunnels by site.
  - A broken time zone file can no longer crash the terminal UI.

### Changes made at the same time are kept

- Every network the sites exchange carries the fingerprints it was made
  from (a comment line, not part of its fingerprint; older versions ignore
  it). A server that gets a newer network lacking a change of its own puts
  both together: each setting, each field of each site, each mapping and
  tunnel limit; where both changed the same, the newer one's is kept and
  the log names it. If where both started is not known any more (e.g. after
  a restart), the newer network applies and the change made here is kept in
  `network.toml.lost-<revision>` with a warning.
- A client sends its change to its server before it fetches a newer
  network, and a change made on an older view is put onto the current one
  instead of replacing what changed meanwhile. A client always gets back a
  revision above its own when its view differs from what it sent.
- A server made a client while it was away (it takes its new server for a
  server and sends nothing) gets a revision above its own from its server
  after 30 s, so that it fetches its view; no more hanging there.
- A refusal names the network it is about (a client that changed again
  since sends the new one); a refused push counts at the next resync; a
  newer network from the server tells why a change was refused. What was
  rolled back or refused is no longer fetched again after every change made
  here, what did not fit still is.
- A join that is rolled back after the server took it is sent to the server
  again (it no longer comes back with the server's next network).
- `network.toml` edited by hand with the revision it had gets the next one,
  so the edit reaches the other sites instead of being replaced; a file
  with a lower revision (an old copy put back) still takes the peers'
  network. A change made here gets a revision above what the servers
  announce.

### Admin flows

- `wanderlan new [--name] [--address] [--trunk] [--vlans] [--confirm]`
  starts a network from the command line (also with `--net`), as the
  wizard does. Both suggest the address of a VPN interface, list the
  addresses of the box and warn about a public one or the default route's.
- Invitations are valid for a day (`--valid 2h|7d|never`) and are removed
  when they expire; the logs name only the start of a code; `invite list`
  and the TUI show how long a code is still valid.
- `wanderlan unblock <site>` (or a port); the loop guard's warning names
  the command. `wanderlan status` has a SYNC column and names the interface
  of every tunnel and whether the peer has the same network.
- The TUI shows what another network of the box has (overview and status
  line) and who takes an offline site online where this site may not.
- Hints where they help: a join on a configured box points to `--net`; the
  join output names the site that takes the new one online; the reset
  prompt names the network and the clients that lose their way in;
  `networks remove` reminds of the other sites; "daemon not running" points
  to `systemctl status` and `journalctl`.
- `wanderlan role <site> client <server>`; `wanderlan manage <site>` without
  a value shows who changes the tunnel.
- Texts: VXLAN instead of GRETAP in the package descriptions; the key
  sequence `a`, `Enter`, `c`; the loop guard also shuts further trunk
  ports; mappings as `map = { <source> = <mapped> }`; settings of
  `[network]` are defaults; `apt purge` also removes `*.rejected`,
  `network.toml.v1`/`.v2` and `network.toml.lost-*`.

### Terminal UI

- Breakout: `Enter` on the uplink of a breakout lists the interfaces of
  this box to choose from (the default route first, then those that can
  take the traffic, with their addresses and default routes; point-to-point
  links such as WireGuard count), or a name typed in; trunk ports are not
  offered.
- The traffic graphs stand still between two samples: every sample of the
  daemon has columns of its own (whatever the moment of drawing, the
  jitter of the sampling time or the width of the terminal), and the graph
  moves by whole samples when a new one comes, instead of every second
  re-sorting the samples into columns. The scales keep their width, the
  LOAD column of the overview has a round scale, and the VLAN window
  records each sample of the daemon once, at its time.
- The graph of the VLAN window tells download from upload: it shows the
  traffic of the VLAN over the tunnels (in from the other sites, out to
  them). It added up all ports, and as the bridge sends out on one port
  what came in on another, in and out were always the same. Its tunnels
  are named by their site, not by their interface.

### Several networks on one box

- One daemon takes part in several independent networks:
  `wanderlan networks add|remove|list`, every command (and the TUI) takes
  `--net <name>` or `WANDERLAN_NET`, `B` in the TUI switches. A further
  network keeps its files in `/etc/wanderlan/networks/<name>` and
  `/var/lib/wanderlan/networks/<name>` with an identity of its own; the
  network in `/etc/wanderlan` stays as it is (the default network).
- A further network has a trunk of its own (then its VLANs may overlap) or
  shares one with other VLANs; one reconcile applies what all networks want,
  the earlier network keeps what two want. A change that takes something of
  another network is refused, the status names what a network lost.
- Ports another network of the box has are given up when a network is set
  up or joined (the next free ones in steps of 10); tunnels of a further
  network are called `v<tag>…` with the alias `wanderlan <network>/<site>`.
- The table `ip wanderlan` has its rules per tunnel port (rewritten once
  after the update).

### Identities instead of names

- Every reference to a site is its identity: `network.toml` has a table
  `[site."<identity>"]` per site with its `name` inside, `server`,
  `managed_by` and `tunnel_vlans` name sites by their identities (the
  names as comments), `local.toml` too (`self`, `[tunnels."<identity>"]`).
  Names are labels: a rename changes only the name, and two sites may have
  the same one once both have an identity (a server with two clients
  `c1`); where a name is not unique the UI shows `c1#Ab3dK9xQ`, and the
  command line takes that form, an identity, or a name only one site has.
- The daemon writes and sends only this form, also for a site without a
  box yet (a placeholder that follows its name, marked with a comment).
  Names are for people: in a file edited by hand a site may be named by its
  name (`server = "hq"`, `self = "hq"`, a new site as `[site.hq]`), and
  the older forms are read; the daemon replaces the names with the
  identities when it loads (also on `systemctl reload`) or applies the
  file and writes it back (the old file is kept as `network.toml.v1`/`.v2`).
- Tunnels are named by the identity of the peer (`vx` and 13 characters,
  alias `wanderlan <site>` in `ip link`): after the update every tunnel is
  built anew once (a few seconds), afterwards a rename keeps it up. The
  sets of the clients behind NAT are named by their tunnels.
- The sites exchange the network in this form only: **update all sites**.
  Until then the older ones get no changes, and a newer site asks an older
  one for the same network every 30 s (their fingerprints differ).
- `wanderlan vlan` shows a tunnel by the site it leads to.

### Fixes, third review

- Who changes a client's tunnel to its server is one option with three
  choices, `changes = "server" | "client" | "both"` (TUI: "Changes";
  `wanderlan manage <site> server|client|both`; `wanderlan invite --changes`).
  It covers what goes through that tunnel (VLANs, mappings, its tunnel
  VLANs, breakouts, online state) and the option itself; the address, the
  server and removing the client stay with its server, servers are changed
  by every server. It replaces `managed_by`/`self_managed`, which are read
  once and become `changes`. Older sites do not read it: update all sites.
- Settings take effect at once. The probes of a site carry its control
  and tunnel port (behind their tag, older versions ignore them), so its
  peers follow a change right after the apply (and a rollback) instead of
  after the confirmation and the sync. After another control port the old
  one keeps answering (probes and sync) until every peer probes the new
  one: a server no longer loses its clients (they never get contacted
  first, behind NAT the old port was the only way to it). A new interval
  of the path MTU check counts at once, and after another tunnel port a
  client tunnel runs through NAT until its test finds it direct again. In
  the TUI the settings of this host (Settings `8`, the trunk ports) are
  applied when saved, like `wanderlan settings`; `c` keeps them.
- Breakout only at clients: a server is where the gateways of its devices
  are, a breakout there would only bypass them. The Breakout view and
  `wanderlan breakout` offer only clients, a site that becomes a server
  loses its breakouts, one of a server from an earlier version is ignored
  with a warning; a client + server breaks out only towards its own server.
- Breakout: a frame for a router never floods the tunnels (also while its
  port is gone; the pair of a breakout goes only after its rules); IPv6
  breaks out only what goes to a router of the VLAN (learned from its
  advertisements), devices with global addresses reach each other directly;
  switching IPv6 forwarding on keeps the default routes learned from router
  advertisements; router traffic leaves only by its uplink; the way back
  only from the uplink (no clash with tools that copy conntrack marks),
  ICMP errors of the box reach the device; breakout UDP to the tunnel port
  is masqueraded again; works with `ipv6.disable=1`; a ULA-only uplink does
  not count for IPv6; forwarding switched on by another tool stays; another
  firewall that drops forwarded packets is named in the status.
- Join: a client that invites asks its own server before the new box gets
  its answer (no half-admitted site); a box that claims an identity of the
  network proves that it holds its key; a cloned box is refused also behind
  the same address; a rejoin without changes keeps the code; a rename is
  recognised when the old name went to a new box; mapped VLANs are VLAN
  IDs; a rolled-back first configuration from files is set aside; files do
  not run ahead of the configuration in effect when writing fails.
- Client + server: removing a site with tunnel limits works again; a
  refusal goes down to the clients only when the push held nothing of this
  site; grace times end again.
- Graphs: the VLAN window records only its VLAN (with all VLANs it held
  hundreds of MB); a renamed peer keeps its history; a clock going back
  starts it again; no gaps between parts narrower than the samples; less
  work per second.

### Traffic graphs

- The daemon records the traffic of its tunnels and trunk ports and every
  round trip of the last hour (`history` request): the graphs are full as
  soon as the terminal UI starts, with a real time axis.
- Graphs mirrored: in above the line, out below it, each on a round scale
  of its own, with now, mean and peak in the title; nothing hides the other
  direction any more. `z` switches between 2 minutes, 10 minutes and an
  hour.
- The Overview shows all tunnels together (on a tall enough terminal) and a
  LOAD sparkline per neighbour; the details of a neighbour show its round
  trips over the same time with the lost probes.

### Fixes found in a review

- Sync: a change a client of a client made and the top server refused is
  refused at every site on the way (it no longer goes round and round); a
  rolled-back change of a client stays rolled back (the client learns it);
  a client that is busy or failed is not sent the same change every second.
- A removed peer behind the same NAT address as another client gets its
  grace time too; the grace starts when the change is confirmed; a peer
  that comes back is tested anew (direct or NAT); a client that became a
  server is probed again; the probes restart without "Address in use".
- Rights: the VLANs of the network are decided with the rights of the site
  that passes a change on (a released site below a restricted one does not
  open them); a site new below a client brings its own ports; a tunnel limit
  towards a site a client does not see stays; a rename by a manager the
  client does not see is taken over.
- Validation: two servers at one address are found also with a client in
  between; a VLAN mapping may use one of the site's own VLANs. A native
  mapping drops tagged frames of its VLAN on the trunk.
- `wanderlan vlan <vid>` shows the rates (it measured nothing before); a
  peer that does not answer shows no round trip; rates round to the right
  unit everywhere (`1.0M`, not `1000.0k`).
- TUI: the cursor of the Overview and of the breakouts stays on its row;
  the ports of the VLAN window scroll; the VLANs of a peer keep their order;
  a site renamed in the draft finds its tunnel; an unconfigured site says
  so.

### Client + server: clients with clients of their own

- A client with clients of its own is their server in every respect: it
  knows all sites below it (also the clients of its clients), may change
  them like a server, and its server takes over the changes it passes on
  with the rights of the site that made them. Before, a change only a client
  of a client may make (its own ports) was refused by the server on top, and
  a third level of clients was lost again.
- The role shows it: `client of hq + server` (TUI, `wanderlan status`).
- A site is no longer moved away from under a client + server (it would
  never learn where it went): the change is refused and says how to do it.
- A server made a client of a client + server learns it from its old peers:
  they refused its fetch ("unknown identity"), as they no longer kept the
  keys of a peer in its grace time, and now give it its own view.
- The daemon's socket for the tests of the client tunnels could get the
  tunnel port of its own site by chance (a free port is picked at random);
  that site's tunnels then did not come up ("Address in use") until a
  restart. It now never takes the ports of its site.

### TUI: fixes and easier to use; sites and mappings on the command line

- Editing a site in the TUI kept neither its own ports nor its breakouts (a
  site with its own ports could not be edited by its manager at all).
- A rolled-back change comes back with what was edited during the
  countdown, also when a confirm or rollback comes too late; a review shows
  the draft as it is after a sync; changes of `local.toml` by the CLI are
  noticed (status: `local_fingerprint`); a draft that a sync overtook keeps
  the ports and identities of the other sites. Changes from a peer and joins
  are confirmed by the daemon itself: the TUI says so and waits for them.
- Selections follow their site, mapping or breakout after a sync; new
  mappings and breakouts only offer sites this site may change; the
  invitation form survives a lost connection.
- Site form: "Server" and "Changes" (nobody, a site as manager, or the
  manager and the site itself). "This site" in Settings has a form of its
  own; list and form use the same names.
- From a join to a running site: the server says when the site joined and
  that it starts offline, the Overview names offline sites, and `o` on a
  site of the Overview goes straight to the review.
- Finding out why a tunnel is down: the peer details show where it is
  probed (address, control and tunnel port of that site) and what to check;
  site details show its ports; the status line counts tunnels that are down;
  the reason is under the Sites list.
- Tables size their columns to the content and cut with `…`; the line below
  shows the selected row in full. `?` opens the help at the current view;
  without a daemon the status line says `no daemon`. Popups keep a blank
  margin. The breakout view has one row per segment, `native` for VLAN 1 and
  all uplinks; the daemon reports the uplinks before the first breakout.
- The warning about a ring of tunnels names the limit of a direct tunnel
  when that limit is what leaves the VLANs out.
- CLI: `wanderlan site add|remove|vlans`, `wanderlan vlans` and
  `wanderlan map add|del`, checked by the daemon like every other change;
  `wanderlan reset` names `wanderlan site remove` for the other sites.

### Local internet breakout per VLAN

- A VLAN of a site can reach the internet through that site's own uplink
  instead of the tunnel (`breakout = { <VLAN> = { uplink, except } }` in
  `[site.<name>]`), completely transparently: the devices keep their
  gateway, DHCP and DNS; frames from the trunk to public IPv4 and IPv6
  destinations leave through the uplink, masqueraded, private destinations
  and the exceptions of the VLAN go through the tunnel as before. The
  uplink is the interface of the default route or one chosen per VLAN (a
  second line with a default route of its own). Nothing of a breakout
  reaches a tunnel; a family that cannot break out stays on the tunnel and
  the status says why.
- In the kernel: a veth pair per VLAN and segment (`wlbo<vid>` in the IP
  stack, `wlbp<vid>` in the bridge), the nftables tables `bridge
  wanderlan-breakout` and `inet wanderlan-breakout`, routing rules of
  protocol 87 for the answers, forwarding where it is needed (put back
  afterwards, written down in `/var/lib/wanderlan/breakout-sysctl`). The
  unit may write `/proc/sys/net/ipv{4,6}/conf` and loads `veth`.
- Its manager decides, like for the VLANs of a site. TUI: new view Breakout
  (`7`, before Settings `8`); CLI: `wanderlan breakout`; `wanderlan status` shows it. With eight
  views the brand in the tab bar shows from 90 columns on.
- Update all sites before using it: older ones do not read a network with
  a breakout (networks without one stay as they were).

### Settings of a site only on that site

- Every setting of the Settings view (`8`) changes this site only, in the
  TUI and with `wanderlan settings [<setting> <value|default>]`: the commit
  timeout, the path MTU check and a fixed MSS go into `local.toml`; the
  control port and the tunnel port into the site's own entry of the network
  (`site.<name>.control_port` / `tunnel_port`), which only the site itself
  may change; the other sites learn them with the network and connect
  there (also right after a change: they fetch from where its probes come
  from).
- The values of `[network]` (ports, commit timeout, MTU check, MSS) are only
  the defaults of the sites without their own and cannot be changed any
  more; the network VLANs stay where they were (VLANs `3` → `v`, Sites `2` →
  `m`), with the rights of before.

### Tunnel mode found out by itself; fixes

- No `tunnel_mode` setting and no `wanderlan tunnel-mode` any more: a client
  and its server find out themselves whether the tunnel can run `direct`.
  The client asks for a test in an authenticated probe and listens at its
  tunnel interface for two seconds; the server sends a test frame to the
  client's address at the tunnel port from another port (a tag of the
  tunnel's keys over the request). If it arrives, both run `direct`; two
  failed tests in a row bring them back to `nat`, with a growing pause.
  `wanderlan status`, the TUI (peer details) and the log tell the mode and
  why. Old `tunnel_mode` lines are read and ignored with a warning; with
  mixed versions a network that had `direct` set differs in its
  fingerprint until all sites are updated.
- Sync and joins: a revision cannot run past the largest one (a client's
  view far ahead is refused); a push is answered "refused" or "busy" when it
  was not taken over (it said "ok"); a join never takes over the entry of
  the inviting site; a released client may not change who manages it or
  its server; a client's tunnel port is taken over; the fingerprint does not
  depend on the order of the mappings and tunnel limits (a client re-pushed
  every 15 s); a fetched network that is not newer is not used.
- Control port: at most 8 connections per address and 3 s for a line an
  honest peer sends at once; a push waits for its server's decision.
  Unauthenticated (version 1) announces count only for peers without a
  key; a forged answer no longer ends a probe as "wrong key"; the probe
  service never waits for the controller; a client that is gone is not
  probed at its old NAT mapping forever; a restarted server gets the next
  keepalive at once.
- A renamed client keeps its tunnel up (identity, endpoint and mode move
  with the name). `tunnel_port` must be at most 65534 and neither it nor
  the port above may be the control port.
- `ip_gre` is no longer unloaded (it would remove GRE tunnels of others).
- Faster: a reconcile reads the remote of each tunnel interface with one
  small request instead of every forwarding table of the box; a client that
  is gone costs no `nft` every 2 s; the TUI sums the VLAN traffic in one
  pass and does not redraw for mouse moves.
- CLI: edit commands print why the daemon refused a change and its
  warnings; `wanderlan trunk <site>` needs a tunnel to that site; join and
  rename texts fixed.
- TUI: forms find their site or mapping by name (a sync no longer makes them
  edit another one); editing a site keeps who manages it unless "Changes"
  is changed, a rename there follows the rename rights; a change that was
  rolled back (also by the countdown) comes back into the draft; a draft
  that a sync overtook builds on the new configuration; after a reset no
  old draft is left; c and r work where the banner says so; no frozen
  countdown without the daemon; quitting tells what would be lost; a reset
  shows that it runs; the overview says why tunnels are down (offline, key)
  and a blocked trunk port is freed with `U`; the setup's trunk list
  scrolls, refuses what the port window refuses and takes the whole
  `wanderlan join …` line; the invitation form keeps its input when refused
  and a new invitation opens the list; a server with clients is not
  deleted; the tunnel port is in Settings; site details scroll; the log
  window stays full; the round-trip history shows the newest samples;
  secondary text uses the terminal's dim colour.

### VXLAN tunnels, clients behind NAT

Update all sites and open UDP 51822 (the tunnel port, `network.tunnel_port`)
between the sites: every tunnel is VXLAN now (`vx-<site>` instead of
`gt-<site>`); GRE between the servers is no longer needed.

- Every tunnel is a VXLAN interface on the tunnel port, the VNI from the two
  identities. VXLAN is segmented and aggregated (GSO/GRO) by every kernel
  and offloaded by many network cards. The GRE tunnels of an older version
  are removed on the update.
- A client's tunnel runs in one of two modes, `tunnel_mode` in
  `network.toml`, `wanderlan tunnel-mode <client> nat|direct` or the site
  form of the TUI (its manager decides):
  - `nat` (the default, also right after joining): the client may sit behind
    NAT, several behind the same one, also with a changing public address.
    It opens every connection to its server; its server never contacts it
    first, it answers and sends where the client's packets come from. A
    keepalive frame every 15 s keeps the NAT open; the server learns the
    NAT's port from it in the kernel (nftables table `ip wanderlan`) and
    follows a new port in place.
  - `direct`: the server sends to the client's address, both ends from a
    source port per connection, which spreads the load over the receive
    queues; no keepalives. Between servers always.
- Cheap per packet: only the keepalives reach the learning rule (data frames
  stop at one compare of their inner ethertype), and where connection
  tracking is loaded the server exempts the tunnel packets from it
  (`notrack`). The table is written in the syntax of `nft -f`, so nftables
  1.0 (Debian 12) works too.
- The MSS is 94 bytes below the path MTU for every tunnel; larger packets are
  fragmented outside as before, also with DF.
- Through NAT both ends send from the tunnel port. A VXLAN interface cannot
  be held to one source port any more (Linux includes the upper end of the
  range since March 2026), so a rule in `ip wanderlan` sets the tunnel port
  in the packets of these tunnels.
- A client sends its changes to its server itself (`SYNC` with a body); the
  server no longer fetches them. Its probes are answered, and the server
  probes it only where its authenticated probes come from, once that place
  answered a challenge.
- Several clients may share an address when they have identities; a box
  that joins behind the NAT of a client becomes a client of its own (with the
  same host name it needs `--name`).
- The service also has `CAP_NET_RAW` and `AF_PACKET` (for the keepalives)
  and loads the module `vxlan` instead of `ip_gre`.

### Identities and keyed tunnels

Update all sites: an older version cannot read a `network.toml` with
identities, and joins need the new join protocol (2) on both sides.

- Every box has an identity, an X25519 key pair created when it is first
  configured or joins (`/var/lib/wanderlan/identity`); its public key is the
  site's `id` in `network.toml`. A site of an older version claims its
  identity at its first start.
- The keys of a tunnel derive from the two identities and never travel: the
  VNI of the tunnel (the kernel drops packets of another box), the key of
  the control probes (version 2, `WLP2`: every probe and answer carries a
  tag) and the key of the configuration sync (`SYNC`, both sides prove their
  key over fresh nonces).
- A tunnel to a site with an identity comes up only once that site has
  answered the probes with its key. Something at its address that answers
  with another key keeps it down ("wrong key"); `wanderlan status`, the TUI
  (peer details) and the log say so.
- Joins tell boxes apart by their identity: the same box joins again (also
  from a new address); a reset box takes over its old entry only if the old
  box does not answer any more; a second box at the address of a running
  site, or a cloned machine with the identity of a running site, is refused.
  Before, a second box at the same address (behind the same NAT) took over
  the first one's site silently when it joined with its host name.
- Renames are recognised by the identity; a `network.toml` written without
  the `id` lines keeps the identities.

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

### Renaming sites

- A site can always rename itself, also a client that is restricted:
  Sites (2), `N` (a window with only the new name), or `wanderlan rename
  <site> <new-name>`. Its manager (`managed_by`, "Changes") may rename it
  too; other sites may not, and a client never renames its server. Before,
  a client could not rename itself at all. (`rename_by_manager` of 1.0.22
  is read and ignored with a warning.)
- The rename reaches every site: a renamed site is recognised by its
  address. The server takes over the new name of its client (also where
  others refer to it, e.g. `managed_by`), the client follows a rename by
  its server (its `self` in local.toml), and every site moves the trunk of
  the tunnel to the renamed site in its local.toml along. Before, the
  server refused the client's change and the client refused the server's
  network, so they stayed apart.
- Fix: after a site was renamed (or replaced by one with the same
  address), its old name kept answering for the address, so the renamed
  site was given an empty network and fetched it again every second.
- A renamed site shows as changed (`*`) in the draft, not as a new site
  and a removed one.
- The site window no longer offers "any site" under "Changes" and has no
  "Connected to" any more: a site added by hand is a client of this site,
  an edit keeps the role (`wanderlan role` changes it). `wanderlan manage
  --anyone` is gone too.

### A more readable network.toml

- Every site is a table `[site.<name>]` with everything about it:
  `server = "hq"` for a client (a server has no `server`; no more
  `uplink = ""`), its VLANs, its mappings as `map = { <source VLAN> =
  <mapped VLAN> }` (e.g. `map = { 13 = 113, 20 = "native" }`, read like the
  mapping view) and the limits of its tunnels as `tunnel_vlans = { hq =
  "30-40" }`. Before: `[[site]]` with `name`, separate `[[map]]` blocks with
  `site`, `local` and `shared`, and `[[tunnel]] between = [...]`.
- The old form is still read. The daemon writes an old file in the new form
  when it starts (the revision stays) and keeps it as `network.toml.v1`; an
  older version can only read that one.
- The sites exchange the network in the old form, byte for byte as before,
  so sites with older and newer versions keep syncing during an update.
- Messages say `server` where they said `uplink`, and name the sites of a
  tunnel limit instead of its number.

### VLAN mappings: source VLAN and mapped VLAN

- A mapping now speaks of the **source VLAN** (the VLAN of the network,
  before "shared") and the **mapped VLAN** (its name on the site's trunk,
  `native` or a VLAN ID; before "local"): in the mapping table (columns
  Site, Source VLAN, Mapped VLAN; meaning `source VLAN 15 = VLAN 115 at
  site2`), the edit window (the same order), the site details, the
  invitations, the help, the check messages, the log lines of the
  translation, `wanderlan invite --map MAPPED:SOURCE` and the README.
- `network.toml` keeps its keys (`local`, `shared`); existing files load
  unchanged.

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
