# wanderlan

wanderlan connects several sites on layer 2, so that the same VLANs, subnets
and DHCP servers are available at every site as if it were one network. All
sites together behave like one big switch: plug a site in, choose which VLANs
it takes part in, map VLAN IDs or put untagged traffic into a VLAN.

wanderlan does **not** build a VPN. The sites must already reach each other
through an existing transport network, usually a VPN set up beforehand
(WireGuard, IPsec, a provider network, …). Every site has an **address** in
that network. On top of it wanderlan builds the layer-2 part:

```
Clients ─ trunk ─ br0 ─┬─ gt-<peer1> ─┐
                       └─ gt-<peer2> ─┴─ existing VPN / transport network
```

* one GRETAP tunnel `gt-<peer>` to every other site, between the site
  addresses,
* a Linux bridge `br0` with VLAN filtering (STP off) with the local trunk and
  all tunnels as ports,
* VLANs per site (like the allowed VLANs of a switch port) and VLAN mappings
  per site (untagged → VLAN, VLAN → VLAN),
* joining with an invitation code: `wanderlan join <address> <code>`,
* TCP MSS clamping from measured path MTUs, GRE fragmentation as safety net,
* loop protection (split horizon plus a loop guard),
* changes with automatic rollback unless confirmed,
* automatic synchronisation of the shared configuration between all sites,
* an interactive terminal UI (`wanderlan`) and a daemon (`wanderland`).

Gateways and DHCP servers of the stretched VLANs stay on your existing
routers; wanderlan neither assigns addresses in the VLANs nor routes.

## Contents

- [Requirements](#requirements)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [Terminal UI](#terminal-ui)
- [Command line](#command-line)
- [How it works](#how-it-works)
- [Troubleshooting](#troubleshooting)
- [Design decisions](#design-decisions)
- [Development](#development)
- [Maintainers: releases and the APT repository](#maintainers-releases-and-the-apt-repository)

## Requirements

* **Debian 13 (trixie)** is the target platform. Debian 12, Ubuntu 24.04 and
  Proxmox VE 9 (based on Debian 13) and 8 work as well; the binaries are
  static (musl) for amd64 and arm64.
* A VM or small host per site, connected to the local network with a
  **VLAN trunk** (an interface that carries the VLANs tagged). The trunk must
  not be part of another bridge or bond.
* An **existing transport network** between the sites (usually a VPN) in
  which every site has an IPv4 address. It must carry
  * GRE (IP protocol 47) between the site addresses, and
  * UDP and TCP on the control port (default 51821) between the site
    addresses (path MTU probes, configuration sync).
* Kernel modules `bridge`, `ip_gre`, `sch_ingress`, `cls_flower`, `act_vlan`,
  `act_gact`, `nf_tables` (all part of the standard kernels; the package loads
  them at boot).

All sites are equal: every site has a tunnel to every other site, so **every
site must reach every other site** in the transport network (with a VPN
server in the middle: routing between the VPN clients must be allowed).

## Installation

Add the APT repository and install the package:

```
curl -fsSL https://upchui.github.io/Wanderlan/key.gpg | sudo gpg --dearmor -o /usr/share/keyrings/wanderlan.gpg
echo "deb [signed-by=/usr/share/keyrings/wanderlan.gpg] https://upchui.github.io/Wanderlan/apt stable main" | sudo tee /etc/apt/sources.list.d/wanderlan.list
sudo apt update && sudo apt install wanderlan
```

On minimal installations `curl` and `gpg` may be missing:
`sudo apt install curl gpg`.

Every commit on `main` that passes all tests is published automatically,
as version `<version>+<build>.g<commit>` (e.g. `0.1.0+57.gc256ea5-1`);
updates come with `sudo apt upgrade`. The last 20 of these builds and every
tagged release (e.g. `0.1.0-1`) stay in the repository:
`apt-cache madison wanderlan` lists them and
`sudo apt install wanderlan=<version>` downgrades.

The package installs

| Path | Content |
|------|---------|
| `/usr/bin/wanderland` | the daemon |
| `/usr/bin/wanderlan` | terminal UI and command line |
| `/usr/lib/systemd/system/wanderland.service` | systemd unit, enabled and started |
| `/etc/wanderlan/mesh.toml`, `local.toml` | configuration (conffiles, empty templates) |
| `/usr/share/doc/wanderlan/examples/` | example configuration (`wanderlan init` prints it too; minimal images such as Docker's `-slim` skip `/usr/share/doc`) |
| `/usr/lib/modules-load.d/wanderlan.conf` | kernel modules loaded at boot |

and creates the group `wanderlan`. Members of this group can use the TUI and
the command line without root:

```
sudo usermod -aG wanderlan $USER    # then log in again
```

## Quick start

On the **first site** (say `heim` at transport address `10.8.0.1`):

1. Run `wanderlan`. On a site without configuration the setup wizard opens;
   choose **Start a new network**. The name (from the host name), the trunk
   (the interface that does not lead to the transport network) and the
   address are suggested; add the VLANs of the network (e.g. `10-50`).
2. Review the change and apply it, then confirm it with `c` within the
   commit timeout (60 s by default), otherwise it is rolled back.
3. The wizard creates an invitation right away and shows the command for the
   next site, e.g. `wanderlan join 10.8.0.1 K7F3Q-9QX2M`.

On **every further site** install the package and run that command:

```
sudo wanderlan join 10.8.0.1 K7F3Q-9QX2M
```

The new site gets its name from the host name (`--name` to choose one),
picks the trunk automatically (`--trunk` if there is more than one
candidate), and receives the configuration of the network; the inviting site
adds it with the address the request came from. Both sides confirm
automatically as soon as they reach each other, and all other sites build
their tunnel to the new site through the configuration sync. In the TUI the
wizard offers the same as **Join an existing network**.

A new site starts **offline**: its tunnels exist but stay down, while the
control channel already shows whether it is reachable, its round trip and
path MTU. Check everything, then take it online on the inviting site:
`sudo wanderlan online <site> --confirm` (TUI: Sites, `o`, `a`, `c`). Online
stays online, also across restarts, until someone takes it offline.

Invitations can be created on **any** site of the network (`wanderlan
invite`, or `i` in the TUI); the command contains that site's address. An
invitation can carry **rules for the new site**: name (`--name`), VLANs
(`--vlans native,10-50`), the VLAN for its untagged traffic (`--native 20`),
translations (`--map 130:30`). A code works once and stays valid until it is
used or deleted (`wanderlan invite list`, `wanderlan invite delete <code>`;
TUI: Sites, `I`).

The site that created the invitation **manages** the new site: only it may
change the new site's VLANs, mappings and online state, or remove it. With
`--self-managed` the new site may change its own rules too. Sites without a
manager (the first site, sites added by hand) may be changed from every
site.

To start over on a site: `sudo wanderlan reset` removes the bridge, the
tunnels, all rules, the configuration and the invitations; the site is
unconfigured again, like after the installation. Every step runs even if an
earlier one fails, and the daemon checks afterwards that nothing is left
(otherwise it says what and the reset can be repeated). The kernel creates
the devices `gre0`, `gretap0` and `erspan0` by itself when it loads its GRE
module for the first tunnel; as root, outside containers and if no other GRE
tunnel uses the module, the reset unloads it so they disappear too. Remove
the site on another site too (TUI, Sites, `d`) if it leaves the network for
good; until then the others keep listing it, which does not stop it from
starting or joining another network.

Without the TUI: write `/etc/wanderlan/mesh.toml` and `local.toml` (see
`wanderlan init` or the examples), check them with `wanderlan validate`,
then `sudo wanderlan apply` and `sudo wanderlan confirm`.

## Configuration

All sites share **`/etc/wanderlan/mesh.toml`**; the daemon keeps it identical
on all sites. What is specific to a host is in **`/etc/wanderlan/local.toml`**,
which is never synchronised.

`local.toml`:

```toml
self = "haus2"     # name of this site in mesh.toml
trunk = "ens19"    # local interface carrying the VLANs
```

`mesh.toml`:

```toml
[mesh]
vlans = "10-50"               # VLANs of every site that does not list its own
mtu_probe_interval = "5m"     # how often the path MTU to each peer is measured
# mss = 1300                  # optional: fixed TCP MSS instead of measuring
commit_timeout = "60s"        # changes are rolled back unless confirmed in time
# control_port = 51821        # UDP/TCP port for probes, sync and joining
# revision = 12               # maintained by wanderlan, do not edit by hand

[[site]]
name = "heim"
address = "10.8.0.1"          # address of the site in the transport network

[[site]]
name = "buero"
address = "10.8.0.2"

[[site]]
name = "haus2"
address = "10.8.0.11"
vlans = "20,30"               # haus2 only takes part in VLANs 20 and 30

[[site]]
name = "haus3"
address = "10.8.0.12"

[[map]]                       # untagged traffic on the trunk of haus3 is VLAN 20
site = "haus3"
local = "native"
shared = 20

[[map]]                       # local VLAN 130 at haus3 is shared VLAN 30
site = "haus3"
local = 130
shared = 30
```

### Settings

| Key | Default | Meaning |
|-----|---------|---------|
| `mesh.vlans` | `all` | VLANs of the network, used by every site without its own `vlans`. Missing, empty or `"all"`: every VLAN and the untagged traffic, like a trunk between two switches. Otherwise e.g. `"native,10-50"`, `"10,20,30-40"` or a list `[10, "20-30"]`. `native` is the untagged traffic (the native VLAN, carried as VLAN 1 between the sites, so tagged VLAN 1 on a trunk is the same network, like on most switches). |
| `mesh.mtu_probe_interval` | `5m` | Interval of the path MTU measurement (at least 10 s). |
| `mesh.mss` | unset | Fixed TCP MSS (536–1460) instead of measuring. |
| `mesh.commit_timeout` | `60s` | Time to confirm a change (10 s–1 h). |
| `mesh.control_port` | `51821` | Port of the daemon-to-daemon control channel. |
| `mesh.revision` | `0` | Revision counter used by the sync; raised automatically. |
| `site.name` | – | Unique, `a-z`, `0-9`, `-`, at most 12 characters (it becomes part of the interface name `gt-<name>`). |
| `site.address` | – | Unique IPv4 address of the site in the transport network. |
| `site.vlans` | `mesh.vlans` | VLANs this site takes part in, like the allowed VLANs of a switch port. A tunnel carries the VLANs both of its sites take part in. |
| `site.online` | `true` | `false`: all tunnels of the site stay down. Sites that join with an invitation start with `false`. |
| `site.managed_by` | – | The site that invited this one; only it (and the site itself with `self_managed = true`) may change the site's VLANs, mappings and online state. |
| `site.self_managed` | `false` | The site may change its own rules too. |
| `map.site` | – | Site the mapping applies to. |
| `map.local` | – | `"native"` (untagged) or a local VLAN ID (not 1). |
| `map.shared` | – | The VLAN ID in the shared network; must be one of the VLANs of the site, not 1 (the native VLAN). |

The configuration is checked when it is loaded; every problem is reported
with file and line, e.g.

```
mesh.toml:24: error: site "haus2": vlans: VLAN ID 5000 is outside 1-4094
mesh.toml:42: error: map #2: shared VLAN 60 is not one of the VLANs of site "haus3" (10-50)
```

Rules: names and addresses are unique; shared VLANs of mappings are VLANs of
their site; per site at most one native mapping, no local VLAN mapped twice
and no two local VLANs onto the same shared VLAN; `self` exists in the mesh;
unknown keys are errors (typos). Configurations of wanderlan 0.1 with `role`
and `hub` still load; these keys are ignored with a warning.

### VLAN mappings

* **Native VLAN of the network** (`native` in the VLANs, part of `all`):
  untagged traffic of all sites is one network and leaves every trunk
  untagged. Careful: if the untagged LANs of two sites are already connected
  by another path, this is a loop (the loop guard cuts the tunnel).
* **Native mapping** of a site: its untagged frames belong to the shared
  VLAN, and the shared VLAN leaves its trunk untagged; the native VLAN of the
  network then does not reach this site.
* **Translation** `local = L`, `shared = S`: frames tagged L on the trunk are
  rewritten to S when they enter, S is rewritten to L when it leaves (tc
  `clsact` + `flower` + `vlan modify`). At that site, frames that arrive
  already tagged S are dropped, so the local VLAN S and the translated VLAN
  never merge; if L is itself a shared VLAN, it is not available as shared
  VLAN at that site (the validator warns about it).

### Changes, rollback and synchronisation

Every change of the running configuration (TUI, `wanderlan apply`, or a
configuration received from a peer) is applied immediately and must be
confirmed within `commit_timeout`; otherwise the previous state is restored.
So nobody locks themselves out when working through the tunnel itself.

Confirming a local change raises `mesh.revision` and writes the files. The
daemons announce revision and fingerprint of their `mesh.toml` to their
peers; a site that sees a newer revision at a direct peer fetches it, applies
it and confirms it automatically as soon as a peer is reachable with the new
configuration. Configurations that fail are not fetched again. A change that
is based on an outdated revision is refused ("reload and try again"), so
concurrent edits do not silently overwrite each other.

If you edit the files by hand, run `sudo wanderlan apply` (and `confirm`);
not confirmed edits are restored and kept as `mesh.toml.rejected`.
`systemctl reload wanderland` (SIGHUP) takes the files as they are, without
raising the revision.

### MTU and MSS

The daemon measures the path MTU to every peer by binary search with UDP
probes that have the DF bit set, at start, every `mtu_probe_interval` and
when a peer becomes reachable again. From it, the TCP MSS is

`MSS = path MTU − 20 (outer IP) − 4 (GRE) − 18 (Ethernet + VLAN tag) − 40 (inner IP + TCP)`

and TCP SYNs that enter or leave a tunnel port with a larger MSS are
clamped in the nftables table `bridge wanderlan` (IPv6: 20 bytes less).
Ports without a measurement use the smallest measured value. GRETAP
fragmentation (`ignore-df`, `nopmtudisc`, MTU 1500) stays on as safety net,
so non-TCP traffic larger than the path still arrives.

### Loop guard

Split horizon keeps the network itself loop-free, but the LANs of two sites
may also be connected by another path. The daemon watches the bridge's
forwarding database: if the same MAC address keeps moving between the trunk
and a tunnel, or between two tunnels (5 times within 10 s), it shuts the
tunnel port down (never the trunk), logs it
and shows it in the TUI. The port stays down until it is re-enabled with
`wanderlan unblock gt-<site>` (or `u` in the TUI) or the configuration
changes.

## Terminal UI

`wanderlan` without arguments starts the TUI (keyboard first, works over
SSH and in 80×24; `--mouse` enables the mouse). It talks to the daemon only
through `/run/wanderlan/wanderlan.sock`.

| Key | View |
|-----|------|
| `1` | Overview: all sites, live status per peer (last reply, RTT, path MTU, MSS, port state, MAC addresses, throughput, config in sync), total traffic of the tunnels and the trunk. `Enter` on a peer: details with a traffic graph (received/sent), round-trip history, min/avg/max round trip, jitter, probe loss, estimated one-way latency (round trip / 2), byte/packet/error/drop counters |
| `2` | Sites: invite a site with its rules (`i`), open invitations (`I`: show, delete), online/offline (`o`), add by hand (`n`), edit (`e`), delete (`d`) |
| `3` | VLANs: VLANs of the network (`v`), mapping table, conflicts are marked immediately |
| `4` | MAC table: filter (`/`), sort (`s`, `o`) |
| `5` | Log: live events, filter by level and peer |
| `6` | Setup: new network or join; on a configured site: invite (`i`, `I`) and reset (`R`) |
| `7` | Settings: every setting of the network and of this host with an explanation; `Enter` changes it |

Changes are collected in a draft. `a` shows the diff against the running
configuration and applies it; while a change waits for confirmation a
banner shows the remaining time, `c` confirms and `r` rolls back. `x`
discards the draft, `?` shows all keys, `q` quits.

## Command line

| Command | Purpose |
|---------|---------|
| `wanderlan status [--json]` | State of this site and its peers |
| `wanderlan validate <mesh.toml> [--local <local.toml>]` | Check files (offline) |
| `wanderlan apply [--mesh F] [--local F] [--timeout 60s]` | Apply files (default: those in `/etc/wanderlan`) |
| `wanderlan confirm` / `wanderlan rollback` | Confirm or revert the pending change |
| `wanderlan init [--dir D]` | Print or write the example configuration |
| `wanderlan export` | Print the running `mesh.toml` |
| `wanderlan unblock <port>` | Re-enable a port shut down by the loop guard |
| `wanderlan log [-f]` | Event log |
| `wanderlan probe` | Measure the path MTU to all peers now |
| `wanderlan invite [--name N] [--vlans V] [--native VID] [--map L:S]... [--self-managed]` | Create an invitation with rules for the new site; prints `wanderlan join <address> <code>` |
| `wanderlan invite list` / `wanderlan invite delete <code>` | Open invitations of this site |
| `wanderlan join <address>[:port] <code> [--name N] [--trunk IF]` | Join a network (on an unconfigured site) |
| `wanderlan online <site> [--confirm]` / `wanderlan offline <site> [--confirm]` | Take a site online (its tunnels come up) or offline |
| `wanderlan reset [--yes]` | Remove everything wanderlan set up on this site; unconfigured again |

The socket can be changed with `--socket` or `WANDERLAN_SOCKET`.

## How it works

On each site the daemon creates, and removes again when it stops:

* `br0`: bridge, `vlan_filtering 1`, STP off, no default VLAN, no
  netfilter calls; marked with the interface alias `wanderlan`;
* `gt-<peer>` for every other site: GRETAP from the own to the peer's
  address, `ignore-df`, `nopmtudisc`, MTU 1500, marked with the alias
  `wanderlan`; all tunnel ports are `isolated` (split horizon: isolated ports
  do not forward to each other, so a frame from one site only reaches the
  trunk of the other sites and never circles);
* the VLANs of the site on the trunk; on each tunnel the VLANs both sites
  take part in;
* tc `clsact` filters on the trunk for translations (recognised by their
  action cookie `wl:…`);
* the nftables table `bridge wanderlan` for MSS clamping.

It adopts what it created before (after a restart nothing changes) and never
touches interfaces it did not create: if a foreign `br0` or `gt-*` exists it
refuses to start the configuration instead. After every topology change it
flushes the learned MAC addresses (again after 10 s and 30 s, while the other
sites follow).

The daemon runs as root with `CAP_NET_ADMIN` as its only capability and a
hardened systemd unit. Logs go to the journal: `journalctl -u wanderland`.

## Troubleshooting

* **`wanderlan status` / the TUI says the daemon is not running**:
  `systemctl status wanderland`, `journalctl -u wanderland -e`. The TUI shows
  the state of the service and its last log lines itself. systemd restarts
  the daemon after a crash; the network keeps running meanwhile.
* **Typed `wanderland` instead of `wanderlan`**: harmless. `wanderland` is
  the daemon; started from a shell it only explains that, and a second daemon
  never starts while one runs (`/run/wanderlan/wanderland.lock`). If the API
  socket is deleted, the daemon creates it again within seconds.
* **Permission denied on the socket**: add yourself to the group
  `wanderlan` and log in again, or use `sudo`.
* **"trunk interface … is already attached to vmbr0"**: the trunk must not be
  part of another bridge or bond (on Proxmox use a dedicated NIC or VLAN-aware
  VM interface).
* **"interface br0 already exists and was not created by wanderlan"**: remove
  or rename the existing interface; wanderlan does not modify foreign
  interfaces.
* **Tunnel up but no traffic**: check that GRE (IP protocol 47) passes between
  the site addresses in both directions (firewalls on the hosts, the VPN's
  allowed IPs/policies); `tcpdump -ni any proto gre`. Check that the VLAN is
  one of the site's VLANs and that the switch port is a trunk with that VLAN.
* **Nothing from a client arrives (TX 0 towards all peers)**: the client
  probably sends untagged; untagged traffic is only bridged with a native
  mapping for that site (VLANs, 3, `n`), otherwise tag it (on Proxmox: VLAN
  tag on the VM's network device). On Proxmox also switch off the firewall
  (or its MAC filter) on the trunk NIC of the wanderlan VM: it drops frames
  with foreign source MACs, which is all bridged traffic.
* **`wanderlan join` fails**: the code is used up or wrong (create a new one
  with `wanderlan invite`), or TCP on the control port of the inviting site
  is not reachable. A site that does not answer within `commit_timeout` after
  joining is removed again on the inviting site; its code can be used again.
* **Peer "does not answer control probes" / no path MTU / no sync**: allow UDP
  and TCP on the control port (51821) between the site addresses; check that
  the peer lists this site in its configuration (`wanderlan status` on both).
* **Large transfers stall**: compare the path MTU in `wanderlan status` with
  your VPN's MTU; set `mss` in the configuration if probes are filtered.
* **A port is BLOCKED**: the loop guard found MAC flapping; look for a second
  layer-2 path between sites (e.g. two sites bridged by another link), fix
  it, then `wanderlan unblock gt-<site>`.
* **My change was rolled back**: confirm within `commit_timeout` (`c` in the
  TUI, `wanderlan confirm`). Rejected hand-edited files are kept as
  `/etc/wanderlan/*.rejected`.
* **A synchronised change did not arrive at a site**: `wanderlan log` shows
  why (e.g. invalid for that site, rolled back); fix the configuration and
  confirm a new revision.
* **Proxmox with the Proxmox firewall**: `br_netfilter` may be loaded;
  wanderlan disables netfilter calls on `br0`, host firewalls must still let
  GRE and the control port through.

## Design decisions

Deviations from the original specification, and why:

* **No VPN inside wanderlan.** Transport and encryption are left to an
  existing VPN, decided by the project owner; wanderlan only handles layer 2
  and VLANs. Consequently there are no keys, endpoints or WireGuard settings;
  sites are identified by their transport `address`, and GRE is unencrypted
  on top of that (trusted) transport.
* **Automatic configuration sync** (also a project decision), see above.
  Every site may change the mesh; trust between the sites is implied by the
  shared layer 2.
* **No hubs and spokes** (since 0.2, a project decision: "like a switch you
  plug a cable into"). Every site has a tunnel to every other site and all
  tunnel ports are isolated (split horizon). This needs every site to reach
  every other site in the transport network, but there is nothing to decide
  per site except its VLANs. The loop guard also watches trunk-to-tunnel
  moves, the typical sign of a loop through two sites' LANs.
* **Who may change a site** (project decision): the inviting site manages
  the sites it invited (`managed_by`), unless it allows them to manage
  themselves. Every wanderland enforces this for changes made on it (TUI,
  CLI, edited files). It is an agreement between the daemons, not a
  cryptographic protection: someone with root on a site could bypass it.
* **New sites start offline** (project decision): the tunnels of a joined
  site come up only when a user takes it online, after checking that it is
  set up correctly; the state is part of the shared configuration.
* **Native = VLAN 1 between the sites**: untagged traffic travels as VLAN 1
  in the tunnels, like the default native VLAN of most switches. An empty
  VLAN list means all VLANs plus native, like a plain trunk.
* **Joining with invitation codes**: a site joins with the address of any
  member and a one-time code created there; the member adds it with the
  source address of the request (no address has to be typed) and answers
  with the configuration. Codes do not expire; they are used up or deleted.
  The code protects the layer-2 access a new site gets; it travels in clear
  text through the (trusted) transport network.
* **Path MTU probes use UDP** between the daemons (DF set via
  `IP_PMTUDISC_PROBE`) instead of ICMP: no `CAP_NET_RAW`, and the same probes
  carry reachability, RTT and the sync announcements.
* **MSS clamping on entering and leaving SYNs** per tunnel port: clamping
  only on the egress port would give each direction the other side's value.
* **netlink everywhere except nftables**: links, bridge, VLANs, GRETAP and tc
  use rtnetlink (raw attributes where `netlink-packet-route` lacks them:
  `IFLA_GRE_IGNORE_DF`, the tc `vlan` and `gact` actions); nftables uses its
  JSON interface through the `nftables` crate (which runs `nft -j`, hence the
  dependency on the `nftables` package). The crate's 0.6.3 release swaps the
  `<`/`>` operators; wanderlan works around it and a test guards the JSON.
* Interface names `br0` and `gt-<site>` are fixed; site names are limited to
  12 characters so that `gt-<site>` fits into 15 characters.
* Debian 13 is the target; the build and test images are Debian 13.

Open questions are collected in [docs/OPEN-QUESTIONS.md](docs/OPEN-QUESTIONS.md).

## Development

Everything builds and runs in Docker; nothing else is needed on the
development machine.

```sh
scripts/check.sh              # cargo fmt --check, clippy -D warnings, tests
scripts/cargo.sh test         # any cargo command in the build container
scripts/build.sh amd64        # static binaries and .deb into dist/ (also arm64)
scripts/integration.sh        # .deb in Debian 13 containers, full integration tests
scripts/systemd-test.sh       # the daemon under systemd with the hardened unit
```

`scripts/systemd-test.sh` boots two Debian 13 containers with systemd as
PID 1 and the package installed, and goes through what users do: a new
network, an invitation, a join, `wanderland` typed by mistake, a deleted
socket, a reset (under `ProtectSystem=strict`), a new network while the old
one still lists the site, a crash, restart and stop. After every step both
services must be active and their journals free of internal errors.

`scripts/integration.sh` loads the required kernel modules into the host
kernel through a privileged helper container (on GitHub runners with sudo)
and runs the scenarios in `tests/integration/cases` on a topology of five
sites, a router standing in for the transport network and one client per
site (see `tests/integration/README.md`).

The workspace has three crates: `wanderlan-core` (configuration, topology,
desired state, reconciliation, system access behind traits with a netlink
and an in-memory implementation), `wanderland` (daemon) and `wanderlan`
(TUI and CLI).

## Maintainers: releases and the APT repository

Publishing is automatic: every push to `main` whose CI run is green
(`ci.yml`: fmt, clippy, tests, packages for amd64 and arm64 with a smoke
test, integration tests) is published by `publish.yml`:

* version `<Cargo.toml version>+<CI run number>.g<commit>`, package version
  with `-1`; the run number makes every build newer than the one before,
* the packages go into the signed repository on the branch `gh-pages`
  (one commit without history, so the branch does not grow), suite `stable`;
  the newest 20 commit builds per architecture stay, tagged releases forever,
* `verify-apt.yml` then waits until GitHub Pages serves the version,
  installs it with the commands above on fresh Debian 13, Debian 12 and
  Ubuntu 24.04, installs exactly this version and downgrades to the previous
  one, and runs two hosts with the example configuration.

Tags are optional: the tag `v<version>` (equal to the version in
`Cargo.toml`) makes `release.yml` build the same way, create a GitHub release
with binaries, packages and `SHA256SUMS`, and add `<version>-1` to the
repository, where it is kept forever. Later commit builds
(`<version>+<build>…`) count as newer. To start a new version line, raise
the version in `Cargo.toml`.

One-time setup (done for this repository):

1. Create the signing key (RSA 4096) outside the repository:
   `scripts/gen-apt-key.sh ~/wanderlan-apt-key`, and commit its public part
   as `key.gpg` in the repository root (every publication puts it on GitHub
   Pages as well). Current key fingerprint:
   `F6C4 00CF 318B FD47 E3A6  B8A7 7BC2 D7AE 6699 82D4`.
2. Store `private.asc` as the repository secret `APT_GPG_PRIVATE_KEY`
   (Settings → Secrets and variables → Actions) and keep a backup offline.
   Without it a publication stops with an error and changes nothing.
3. Set GitHub Pages to "Deploy from a branch", branch `gh-pages`, folder `/`.

Locally: `scripts/build.sh` (with `WANDERLAN_VERSION=…` for a specific
version), `scripts/smoke-test.sh dist <package version>`,
`scripts/test-apt-repo.sh` (retention, indices and signatures of the
repository script with a throwaway key).
