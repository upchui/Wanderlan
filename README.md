# Wanderlan

**Your network at another site, as if you had pulled a long network cable
there, only over the internet.**

Say the main office (`hq`), or your home, has a firewall at 10.0.0.1 with
the network 10.0.0.0/24, its DHCP server, the NAS and the printers. Now a
second office, a second apartment or a holiday house (`office`) should simply be
part of that network, without building a network of its own there. With
wanderlan, a small Linux box at each site does that: the devices plugged
into the switch at `office` get their address from the DHCP server at `hq`,
use 10.0.0.1 as their gateway and find the NAS and the printers as if they
stood next to them.

```
 hq (main office)                                          office (second site)

 firewall 10.0.0.1                                         laptop  10.0.0.57
 DHCP, NAS 10.0.0.20                                       printer 10.0.0.58
     │                                                       │
   switch ─ ens19 [box] ══ internet / VPN ══ [box] ens19 ─ switch
     │                                                       │
     └────────────── one network: 10.0.0.0/24 ───────────────┘
```

## What it is for

Typical uses:

* **A second office in the network of the main office.** The same
  addresses, the same file server and printers, behind the same firewall.
* **A second apartment or house in your home network.** You have set up the
  network at home just right: VLANs for the family, guests and the smart
  home devices, a DNS filter, the NAS, the firewall rules. In a second
  apartment, a holiday home or at your parents' place you want exactly that
  network, not a second one to set up and keep in step: put the box there,
  and every device on the switch or the Wi-Fi access point behind it is in
  your home network, with the same VLANs (the access point's SSIDs on them),
  DNS and smart home.

What that means:

* **One network, not two.** The second site is not a routed network of its
  own but the same one: broadcasts and discovery (DHCP, mDNS, printers,
  NetBIOS) work like on one switch.
* **Nothing to build at the second site.** No second address range, no
  routes, no NAT, no DHCP server of its own: plug in the box, done.
* **Like a cable to the first site.** The devices at the second site go to
  the internet through the firewall of the first site, and its rules apply
  to them too.
* **VLANs come along.** If the network has VLANs (staff, guests, phones), all
  of them reach the other site, or only the ones you choose per site; a VLAN
  can also arrive untagged or with another ID there.
* **More sites.** A third and fourth site join the same way.

**What you need:**

* At each site a small Linux host (Debian 13 or 12, Ubuntu 24.04): a mini PC
  with two network ports, a Raspberry Pi with a USB network adapter, or a VM
  on Proxmox with two network interfaces. One interface reaches the other
  site; the other one (the **trunk**) goes to the local switch.
* A connection between the sites. wanderlan does **not** build a VPN: the
  boxes must already reach each other, usually through a VPN such as
  WireGuard, Tailscale or the site-to-site VPN of the firewall. The VPN also
  encrypts the traffic; wanderlan's tunnels (GRE) are not encrypted. GRE and
  UDP/TCP port 51821 must pass through it.

Under the hood each site runs a small daemon (`wanderland`) with a
VLAN-aware Linux bridge and GRETAP tunnels to the other sites, and a terminal
UI (`wanderlan`) to set it up and watch it; see [Concepts](#concepts) and
[How it works](#how-it-works).

## Contents

- [What it is for](#what-it-is-for)
- [Requirements](#requirements)
- [Installation](#installation)
- [Quick start: a second site in your network](#quick-start-a-second-site-in-your-network)
- [Concepts](#concepts)
- [Everyday tasks](#everyday-tasks)
- [Configuration](#configuration)
- [How it works](#how-it-works)
- [Terminal UI](#terminal-ui)
- [Command line](#command-line)
- [Troubleshooting](#troubleshooting)
- [Project decisions](#project-decisions)
- [Development](#development)
- [Maintainers: releases and the APT repository](#maintainers-releases-and-the-apt-repository)

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
  service loads them when it starts).

## Installation

```
curl -fsSL https://upchui.github.io/Wanderlan/key.gpg | sudo gpg --dearmor -o /usr/share/keyrings/wanderlan.gpg
echo "deb [signed-by=/usr/share/keyrings/wanderlan.gpg] https://upchui.github.io/Wanderlan/apt stable main" | sudo tee /etc/apt/sources.list.d/wanderlan.list
sudo apt update && sudo apt install wanderlan
```

(On minimal installations first `sudo apt install curl gpg`.)

Every commit on `main` that passes all tests is published with a version of
its own: 1.0.0, 1.0.1, 1.0.2 and so on; the package is named exactly like
the version, and `wanderlan --version` also names the commit
(`wanderlan 1.0.2 (c463aa1)`), so
`sudo apt upgrade` always brings the newest one. The last 20 versions and
every tagged release stay in the repository: `apt-cache madison wanderlan`
lists them, `sudo apt install wanderlan=<version>` goes back. Update all
sites to the same version. Older builds kept the shared configuration under another file
name; the first start of a new build moves it to `network.toml`.

The package installs the daemon `wanderland` (systemd service, enabled and
started), the terminal UI and command line `wanderlan`, empty configuration
files in `/etc/wanderlan/`, examples in `/usr/share/doc/wanderlan/examples/`
and the group `wanderlan`. Its members can use `wanderlan` without root:

```
sudo usermod -aG wanderlan $USER    # then log in again
```

The system stays clean: when the service stops, for whatever reason (a
stop, an update, a crash), everything wanderlan set up is removed, and a
start builds it anew; an update restarts the service. `sudo apt remove
wanderlan` removes everything as well and keeps the configuration for a
later installation; `sudo apt purge wanderlan` removes that too.

## Quick start: a second site in your network

The example from the top: `hq` (main office, firewall 10.0.0.1, network
10.0.0.0/24) and `office` (the second site). Each has a box with `ens18`
towards the VPN (VPN addresses `10.8.0.1` at `hq` and `10.8.0.2` at
`office`) and `ens19` to the local switch.

1. [Install](#installation) wanderlan on both boxes.
2. On **`hq`** run `sudo wanderlan`. The setup wizard opens; choose **Start a
   new network**:
   * name `hq` (suggested from the host name),
   * trunk `ens19`: the port into 10.0.0.0/24 (an untagged switch port, or a
     trunk port if the network has VLANs),
   * address `10.8.0.1`, its address in the VPN,
   * VLANs empty: everything, the untagged 10.0.0.0/24 included.

   `Enter` shows the change, `c` confirms it.
3. The invitation for the next site opens: name `office`, `Enter`. It shows
   the command for the new site, e.g. `wanderlan join 10.8.0.1 K7F3Q-9QX2M`
   (later: Sites, `i`).
4. On **`office`** run it:

   ```
   sudo wanderlan join 10.8.0.1 K7F3Q-9QX2M
   ```

   The box takes its name from the host name (`--name` to choose one) and
   its trunk itself (`--trunk ens19` if there are several candidates), gets
   its part of the configuration and reports when both sides agree. In the
   TUI the wizard offers the same under **Join an existing network**.
5. Back on **`hq`**, the overview shows `office` reachable, with round trip
   and path MTU. A new site starts offline; take it online:

   ```
   sudo wanderlan online office --confirm
   ```

   (or in the TUI: Sites `2`, select it, `o`, `a`, `c`).
6. Plug a laptop into the switch at `office`: it gets a 10.0.0.x address
   from the firewall at `hq`, and `ping 10.0.0.1` answers. In the TUI,
   `Enter` on `office` in the overview shows the tunnel, its traffic and the
   traffic of every VLAN.

### Good to know

* The boxes need no address in 10.0.0.0/24; their own addresses stay in the
  VPN (and their own LAN, if any).
* Only part of the network at a site: give the invitation its VLANs
  (`wanderlan invite --name office --vlans 10,20`), put the untagged traffic
  of the site into a VLAN (`--native 30`) or map VLAN IDs (`--map 130:30`:
  VLAN 130 there is source VLAN 30); see [Everyday tasks](#everyday-tasks).
* No DHCP server of its own on the switch at the second site, otherwise the
  one network has two.
* Do not connect the two networks a second way as well (another VPN bridge,
  a cable): that is a loop. The loop guard then shuts the tunnel down until
  it is fixed.
* Speed and latency are those of the connection between the sites. Path MTU
  and TCP MSS are measured and set automatically.

## Concepts

For two sites the example above is all there is. With more sites, wanderlan
works like plugging switches together: one site is the **server**, every
further site is a **client** plugged into it with an invitation code, and for
every site you decide which VLANs it gets, which VLAN its untagged traffic
belongs to and which VLAN IDs are translated.

```
                  hq   (server, VLANs: all)
                 /    \
  office (client of hq)   lab (client of hq)
  VLANs 10, 20            untagged traffic = VLAN 30
```

`office` and `lab` each have one tunnel, to `hq`. They have no tunnel to
each other and do not even know about each other; whether traffic flows
between two sites is decided by the VLANs alone. Gateways and DHCP servers
of the network stay where they are; wanderlan does not route or hand out
addresses.

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

A single tunnel can be limited further (`[[tunnel]]`): e.g. `dc – hq` carries
only 10-20 and `dc – lab` only 30-40, although all three take part in more.
Together with a trunk of its own for a tunnel, the same VLAN IDs on another
interface of a site are a network of their own and still reach another site.

A server passes a VLAN on between two other servers when it is the only way
between them for that VLAN: if VLAN 30 is only on `hq – dc` and `dc – lab`,
`hq` and `lab` meet through `dc`. Where every pair has its own tunnel with
the VLAN, it goes directly (split horizon). If the tunnels with a VLAN form a
ring without every direct tunnel (`a – b – c – d – a`), nobody passes it on
inside the ring, so `a` and `c` do not meet in it; `wanderlan validate` and
the TUI warn about that. Both rules together keep the network free of loops.

Besides the VLANs its server gives it, a site can have **own VLANs**: they
are on its trunk and go to its own clients, but never up to its server. So a
client can build its own VLANs below it without touching what its server
decided.

### VLAN mappings

A mapping gives a VLAN of the network, the **source VLAN**, another name on
one site's trunk, the **mapped VLAN**:

* `source VLAN 30 = untagged at lab`: what arrives untagged at lab's trunk is
  VLAN 30 everywhere else, and VLAN 30 leaves lab's trunk untagged;
* `source VLAN 30 = VLAN 130 at lab`: VLAN 130 at lab is VLAN 30 everywhere
  else.

The VLAN mapping views (Site, Source VLAN, Mapped VLAN) show next to each
site whether it is a server or a client. In `network.toml` a mapping keeps
its keys: `shared` is the source VLAN, `local` the mapped VLAN.

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
the VLANs its server gave it, only those of sites it invited itself. Its own
VLANs (see above) are always its own business. A client may in any case only
change itself and its own clients, never its server. The daemons enforce
this among themselves; it is not a cryptographic protection.

### Online, offline and confirmed changes

A site that joins starts **offline**: its tunnel exists but stays down, while
the control channel already shows whether it is reachable, its round trip and
path MTU. Once everything is right, its manager takes it online; online stays
online, also across restarts, until someone takes it offline.

Every change is applied immediately and must be **confirmed** within the
commit timeout (60 s by default); otherwise the previous state comes back.
So a change that cuts you off undoes itself.

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

**Give a site VLANs of its own.** VLANs (`3`), `o`: VLANs on this site's
trunk and towards its own clients, never to its server; possible even when
the server manages everything else. CLI: `wanderlan own-vlans <site> 50,60`
(`none` removes them).

**Put untagged traffic into a VLAN, translate a VLAN.** VLANs (`3`), `n`:
site, source VLAN (e.g. `30`), mapped VLAN `native` (or the VLAN ID on that
site's trunk, e.g. `130`).

**Let a site change its own VLANs, move it.** Sites, `Enter` on the site:
"Changes" (only its manager / the manager and the site / any site) and
"Connected to" (server, or client of a site). CLI:
`wanderlan manage <site> --self-managed yes`,
`wanderlan role <site> client <server>`, `wanderlan role <site> server`.

**Limit what one tunnel carries.** VLANs (`3`), `t` lists every tunnel with
what it carries; `Enter` limits it (e.g. `30-40`, or `none`), `d` lifts the
limit. CLI: `wanderlan tunnel-vlans dc lab 30-40` (`all` lifts it). Whoever may
change one of the two sites may limit their tunnel.

**Use several trunk ports.** The trunk can be several local interfaces;
they are bridged like the ports of a switch and all carry the same VLANs and
mappings (the first is the main port). Settings (`7`), "Trunk", lists the
interfaces of the host: `Space` chooses one or more, `m` makes one the main
port, a name that is not there yet goes in the last row; interfaces that
cannot be a trunk port are grey with the reason (in a bridge of someone
else). A port of another segment moves here when chosen: that segment keeps
the rest, a tunnel left without ports uses the default trunk, and the window
says what changes before it is saved (the same in `wanderlan trunk`). An interface the tunnels run over is marked yellow with
their peers: as the kernel routes the tunnel packets, so a VPN with its own
routing rules (Tailscale, WireGuard) counts. It can be chosen, but bridged
it no longer carries this host's own traffic: if the tunnels drop, do not
confirm and the change is rolled back by itself (`--confirm` of the CLI
waits in that case too). Or `wanderlan trunk <this site> ens19,ens20`. A missing
further port (e.g. an unplugged adapter) is shown and left out; a loop
between two ports shuts down the further one, never the main port.

**Give a tunnel its own trunk.** Normally all tunnels share the trunk. A
tunnel can be bridged with other local interfaces instead, as a segment of
its own at this site (bridge `wl-<first interface>`, same VLANs and
mappings): only the network on those interfaces reaches the other site
through this one. So a server says for each site exactly which ports carry
it, and a client which ports are its trunk. Settings (`7`), "Trunk per
tunnel" (the same list of interfaces; nothing chosen is the default trunk),
or `wanderlan trunk <site> ens20` / `ens20,ens21` (`default` to undo).

```
hq
  ens19 ─┐
  ens22 ─┴ br0 ───────┬── gt-office ══ office
                      └── gt-lab    ══ lab
  ens20 ── wl-ens20 ───── gt-dc     ══ dc
```

**Watch the traffic of one VLAN.** Overview (`1`), `Enter` on a neighbour:
beside its traffic graph the VLANs of the tunnel with what comes in and goes
out now, the busiest first. `Up`/`Down` chooses one, `Enter` opens it: every
port of this site with the VLAN (tunnels and trunk ports), in and out now,
bytes and packets, the MAC addresses behind each port, a graph, where the
VLAN is on the trunk (mapped or not), which tunnels carry it and where this
site passes it on. `m` shows its MAC table. CLI: `wanderlan vlan 20`.

**Start over on a site.** Setup (`6`), `R`, or `sudo wanderlan reset`: the
bridges, tunnels, all rules, the configuration and the invitations are
removed and the site is unconfigured again, like after the installation
(also the kernel's `gre0`/`gretap0`/`erspan0` when it runs as root outside a
container and no other GRE tunnel needs them). Join again with a new
invitation; a client that joins through another site becomes that site's
client. If the site leaves for good, delete it on its server (Sites, `d`).

**Edit the files by hand.** Edit `/etc/wanderlan/network.toml` or `local.toml`,
check with `wanderlan validate /etc/wanderlan/network.toml --local
/etc/wanderlan/local.toml`, then `sudo wanderlan apply` and
`sudo wanderlan confirm`. Edits that are not confirmed are restored and kept
as `*.rejected`.

## Configuration

Two files in `/etc/wanderlan/`, both maintained by the daemon:

* `network.toml`: the network. A server holds the whole network; a client only
  its part of it (itself, its server, its own clients). The daemons keep the
  files in sync.
* `local.toml`: this host only, never synchronised.

`local.toml`:

```toml
self = "hq"         # which site of network.toml this host is
trunk = "ens19"     # the interface carrying the VLANs (several: ["ens19", "ens22"])

# Optional: the tunnel to dc uses a trunk of its own (a segment of its own).
[tunnels.dc]
trunk = "ens20"
```

`network.toml` on `hq` (a server):

```toml
[network]
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

The `network.toml` of `lab` contains only `hq` (as its server), `lab` and lab's
mapping.

### Settings

| Key | Default | Meaning |
|-----|---------|---------|
| `network.vlans` | all | VLANs of every site without its own list. Missing, empty or `"all"`: every VLAN and the untagged traffic. Otherwise e.g. `"native,10-50"`, `"10,20,30-40"` or `[10, "20-30"]`. |
| `network.commit_timeout` | `60s` | Time to confirm a change (10 s to 1 h). |
| `network.mtu_probe_interval` | `5m` | Interval of the path MTU measurement (at least 10 s). |
| `network.mss` | unset | Fixed TCP MSS (536 to 1460) instead of the measured value. |
| `network.control_port` | `51821` | UDP and TCP port of the daemons on the site addresses. |
| `network.revision` | `0` | Revision counter of the sync; raised by the daemon. |
| `site.name` | | Unique; `a-z`, `0-9` and `-`, at most 12 characters (it is part of the interface name `gt-<name>`). |
| `site.address` | | Unique IPv4 address of the site in the transport network. |
| `site.vlans` | `network.vlans` | VLANs this site takes part in. |
| `site.own_vlans` | none | The site's own VLANs: on its trunk and towards its own clients, never towards its server. The site itself may always change them. |
| `site.uplink` | none | The site this one is a client of; none: a server. `""` marks a server explicitly. |
| `site.online` | `true` | `false`: the tunnels of the site stay down. Sites that join start with `false`. |
| `site.managed_by` | none | The site that manages this one (the inviting site). |
| `site.self_managed` | `false` | The site may also change its own VLANs, mappings and online state. |
| `map.site` | | The site the mapping belongs to. |
| `map.local` | | The mapped VLAN: `"native"` (untagged) or the VLAN ID on the site's trunk (not 1). |
| `map.shared` | | The source VLAN: the VLAN in the network; one of the VLANs of the site, not 1. |
| `tunnel.between` | | The two sites of a tunnel, e.g. `["dc", "lab"]`. |
| `tunnel.vlans` | | The most this tunnel carries (on top of the VLANs of both sites), e.g. `"30-40"`; `"none"`: no VLAN at all. |
| `self` (local) | | Which site of `network.toml` this host is. |
| `trunk` (local) | | The interface carrying the VLANs, or several (`["ens19", "ens22"]`, bridged like switch ports, the first is the main port). |
| `tunnels.<site>.trunk` (local) | `trunk` | A trunk of its own for the tunnel to `<site>`: one interface or several. A port belongs to one segment. |

Every file is checked when it is loaded or applied; problems are reported
with file and line:

```
network.toml:24: error: site "office": vlans: VLAN ID 5000 is outside 1-4094
network.toml:42: error: map #2: source VLAN 60 is not one of the VLANs of site "office" (10,20)
```

Names and addresses are unique, uplinks lead to an existing site and never
in a circle, mappings use VLANs of their site (at most one native mapping
per site, no mapped VLAN used twice, no source VLAN mapped twice per site),
`self` exists, and unknown keys are errors (typos). Older files load
too: `role` and `hub` (wanderlan 0.1) are ignored with a warning, and a site
with `managed_by` but without `uplink` is a client of its manager.

## How it works

### On each site

The daemon creates, and removes again whenever the service ends (a stop,
an update, a crash or a kill: systemd then runs `wanderland --cleanup`, and
once more before every start in case that could not run; it also unloads
the kernel module `ip_gre` and with it its devices `gre0`, `gretap0` and
`erspan0`, unless another GRE tunnel uses it or this is a container):

* `br0`: a bridge with VLAN filtering, STP off, no default VLAN and no
  netfilter calls of its own (if `br_netfilter` is loaded, e.g. by Docker,
  with `net.bridge.bridge-nf-call-iptables = 1`, untagged frames still pass
  iptables; the daemon warns about it), counting the traffic per VLAN and port
  (`vlan_stats_per_port`), with the trunk as a port. The kernel takes the
  counters only from a new bridge, so a bridge of an older version is built
  anew once (a short interruption); on a kernel without them the bridge
  stays as it is and the traffic per VLAN is not shown;
* `wl-<interface>`: the same for every further trunk ("trunk per tunnel");
* `gt-<peer>`: a GRETAP tunnel (`ignore-df`, `nopmtudisc`, MTU 1500) to its
  server, to each of its clients and, on a server, to every other server. The
  ports between servers are `isolated` (split horizon: a frame from one
  server is never forwarded to another server); the ports of clients and
  uplinks are ordinary ports, a tree that is loop-free by itself;
* the VLANs of the site on the trunk (VLAN 1 untagged as the native VLAN) and
  on every tunnel the VLANs both ends take part in;
* tc `clsact` filters on the trunk for translations (recognised by their
  action cookie `wl:…`), all in one flower priority per direction
  (`0xc000`, handle = VLAN ID), so a frame costs one lookup however many
  translations there are;
* the nftables table `bridge wanderlan` for MSS clamping: every bridged
  frame of the host passes one rule there, only TCP SYNs go on to the MSS
  rules (chain `wl-mss`).

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
with them in effect. A newer revision with the same content (a change
elsewhere in the network that does not concern this site) is taken over
without applying anything.

If a server cannot take over a change of its client (e.g. a new client with a
name that is already used in the network), it marks the client as `refused`
and tells it why. After 15 s the client takes the server's configuration
back and shows the reason in its overview and in `wanderlan status`. A
refused configuration is not fetched again for 10 minutes.

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
below them. With limits per tunnel the rule is the same per VLAN: the
tunnels that carry it fall into blocks (parts in which every two tunnels
lie on a common cycle); a site passes a VLAN on only between tunnels of
different blocks, so a frame crosses each block in one hop and never comes
back. Usually the kernel's `isolated` port flag does this; a site that
passes VLANs on between servers uses nftables rules (`wl split` in table
`bridge wanderlan`, checked on every reconcile) and only then drops
`isolated`. The LANs of two sites may still be connected by another path.
The loop guard watches the forwarding database of every bridge: if a MAC
address keeps moving between the trunk and a tunnel, or between two tunnels
(5 times within 10 s), it shuts the tunnel port down (never the trunk),
logs it and shows it in the TUI until `wanderlan unblock gt-<site>` (`u` in
the TUI) or the configuration changes.

### Operation

`wanderland` runs as the systemd service `wanderland`, as root with
`CAP_NET_ADMIN` as its only capability and a hardened unit. systemd restarts
it after a crash: the cleanup removes what the crashed daemon left, the new
one builds the network again within seconds. An update restarts it the
same way. Only one daemon
runs at a time: `wanderland` started from a shell only explains that it is
the daemon. Logs go to the journal (`journalctl -u wanderland`) and to the
event log (`wanderlan log`).

## Terminal UI

`wanderlan` without arguments starts the TUI: keyboard first, works over SSH
and needs at least 80×24 (larger terminals show more). `?` shows all keys,
the bar at the bottom the most important ones of each view. Every list
scrolls with Up/Down, PgUp/PgDn, Home and End and shows a scrollbar when it
holds more than fits; `/` searches in Sites, VLANs, MACs and Log. `--mouse`
enables the mouse: a click selects a row (a second click opens it), the
wheel scrolls the list under the pointer, the keys in the bottom bar can be
clicked.

| Key | View |
|-----|------|
| `1` | Overview: this site with its role, the tunnel neighbours live (reply, round trip, path MTU, MSS, port state, MAC addresses, throughput, config in sync or `refused`), traffic of the tunnels and of every trunk, and all known sites with role and link (`via hq` for sites without a tunnel from here), each server with its clients indented below. One cursor runs through the neighbours and the sites. `Enter` on a neighbour: traffic graph, the VLANs of the tunnel with their live traffic (Up/Down, `Enter` opens one: every port, MACs, graph; `m` its MAC table), round-trip history, min/avg/max round trip, jitter, probe loss, latency, counters (Left/Right: the next neighbour); on a site: its details. |
| `2` | Sites: name, address, VLANs, role, manager, tunnel; clients indented below their server. `Enter`: details (role, VLANs, every tunnel with what it carries, mappings, who may change it; Up/Down: the next site). Invite (`i`), open invitations (`I`), online/offline (`o`), new (`n`), edit (`e`: VLANs, "Changes", "Connected to"), delete (`d`), network settings (`m`), trunk ports of this host (`l`). |
| `3` | VLANs: the VLANs of the network (`v`), the own VLANs of this site (`o`) and the mappings (`n`, `Enter`, `d`) with their site and its role; conflicts are marked right away. `t` switches to the tunnels: what each carries, its limit (`Enter`, `d`). |
| `4` | MAC addresses: filter (`/`; `vlan:20` for one VLAN), sort (`s`, `o`). |
| `5` | Log: live events in local time (`t`: UTC), filter by level (`l`), site (`p`) and text (`/`). |
| `6` | Setup: new network or join; on a configured site invite (`i`) and reset (`R`). |
| `7` | Settings: every setting with an explanation, the trunk of every tunnel, and the role and rights of this site. `Enter` on a trunk lists the interfaces of the host to choose from (`Space`, `m`: main port). |

Changes are collected in a draft; the status line says how many it holds,
and new or changed rows are marked `+` or `*`. `a` shows the difference to
the running configuration and applies it; a banner counts down until it is
confirmed (`c`) or rolled back (`r`, asks first), also while a window is
open. `x` discards the draft, `q` quits; where a wrong "yes" would lose
something (reset, discard, roll back, quitting with a draft, closing a form
with input) Enter means no and only `y` is yes. Messages that do not fit get
a second line, `M` shows the last ones, and the status line of every view
names problems of this site (an error, blocked ports, refused changes).
When the daemon is not reachable, the TUI shows the state of the service and
its last log lines and keeps trying.

## Command line

| Command | Purpose |
|---------|---------|
| `wanderlan status [--json]` | This site, all known sites with role and link, the tunnel neighbours |
| `wanderlan log [-f]` | Event log |
| `wanderlan vlan <vid> [--json]` | Traffic of one VLAN on every port of this site now, its counters and MAC addresses |
| `wanderlan invite [--name N] [--vlans V] [--native VID] [--map MAPPED:SOURCE]... [--self-managed]` | Create an invitation with rules; prints `wanderlan join <address> <code>` |
| `wanderlan invite list` / `wanderlan invite delete <code>` | Open invitations of this site |
| `wanderlan join <address>[:port] <code> [--name N] [--trunk IF]` | Join a network (on an unconfigured site) |
| `wanderlan online <site>` / `wanderlan offline <site>` | Take a site online or offline |
| `wanderlan manage <site> [--by S \| --anyone] [--self-managed yes\|no]` | Who may change a site |
| `wanderlan role <site> server\|client <server>` | Make a site a server or a client |
| `wanderlan trunk <site> <interfaces\|default>` | Trunk ports of the tunnel to a site, or (this site's name) of the default trunk; several separated by commas |
| `wanderlan own-vlans <site> <vlans\|none>` | Own VLANs of a site (trunk and its own clients only) |
| `wanderlan tunnel-vlans <site> <site> <vlans\|all\|none>` | The most the tunnel between two sites carries |
| `wanderlan apply [--network F] [--local F] [--timeout 60s]` | Apply files (default: those in `/etc/wanderlan`) |
| `wanderlan confirm` / `wanderlan rollback` | Confirm or revert the pending change |
| `wanderlan validate <network.toml> [--local <local.toml>]` | Check files without applying them |
| `wanderlan init [--dir D]` / `wanderlan export` | Print or write the example configuration / print the running `network.toml` |
| `wanderlan probe` / `wanderlan unblock <port>` | Measure the path MTU now / re-enable a port the loop guard shut down |
| `wanderlan reset [--yes]` | Remove everything wanderlan set up on this site |

Commands that change the configuration (`online`, `offline`, `manage`,
`role`, `trunk`, `own-vlans`, `tunnel-vlans`) take `--confirm` to confirm right away; without it, run
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
* **"Your change was not taken over by …"** on a client, or `refused` in
  the CONFIG column of its server: the server could not take over the
  client's change; the reason follows (e.g. the name of a new client is
  already used in the network, where the client cannot see it). The client
  runs with the server's configuration again; correct the change and apply
  it once more. The message goes away with the next change confirmed on the
  client.

## Project decisions

* **No VPN inside wanderlan.** Transport and encryption belong to the existing
  VPN; wanderlan only does layer 2 and VLANs. Sites are identified by their
  transport address, and GRE runs unencrypted on top of that trusted network.
* **Servers and clients, like plugging switches together.** Roles are not
  configured up front: a site that joins is a client of the site that invited
  it and only has to reach it; servers are all connected with each other,
  with split horizon. There are no tunnels between clients; the VLANs decide
  who meets whom.
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

* as the version of the commit (`scripts/version.sh`): the version in
  `Cargo.toml` with its last number raised by the commits since that line
  last changed, so the commit that sets 1.0.0 is 1.0.0 and the next one
  1.0.1; the package version is the same, `--version`, the job names, the
  summaries and the release title add the commit;
* into the signed repository on the branch `gh-pages` (one commit without
  history), suite `stable`; the newest 20 versions per architecture stay,
  tagged releases forever;
* `verify-apt.yml` then installs it from GitHub Pages with the commands above
  on Debian 13, Debian 12 and Ubuntu 24.04, also exactly this version and a
  downgrade, and runs two hosts with an invitation.

A tag `v<version>` on a commit, with that commit's version (e.g. `v1.0.7`),
additionally makes `release.yml` create a GitHub release with binaries,
packages and `SHA256SUMS` and keeps the package `<version>` in the
repository forever. To start a new version line, set e.g. `1.1.0` in
`Cargo.toml`: that commit is 1.1.0, the next 1.1.1.

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
