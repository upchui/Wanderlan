# wanderlan

wanderlan stretches VLANs between sites on layer 2, over a network that
already connects them (usually a VPN). It works like plugging switches
together: one site is the **server**, every further site is a **client**
plugged into it with an invitation code, and for every site you decide
which VLANs it gets, which VLAN its untagged traffic belongs to and which
VLAN IDs are translated.

```
                  hq   (server, VLANs: all)
                 /    \
  office (client of hq)   lab (client of hq)
  VLANs 10, 20            untagged traffic = VLAN 30
```

`office` and `lab` each have one tunnel, to `hq`. They have no tunnel to
each other and do not even know about each other; whether traffic flows
between two sites is decided by the VLANs alone.

wanderlan does **not** build a VPN, route or hand out addresses: the sites
must already reach each other through an existing transport network
(WireGuard, IPsec, a provider network, ...), and gateways and DHCP servers of
the stretched VLANs stay where they are. On top of the transport network each
site runs a small daemon (`wanderland`) that builds a VLAN-aware Linux
bridge and GRETAP tunnels, and a terminal UI (`wanderlan`) to set it up and
watch it.

## Contents

- [Concepts](#concepts)
- [Requirements](#requirements)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Everyday tasks](#everyday-tasks)
- [Configuration](#configuration)
- [How it works](#how-it-works)
- [Terminal UI](#terminal-ui)
- [Command line](#command-line)
- [Troubleshooting](#troubleshooting)
- [Project decisions](#project-decisions)
- [Development](#development)
- [Maintainers: releases and the APT repository](#maintainers-releases-and-the-apt-repository)

## Concepts

### Servers and clients

The first site of a network is a **server**. A site that joins with an
invitation becomes a **client** of the site that invited it, like a cable
plugged into a switch: its only tunnel leads to that site (its *uplink*), so
a client only has to reach its server in the transport network. Servers are
connected with each other (every server with every other server), so servers
must reach each other.

A client can invite sites too; they become its own clients. A site can be
made a server or the client of another site later. Every view shows the role
(`server`, `client of hq`).

### VLANs like a switch trunk

Every site takes part in a set of VLANs, like the allowed VLANs of a switch
port. Without a list it takes part in **all** VLANs and in the untagged
traffic, like a plain trunk between two switches. `native` in a list stands
for the untagged traffic, e.g. `native,10-50`; between the sites it travels as
VLAN 1, the default native VLAN of most switches.

A tunnel carries the VLANs that **both** of its ends take part in. So two
clients exchange traffic only in a VLAN that they and their server all have;
a VLAN the server does not take part in does not pass it.

### VLAN mappings

Per site, untagged traffic can be put into a VLAN (`native → 30`: what
arrives untagged at that site's trunk is VLAN 30 everywhere else, and VLAN 30
leaves its trunk untagged), and local VLAN IDs can be translated (`130 → 30`:
VLAN 130 at that site is VLAN 30 everywhere else). The VLAN mapping views
show next to each site whether it is a server or a client.

### Who knows what

Servers know the whole network. A client knows only itself, its server and
its own clients: it never learns about the clients of other servers, neither
in the terminal UI nor in its configuration files. Changes made on a client
(to itself and to its own clients) reach the network through its server.

### Who may change what

The site that created an invitation **manages** the site that joined with it.

| Change | Who |
|--------|-----|
| VLANs, mappings, online state of a managed site, removing it | its manager; the site itself too if it was released ("may change its own VLANs") |
| The same for a site without a manager (the first site) and the VLANs of the network | every site that is not *restricted* |
| Timeouts, MTU check, MSS, control port, invitations | every site |

A site that joined and was not released is **restricted**: it cannot change
any VLANs, only those of sites it invited itself. A client may in any case
only change itself and its own clients, never its server. The daemons enforce
this among themselves; it is not a cryptographic protection.

### Online, offline and confirmed changes

A site that joins starts **offline**: its tunnel exists but stays down, while
the control channel already shows whether it is reachable, its round trip and
path MTU. Once everything is right, its manager takes it online; online stays
online, also across restarts, until someone takes it offline.

Every change is applied immediately and must be **confirmed** within the
commit timeout (60 s by default); otherwise the previous state comes back.
So a change that cuts you off undoes itself.

## Requirements

* **Debian 13 (trixie)**; Debian 12, Ubuntu 24.04 and Proxmox VE 8/9 work as
  well. The binaries are static (musl) for amd64 and arm64.
* Per site a VM or small host with a **trunk**: an interface that carries the
  VLANs tagged (and the untagged network) to the switch or hypervisor. It
  must not be part of another bridge or bond.
* An **existing transport network** in which every site has an IPv4 address.
  Between each client and its server, and between all servers, it must carry
  * GRE (IP protocol 47), and
  * UDP and TCP on the control port (default 51821): probes, configuration
    sync, joining.
* The kernel modules `bridge`, `ip_gre`, `sch_ingress`, `cls_flower`,
  `act_vlan`, `act_gact` and `nf_tables` (standard kernels have them; the
  package loads them at boot).

## Installation

```
curl -fsSL https://upchui.github.io/Wanderlan/key.gpg | sudo gpg --dearmor -o /usr/share/keyrings/wanderlan.gpg
echo "deb [signed-by=/usr/share/keyrings/wanderlan.gpg] https://upchui.github.io/Wanderlan/apt stable main" | sudo tee /etc/apt/sources.list.d/wanderlan.list
sudo apt update && sudo apt install wanderlan
```

(On minimal installations first `sudo apt install curl gpg`.)

Every commit on `main` that passes all tests is published as
`<version>+<build>.g<commit>-1`, so `sudo apt upgrade` always brings the
newest build. The last 20 builds and every tagged release stay in the
repository: `apt-cache madison wanderlan` lists them,
`sudo apt install wanderlan=<version>` goes back. Update all sites to the
same build.

The package installs the daemon `wanderland` (systemd service, enabled and
started), the terminal UI and command line `wanderlan`, empty configuration
files in `/etc/wanderlan/`, examples in `/usr/share/doc/wanderlan/examples/`
and the group `wanderlan`. Its members can use `wanderlan` without root:

```
sudo usermod -aG wanderlan $USER    # then log in again
```

## Quick start

On the **first site** (here `hq` at transport address `10.8.0.1`):

1. Run `wanderlan`. On a site without configuration the setup wizard opens;
   choose **Start a new network**. Name (from the host name), trunk (the
   interface that does not lead to the transport network) and address are
   suggested. Leave the VLANs empty for all VLANs, or limit them, e.g.
   `native,10-50`.
2. Review the change (`Enter`), then confirm it with `c`.
3. The invitation form for the next site opens: its name, VLANs, the VLAN
   for its untagged traffic, translations and whether it may change its own
   VLANs. `Enter` creates the code and shows the command, e.g.
   `wanderlan join 10.8.0.1 K7F3Q-9QX2M` (`Esc` skips this; `i` in Sites
   later).

On the **new site** install the package and run that command:

```
sudo wanderlan join 10.8.0.1 K7F3Q-9QX2M
```

It takes its name from the host name (`--name` to choose one), picks its
trunk itself (`--trunk` if there are several candidates), receives its part
of the configuration and reports when both sides agree. In the TUI the
wizard offers the same under **Join an existing network**.

Back on `hq`, check the new site in the overview (reachable, round trip,
path MTU) and take it online: Sites (`2`), select it, `o`, `a`, `c`, or

```
sudo wanderlan online office --confirm
```

## Everyday tasks

Keys refer to the terminal UI (`wanderlan`); every change ends with `a`
(review and apply) and `c` (confirm).

**Invite a site.** Sites, `i`: name, VLANs, untagged VLAN, translations and
"Changes" (whether it may change its own VLANs). CLI:
`wanderlan invite --name lab --native 30 --map 130:30`. A code works once and
stays valid until used or deleted: Sites, `I` (`d` deletes), or
`wanderlan invite list` / `wanderlan invite delete <code>`. Codes can be
created on any site; the new site becomes a client of that site.

**Take a site online or offline.** Sites, select it, `o`; or
`wanderlan online <site> --confirm` / `wanderlan offline <site> --confirm`.

**Choose the VLANs of a site.** Sites, `Enter` on the site, field "VLANs"
(empty: the VLANs of the network). The VLANs of the network: VLANs (`3`),
`v`, or Settings (`7`).

**Put untagged traffic into a VLAN, translate a VLAN.** VLANs (`3`), `n`:
site, local VLAN `native` (or a local VLAN ID, e.g. `130`), shared VLAN
(e.g. `30`).

**Let a site change its own VLANs, move it.** Sites, `Enter` on the site:
"Changes" (only its manager / the manager and the site / any site) and
"Connected to" (server, or client of a site). CLI:
`wanderlan manage <site> --self-managed yes`,
`wanderlan role <site> client <server>`, `wanderlan role <site> server`.

**Give a tunnel its own trunk.** Normally all tunnels share the trunk. A
tunnel can be bridged with another local interface instead, as a segment of
its own at this site (bridge `wl-<interface>`, same VLANs and mappings): only
the network on that interface reaches the other site through this one.
Settings (`7`), "Trunk per tunnel", or `wanderlan trunk <site> ens20`
(`default` to undo).

```
hq
  ens19 ── br0 ───────┬── gt-office ══ office
                      └── gt-lab    ══ lab
  ens20 ── wl-ens20 ───── gt-dc     ══ dc
```

**Start over on a site.** Setup (`6`), `R`, or `sudo wanderlan reset`: the
bridges, tunnels, all rules, the configuration and the invitations are
removed and the site is unconfigured again, like after the installation
(also the kernel's `gre0`/`gretap0`/`erspan0` when it runs as root outside a
container and no other GRE tunnel needs them). Join again with a new
invitation; a client that joins through another site becomes that site's
client. If the site leaves for good, delete it on its server (Sites, `d`).

**Edit the files by hand.** Edit `/etc/wanderlan/mesh.toml` or `local.toml`,
check with `wanderlan validate /etc/wanderlan/mesh.toml --local
/etc/wanderlan/local.toml`, then `sudo wanderlan apply` and
`sudo wanderlan confirm`. Edits that are not confirmed are restored and kept
as `*.rejected`.

## Configuration

Two files in `/etc/wanderlan/`, both maintained by the daemon:

* `mesh.toml`: the network. A server holds the whole network; a client only
  its part of it (itself, its server, its own clients). The daemons keep the
  files in sync.
* `local.toml`: this host only, never synchronised.

`local.toml`:

```toml
self = "hq"         # which site of mesh.toml this host is
trunk = "ens19"     # the interface carrying the VLANs

# Optional: the tunnel to dc uses a trunk of its own (a segment of its own).
[tunnels.dc]
trunk = "ens20"
```

`mesh.toml` on `hq` (a server):

```toml
[mesh]
# vlans = "all"               # VLANs of every site without its own list
commit_timeout = "60s"        # changes are rolled back unless confirmed in time
# mtu_probe_interval = "5m"   # how often the path MTU to each peer is measured
# mss = 1300                  # fixed TCP MSS instead of measuring
# control_port = 51821        # UDP/TCP port for probes, sync and joining
revision = 12                 # maintained by wanderlan

[[site]]
name = "hq"
address = "10.8.0.1"          # address in the transport network

[[site]]
name = "dc"                   # a second server
address = "10.8.0.2"

[[site]]
name = "office"
address = "10.8.0.11"
vlans = "10,20"               # office only takes part in VLANs 10 and 20
managed_by = "hq"             # invited by hq
uplink = "hq"                 # a client of hq

[[site]]
name = "lab"
address = "10.8.0.12"
online = false                # joined, not taken online yet
managed_by = "hq"
uplink = "hq"

[[map]]                       # untagged traffic at lab is VLAN 30
site = "lab"
local = "native"
shared = 30
```

The `mesh.toml` of `lab` contains only `hq` (as its server), `lab` and lab's
mapping.

### Settings

| Key | Default | Meaning |
|-----|---------|---------|
| `mesh.vlans` | all | VLANs of every site without its own list. Missing, empty or `"all"`: every VLAN and the untagged traffic. Otherwise e.g. `"native,10-50"`, `"10,20,30-40"` or `[10, "20-30"]`. |
| `mesh.commit_timeout` | `60s` | Time to confirm a change (10 s to 1 h). |
| `mesh.mtu_probe_interval` | `5m` | Interval of the path MTU measurement (at least 10 s). |
| `mesh.mss` | unset | Fixed TCP MSS (536 to 1460) instead of the measured value. |
| `mesh.control_port` | `51821` | UDP and TCP port of the daemons on the site addresses. |
| `mesh.revision` | `0` | Revision counter of the sync; raised by the daemon. |
| `site.name` | | Unique; `a-z`, `0-9` and `-`, at most 12 characters (it is part of the interface name `gt-<name>`). |
| `site.address` | | Unique IPv4 address of the site in the transport network. |
| `site.vlans` | `mesh.vlans` | VLANs this site takes part in. |
| `site.uplink` | none | The site this one is a client of; none: a server. `""` marks a server explicitly. |
| `site.online` | `true` | `false`: the tunnels of the site stay down. Sites that join start with `false`. |
| `site.managed_by` | none | The site that manages this one (the inviting site). |
| `site.self_managed` | `false` | The site may also change its own VLANs, mappings and online state. |
| `map.site` | | The site the mapping belongs to. |
| `map.local` | | `"native"` (untagged) or a local VLAN ID (not 1). |
| `map.shared` | | The VLAN in the network; one of the VLANs of the site, not 1. |
| `self` (local) | | Which site of `mesh.toml` this host is. |
| `trunk` (local) | | The interface carrying the VLANs. |
| `tunnels.<site>.trunk` (local) | `trunk` | A trunk of its own for the tunnel to `<site>`. |

Every file is checked when it is loaded or applied; problems are reported
with file and line:

```
mesh.toml:24: error: site "office": vlans: VLAN ID 5000 is outside 1-4094
mesh.toml:42: error: map #2: shared VLAN 60 is not one of the VLANs of site "office" (10,20)
```

Names and addresses are unique, uplinks lead to an existing site and never
in a circle, mappings use VLANs of their site (at most one native mapping
per site, no local VLAN mapped twice, no two local VLANs onto one shared
VLAN), `self` exists, and unknown keys are errors (typos). Older files load
too: `role` and `hub` (wanderlan 0.1) are ignored with a warning, and a site
with `managed_by` but without `uplink` is a client of its manager.

## How it works

### On each site

The daemon creates, and removes again when it stops:

* `br0`: a bridge with VLAN filtering, STP off, no default VLAN and no
  netfilter calls, with the trunk as a port;
* `wl-<interface>`: the same for every further trunk ("trunk per tunnel");
* `gt-<peer>`: a GRETAP tunnel (`ignore-df`, `nopmtudisc`, MTU 1500) to its
  server, to each of its clients and, on a server, to every other server. The
  ports between servers are `isolated` (split horizon: a frame from one
  server is never forwarded to another server); the ports of clients and
  uplinks are ordinary ports, a tree that is loop-free by itself;
* the VLANs of the site on the trunk (VLAN 1 untagged as the native VLAN) and
  on every tunnel the VLANs both ends take part in;
* tc `clsact` filters on the trunk for translations (recognised by their
  action cookie `wl:…`);
* the nftables table `bridge wanderlan` for MSS clamping.

Everything it creates carries the interface alias `wanderlan`; foreign
interfaces are never touched (a foreign `br0` or `gt-*` stops the
configuration with an error). After a restart it adopts what is there
without changes. After a topology change it flushes the learned MAC
addresses, again after 10 s and 30 s while the other sites follow.

### Synchronisation

The daemons talk to their tunnel neighbours over UDP on the control port
every 5 s: reachability, round trip, path MTU and the revision and
fingerprint of their configuration. Each neighbour is told the version it
should have: servers and the uplink the whole configuration of this site, a
client only its own part. A site that sees a newer version fetches it over
TCP and applies it; a server that sees changes of one of its clients fetches
the client's part and takes over what the client is responsible for (itself,
its own clients, the network settings), checked against its rights. Changes
received this way are confirmed automatically as soon as a neighbour answers
with them in effect.

A change that is based on an outdated revision is refused ("wait for the
sync, reload and try again"), so concurrent edits do not silently overwrite
each other. `systemctl reload wanderland` (SIGHUP) takes the files as they
are, without raising the revision.

### MTU and MSS

The daemon measures the path MTU to every neighbour by binary search with UDP
probes that have the DF bit set: at start, every `mtu_probe_interval` and
when a neighbour becomes reachable again. TCP SYNs entering or leaving a
tunnel port are clamped to

`MSS = path MTU − 20 (outer IP) − 4 (GRE) − 18 (Ethernet + VLAN tag) − 40 (inner IP + TCP)`

(IPv6: 20 bytes less; ports without a measurement use the smallest measured
value). GRE fragmentation stays on as a safety net, so non-TCP packets larger
than the path still arrive.

### Loops

The network itself is loop-free: split horizon between the servers, a tree
below them. The LANs of two sites may still be connected by another path.
The loop guard watches the forwarding database of every bridge: if a MAC
address keeps moving between the trunk and a tunnel, or between two tunnels
(5 times within 10 s), it shuts the tunnel port down (never the trunk),
logs it and shows it in the TUI until `wanderlan unblock gt-<site>` (`u` in
the TUI) or the configuration changes.

### Operation

`wanderland` runs as the systemd service `wanderland`, as root with
`CAP_NET_ADMIN` as its only capability and a hardened unit; systemd restarts
it after a crash, and the network keeps running meanwhile. Only one daemon
runs at a time: `wanderland` started from a shell only explains that it is
the daemon. Logs go to the journal (`journalctl -u wanderland`) and to the
event log (`wanderlan log`).

## Terminal UI

`wanderlan` without arguments starts the TUI: keyboard first, works over SSH
and in 80×24 (`--mouse` enables the mouse). `?` shows all keys.

| Key | View |
|-----|------|
| `1` | Overview: this site with its role, the tunnel neighbours live (reply, round trip, path MTU, MSS, port state, MAC addresses, throughput, config in sync), traffic of the tunnels and of every trunk, and all known sites with role and link (`via hq` for sites without a tunnel from here). `Enter` on a neighbour: traffic graph, round-trip history, min/avg/max round trip, jitter, probe loss, latency, counters. |
| `2` | Sites: name, address, VLANs, role, manager, tunnel. Invite (`i`), open invitations (`I`), online/offline (`o`), new (`n`), edit (`Enter`: VLANs, "Changes", "Connected to"), delete (`d`). |
| `3` | VLANs: the VLANs of the network (`v`) and the mappings (`n`, `Enter`, `d`) with their site and its role; conflicts are marked right away. |
| `4` | MAC addresses: filter (`/`), sort (`s`, `o`). |
| `5` | Log: live events, filter by level (`l`) and site (`p`). |
| `6` | Setup: new network or join; on a configured site invite (`i`) and reset (`R`). |
| `7` | Settings: every setting with an explanation, the trunk of every tunnel, and the role and rights of this site. |

Changes are collected in a draft. `a` shows the difference to the running
configuration and applies it; a banner counts down until it is confirmed
(`c`) or rolled back (`r`). `x` discards the draft, `q` quits. When the
daemon is not reachable, the TUI shows the state of the service and its last
log lines and keeps trying.

## Command line

| Command | Purpose |
|---------|---------|
| `wanderlan status [--json]` | This site, all known sites with role and link, the tunnel neighbours |
| `wanderlan log [-f]` | Event log |
| `wanderlan invite [--name N] [--vlans V] [--native VID] [--map L:S]... [--self-managed]` | Create an invitation with rules; prints `wanderlan join <address> <code>` |
| `wanderlan invite list` / `wanderlan invite delete <code>` | Open invitations of this site |
| `wanderlan join <address>[:port] <code> [--name N] [--trunk IF]` | Join a network (on an unconfigured site) |
| `wanderlan online <site>` / `wanderlan offline <site>` | Take a site online or offline |
| `wanderlan manage <site> [--by S \| --anyone] [--self-managed yes\|no]` | Who may change a site |
| `wanderlan role <site> server\|client <server>` | Make a site a server or a client |
| `wanderlan trunk <site> <interface\|default>` | Trunk of the tunnel to a site |
| `wanderlan apply [--mesh F] [--local F] [--timeout 60s]` | Apply files (default: those in `/etc/wanderlan`) |
| `wanderlan confirm` / `wanderlan rollback` | Confirm or revert the pending change |
| `wanderlan validate <mesh.toml> [--local <local.toml>]` | Check files without applying them |
| `wanderlan init [--dir D]` / `wanderlan export` | Print or write the example configuration / print the running `mesh.toml` |
| `wanderlan probe` / `wanderlan unblock <port>` | Measure the path MTU now / re-enable a port the loop guard shut down |
| `wanderlan reset [--yes]` | Remove everything wanderlan set up on this site |

Commands that change the configuration (`online`, `offline`, `manage`,
`role`, `trunk`) take `--confirm` to confirm right away; without it, run
`wanderlan confirm` within the commit timeout. They wait for a change that is
still being synchronised. The daemon's socket can be chosen with `--socket`
or `WANDERLAN_SOCKET`.

## Troubleshooting

* **The TUI says the daemon is not reachable**: it shows the state of the
  service and its last log lines; more with `systemctl status wanderland` and
  `journalctl -u wanderland -e`.
* **Typed `wanderland` instead of `wanderlan`**: harmless; the daemon only
  explains what it is and never runs twice.
* **Permission denied on the socket**: join the group `wanderlan` and log in
  again, or use `sudo`.
* **"trunk interface … is already attached to vmbr0"**: the trunk must not be
  part of another bridge or bond (on Proxmox: a dedicated NIC for the VM).
* **"interface br0 already exists and was not created by wanderlan"**: remove
  or rename it; wanderlan does not touch foreign interfaces.
* **A new site gets no traffic**: it starts offline; take it online on its
  server (Sites, `o`).
* **Tunnel up but no traffic in a VLAN**: the VLAN must be one of the VLANs of
  both ends (`status` shows the VLANs per tunnel; for two clients also their
  server's), and the switch port or VM must carry it tagged. GRE (IP protocol
  47) must pass between the site addresses in both directions:
  `tcpdump -ni any proto gre`.
* **Untagged clients get nothing (e.g. no DHCP)**: the site must take part in
  `native` (the default "all" does), or put its untagged traffic into a VLAN
  with a mapping. On Proxmox, switch off the firewall (or its MAC filter) on
  the trunk NIC of the wanderlan VM: it drops frames with foreign source MAC
  addresses, which is all bridged traffic.
* **`wanderlan join` fails**: the code is used up or wrong, or TCP on the
  control port of the inviting site is not reachable. A site that does not
  come up within the commit timeout is removed again; its code works again.
* **A neighbour "does not answer control probes"**: allow UDP and TCP on the
  control port between the site addresses.
* **Large transfers stall**: compare the path MTU in `wanderlan status` with
  the MTU of the VPN; set `mss` if probes are filtered.
* **A port is BLOCKED**: the loop guard found a loop through another layer-2
  path between sites; remove it, then `wanderlan unblock gt-<site>`.
* **A change was rolled back**: confirm within the commit timeout (`c`,
  `wanderlan confirm`).
* **A change did not arrive at a site**: `wanderlan log` there and on its
  server shows why (e.g. not allowed for that site, or invalid there).

## Project decisions

* **No VPN inside wanderlan.** Transport and encryption belong to the existing
  VPN; wanderlan only does layer 2 and VLANs. Sites are identified by their
  transport address, and GRE runs unencrypted on top of that trusted network.
* **Servers and clients, like plugging switches together.** Roles are not
  configured up front: a site that joins is a client of the site that invited
  it and only has to reach it; servers form a mesh with split horizon. There
  are no tunnels between clients; the VLANs decide who meets whom.
* **Clients only know their part.** A client's configuration contains its
  server and its own clients only; servers merge the changes of their clients.
* **Who may change a site**: its manager (the inviting site), the site itself
  only if released. The daemons enforce this among themselves; someone with
  root on a site could bypass it.
* **New sites start offline**, so a site is checked before its traffic flows.
* **Native = VLAN 1 between the sites**, like the default native VLAN of
  switches; an empty VLAN list means all VLANs plus native.
* **Joining with invitation codes**: the inviting site adds the new site with
  the address the request came from, so no address has to be typed. Codes
  work once and do not expire; they travel in clear text through the trusted
  transport network.
* **Changes must be confirmed**, so nobody locks themselves out through the
  tunnel they are changing.
* **UDP probes instead of ICMP** for reachability and path MTU (DF set via
  `IP_PMTUDISC_PROBE`): no `CAP_NET_RAW`, and the same probes carry the sync
  announcements.
* **MSS clamping on entering and leaving SYNs** per tunnel port, so both
  directions get the smaller value.
* **netlink everywhere except nftables**: links, bridges, VLANs, GRETAP and tc
  use rtnetlink; nftables is driven through its JSON interface (the
  `nftables` crate, hence the dependency on the `nftables` package; a test
  guards a workaround for swapped `<`/`>` in its 0.6.3 release).
* **Fixed interface names** `br0`, `wl-<interface>` and `gt-<site>`; site
  names have at most 12 characters so that `gt-<site>` fits.

Open questions are collected in [docs/OPEN-QUESTIONS.md](docs/OPEN-QUESTIONS.md).

## Development

Everything builds and runs in Docker; nothing else is needed on the
development machine.

```sh
scripts/check.sh              # cargo fmt --check, clippy -D warnings, unit and snapshot tests
scripts/cargo.sh test         # any cargo command in the build container
scripts/build.sh amd64        # static binaries and .deb into dist/ (also arm64)
scripts/integration.sh        # the .deb in Debian 13 containers: five sites, a router, clients
scripts/systemd-test.sh       # the daemon under systemd with the hardened unit
```

`scripts/integration.sh` loads the needed kernel modules into the host kernel
through a privileged helper container and runs the scenarios in
`tests/integration/cases` (see `tests/integration/README.md`).
`scripts/systemd-test.sh` boots two Debian 13 containers with systemd as
PID 1 and goes through what users do: a new network, an invitation, a join, a
mistyped `wanderland`, a deleted socket, a reset, a new network while the old
one still lists the site, a crash, restart and stop.

The workspace has three crates: `wanderlan-core` (configuration, topology,
views, rights, desired state, reconciliation, system access behind traits
with a netlink and an in-memory implementation), `wanderland` (the daemon)
and `wanderlan` (TUI and CLI).

## Maintainers: releases and the APT repository

Publishing is automatic: every push to `main` whose CI run is green (fmt,
clippy, tests, packages for amd64 and arm64 with a smoke test, integration
tests, systemd test) is published by `publish.yml`:

* as version `<Cargo.toml version>+<CI run>.g<commit>`, package version with
  `-1`, so every build is newer than the one before;
* into the signed repository on the branch `gh-pages` (one commit without
  history), suite `stable`; the newest 20 builds per architecture stay,
  tagged releases forever;
* `verify-apt.yml` then installs it from GitHub Pages with the commands above
  on Debian 13, Debian 12 and Ubuntu 24.04, also exactly this version and a
  downgrade, and runs two hosts with an invitation.

A tag `v<version>` (equal to the version in `Cargo.toml`) additionally makes
`release.yml` create a GitHub release with binaries, packages and
`SHA256SUMS` and keeps `<version>-1` in the repository forever. To start a
new version line, raise the version in `Cargo.toml`.

One-time setup (done for this repository):

1. Create the signing key (RSA 4096) outside the repository with
   `scripts/gen-apt-key.sh ~/wanderlan-apt-key` and commit its public part
   as `key.gpg`. Current fingerprint:
   `F6C4 00CF 318B FD47 E3A6  B8A7 7BC2 D7AE 6699 82D4`.
2. Store `private.asc` as the repository secret `APT_GPG_PRIVATE_KEY` and
   keep a backup offline; without it a publication stops and changes nothing.
3. GitHub Pages: "Deploy from a branch", branch `gh-pages`, folder `/`.

Locally: `scripts/build.sh` (with `WANDERLAN_VERSION=…` for a specific
version), `scripts/smoke-test.sh dist <package version>`,
`scripts/test-apt-repo.sh` (the repository script with a throwaway key).
